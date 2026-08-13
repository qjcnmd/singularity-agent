//! `sg` 的命令行入口：通过 stdio JSON-RPC 调用 app-server 并渲染结果。

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command as ProcessCommand, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use clap::{Parser, Subcommand, ValueEnum};
use serde_json::{Value, json};
use singularity_core::ClientInfo;
use singularity_model::{import_env_to_user_config, read_user_model_catalog};
use singularity_protocol::{
    AgentCapabilityResult, EmptyParams, EventMetadata, InitializeParams, InputItem,
    ItemEventParams, JsonRpcId, JsonRpcMessage, JsonRpcNotification, Method,
    ProviderConfigurationStatus, RpcMethod, Thread, ThreadEventParams, ThreadIdParams,
    ThreadStartParams, Turn, TurnEventParams, TurnIdParams, TurnInputDelivery, TurnInputParams,
    TurnStartParams, TurnStatus, rpc_methods,
};

mod eval;

const APP_SERVER_BIN_ENV: &str = "SINGULARITY_APP_SERVER_BIN";
const APP_SERVER_DB_ENV: &str = "SINGULARITY_APP_SERVER_DB";
const DEFAULT_APP_SERVER_BIN: &str = "singularity_app_server";
const CLI_CLIENT_NAME: &str = "singularity_cli";
const CLI_CLIENT_TITLE: &str = "Singularity CLI";
const CLI_CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);
const AGENT_TURN_RESPONSE_TIMEOUT: Duration = Duration::from_secs(3600);
const SHUTDOWN_WAIT_TIMEOUT: Duration = Duration::from_secs(2);
const TURN_STATUS_POLL_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Debug, Parser)]
#[command(name = "sg")]
#[command(about = "Singularity coding agent")]
// 命令行顶层参数及其子命令入口。
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
// 面向终端用户的 CLI 命令集合。
enum Command {
    /// Start a thread, submit a goal, and render protocol events.
    Run {
        goal: String,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Resume an existing thread and submit a new user turn.
    Continue {
        thread_id: String,
        instruction: String,
    },
    /// Inspect or interrupt a turn through the app-server protocol.
    Turn {
        #[command(subcommand)]
        command: TurnCommand,
    },
    /// List persisted threads through the app-server protocol.
    Threads,
    /// Configuration and runtime diagnostics.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Run the fixed task set against the configured model list (lightweight regression eval).
    Eval {
        /// Path to eval-config.json (default: evaluations/eval-config.json).
        #[arg(long)]
        config: Option<PathBuf>,
        /// Comma-separated task id override.
        #[arg(long)]
        tasks: Option<String>,
        /// Comma-separated model selector override.
        #[arg(long)]
        models: Option<String>,
        /// Maximum parallel cells (default: 10).
        #[arg(long)]
        max_parallel: Option<usize>,
        /// Per-cell timeout in seconds (default: config or 1800).
        #[arg(long)]
        timeout_secs: Option<u64>,
    },
}

#[derive(Debug, Subcommand)]
// 配置与运行时诊断命令。
enum ConfigCommand {
    /// Print app-server client diagnostics.
    Doctor,
    /// List discovered model ids and explicit selectable overrides.
    Models {
        #[arg(long)]
        refresh: bool,
    },
    /// Import a dotenv file into user-level config and auth files.
    ImportEnv {
        #[arg(long)]
        file: Option<PathBuf>,
    },
}

#[derive(Debug, Subcommand)]
// turn 查询、中断与同一 Turn 操作命令。
enum TurnCommand {
    /// Print the current turn status.
    Status { turn_id: String },
    /// Interrupt a running turn.
    Interrupt { turn_id: String },
    /// Resume a suspended or paused turn from its durable checkpoint.
    Resume { turn_id: String },
    /// Pause a running turn without terminating its checkpoint.
    Pause { turn_id: String },
    /// Append a real user input to a non-terminal turn.
    Input {
        turn_id: String,
        text: String,
        /// Explicit idempotency key for the appended input.
        #[arg(long)]
        input_id: String,
        /// Delivery semantics: steer consumes at the next boundary, follow-up waits for the turn end.
        #[arg(long, value_enum)]
        delivery: Option<TurnDeliveryArg>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
// turn/input 的受控投递枚举表示。
enum TurnDeliveryArg {
    Steer,
    FollowUp,
}

impl TurnDeliveryArg {
    fn protocol_value(self) -> TurnInputDelivery {
        match self {
            Self::Steer => TurnInputDelivery::Steer,
            Self::FollowUp => TurnInputDelivery::FollowUp,
        }
    }
}

// 解析命令、驱动 app-server 客户端，并将错误转换为进程失败。
fn main() {
    if let Err(error) = run_cli(Cli::parse()) {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

// 按命令编排 app-server 请求和面向用户的输出。
fn run_cli(cli: Cli) -> Result<(), String> {
    match cli.command {
        Command::Run {
            goal,
            model,
            json,
        } => {
            let mut client = AppServerClient::spawn()?;
            client.response_timeout = AGENT_TURN_RESPONSE_TIMEOUT;
            client.initialize()?;
            ensure_agent_loop_available(&mut client)?;
            let (thread, thread_events) = client.thread_start(model, !json)?;
            if !json {
                println!("thread {}", thread.thread_id);
            }
            let (turn, turn_events) = client.turn_start(&thread.thread_id, &goal, !json)?;
            if json {
                let mut events = protocol_events(thread_events);
                events.extend(protocol_events(turn_events));
                println!(
                    "{}",
                    json!({
                        "thread": thread,
                        "turn": turn,
                        "events": events,
                    })
                );
            }
            fail_for_failed_turn(&turn)?;
            Ok(())
        }
        Command::Continue {
            thread_id,
            instruction,
        } => {
            let mut client = AppServerClient::spawn()?;
            client.response_timeout = AGENT_TURN_RESPONSE_TIMEOUT;
            client.initialize()?;
            ensure_agent_loop_available(&mut client)?;
            let _thread = client.thread_resume(&thread_id)?;
            println!("thread {thread_id}");
            let (turn, _events) = client.turn_start(&thread_id, &instruction, true)?;
            fail_for_failed_turn(&turn)?;
            Ok(())
        }
        Command::Turn { command } => {
            let mut client = AppServerClient::spawn()?;
            client.initialize()?;
            match command {
                TurnCommand::Status { turn_id } => client.turn_status(&turn_id),
                TurnCommand::Interrupt { turn_id } => client.turn_interrupt(&turn_id),
                TurnCommand::Resume { turn_id } => {
                    client.response_timeout = AGENT_TURN_RESPONSE_TIMEOUT;
                    let turn = client.turn_resume(&turn_id, true)?;
                    fail_for_failed_turn(&turn)?;
                    Ok(())
                }
                TurnCommand::Pause { turn_id } => {
                    let turn = client.turn_pause(&turn_id)?;
                    render_turn(&turn);
                    Ok(())
                }
                TurnCommand::Input {
                    turn_id,
                    text,
                    input_id,
                    delivery,
                } => {
                    client.response_timeout = AGENT_TURN_RESPONSE_TIMEOUT;
                    let delivery = delivery.map_or(TurnInputDelivery::FollowUp, |delivery| {
                        delivery.protocol_value()
                    });
                    let turn = client.turn_input(&turn_id, &text, &input_id, delivery, true)?;
                    fail_for_failed_turn(&turn)?;
                    Ok(())
                }
            }
        }
        Command::Threads => {
            let mut client = AppServerClient::spawn()?;
            client.initialize()?;
            client.thread_list()?;
            Ok(())
        }
        Command::Config { command } => match command {
            ConfigCommand::Doctor => {
                println!("app_server_bin={}", app_server_bin()?);
                println!("app_server_db={}", app_server_db_display());
                println!("client=protocol-only");
                print_readiness()?;
                Ok(())
            }
            ConfigCommand::Models { refresh } => {
                let catalog = read_user_model_catalog(refresh)
                    .map_err(|error| format!("failed to read provider models: {error}"))?;
                println!(
                    "default_selector={}",
                    catalog.default_selector.as_deref().unwrap_or("none")
                );
                println!("cache_status={:?}", catalog.cache_status);
                if catalog.providers.is_empty() {
                    println!("providers=none");
                }
                for provider in catalog.providers {
                    println!(
                        "provider={} discovery={:?} api_key={} base_url={}",
                        provider.provider_name,
                        provider.discovery,
                        if provider.api_key_present {
                            "present(redacted)"
                        } else {
                            "missing"
                        },
                        if provider.base_url_present {
                            "present(redacted)"
                        } else {
                            "missing"
                        }
                    );
                    for model in provider.models {
                        println!(
                            "model={} discovered={} explicit={} selectable={} variants={} default_variant={}",
                            model.id,
                            model.discovered,
                            model.explicit,
                            model.selectable,
                            if model.reasoning_variants.is_empty() {
                                "none".to_string()
                            } else {
                                model.reasoning_variants.join(",")
                            },
                            model.default_variant.as_deref().unwrap_or("none")
                        );
                    }
                    if let Some(error) = provider.error {
                        println!("provider_error={error}");
                    }
                }
                Ok(())
            }
            ConfigCommand::ImportEnv { file } => {
                let result = import_env_to_user_config(file.as_deref())
                    .map_err(|error| format!("failed to import provider env: {error}"))?;
                println!("config_path={}", result.config_path);
                println!("auth_path={}", result.auth_path);
                println!("provider={}", result.provider_name);
                println!(
                    "default_selector={}",
                    result.default_selector.as_deref().unwrap_or("none")
                );
                println!("selectable={}", result.selectable);
                Ok(())
            }
        },
        Command::Eval {
            config,
            tasks,
            models,
            max_parallel,
            timeout_secs,
        } => eval::run_eval(
            config,
            tasks.as_deref(),
            models.as_deref(),
            max_parallel,
            timeout_secs,
        ),
    }
}
fn ensure_agent_loop_available(client: &mut AppServerClient) -> Result<(), String> {
    let capability = client.agent_capability()?;
    let blockers = agent_loop_blockers(&capability.agent_loop.blockers);
    if capability.agent_loop.available
        && blockers == "none"
        && capability.agent_loop.status == "completed"
    {
        return Ok(());
    }
    let status = &capability.agent_loop.status;
    Err(format!(
        "AgentLoop is not available: status={status}; blockers={blockers}"
    ))
}

// 将 capability 中的 blocker 列表压缩为稳定的诊断文本。
fn agent_loop_blockers(blockers: &[String]) -> String {
    let blockers = blockers.join(",");
    if blockers.is_empty() {
        "none".to_string()
    } else {
        blockers
    }
}

// 输出 app-server、AgentLoop 与 provider 的脱敏就绪状态。
fn print_readiness() -> Result<(), String> {
    let mut client = AppServerClient::spawn()?;
    client.response_timeout = AGENT_TURN_RESPONSE_TIMEOUT;
    client.initialize()?;
    let capability = client.agent_capability()?;
    println!("agent_loop={}", capability.agent_loop.status);
    print_provider_configuration(&capability.provider_configuration)
}

// 校验并输出 provider capability，始终只暴露字段存在性。
fn print_provider_configuration(provider: &ProviderConfigurationStatus) -> Result<(), String> {
    let source = match provider.source.as_deref() {
        Some("process_env") => "process_env",
        Some("user_config") => "user_config",
        None => "unconfigured",
        _ => {
            return Err("invalid agent capability: providerConfiguration.source".to_string());
        }
    };
    println!("provider_config_source={source}");
    let snapshot_id = (!provider.snapshot_id.trim().is_empty())
        .then_some(provider.snapshot_id.as_str())
        .ok_or_else(|| "invalid agent capability: providerConfiguration.snapshotId".to_string())?;
    println!("provider_snapshot_id={snapshot_id}");
    println!("provider_configured={}", provider.configured);
    let blocker = match provider.configuration_blocker.as_deref() {
        None => "none",
        Some(blocker) if !blocker.trim().is_empty() => blocker,
        _ => {
            return Err(
                "invalid agent capability: providerConfiguration.configurationBlocker".to_string(),
            );
        }
    };
    println!("provider_configuration_blocker={blocker}");
    for (name, present) in [
        ("SINGULARITY_API_KEY", provider.api_key_present),
        ("SINGULARITY_BASE_URL", provider.base_url_present),
        ("SINGULARITY_MODEL", provider.model_present),
    ] {
        let status = if present {
            "present(redacted)"
        } else {
            "missing"
        };
        println!("{name}={status}");
    }
    Ok(())
}

// 维护 app-server 子进程及其 JSON-RPC stdio 通道。
struct AppServerClient {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: Receiver<Result<String, String>>,
    stdout_reader: Option<JoinHandle<()>>,
    response_timeout: Duration,
    next_id: i64,
}

/// 与 typed request id 匹配的结果及其之前到达的通知。
struct RpcReply<R> {
    result: R,
    notifications: Vec<JsonRpcNotification>,
}

// AppServerClient 的生命周期与 JSON-RPC 操作实现。
impl AppServerClient {
    // 定位并启动 app-server，随后建立异步 stdout reader。
    fn spawn() -> Result<Self, String> {
        let mut command = ProcessCommand::new(app_server_bin()?);
        command.stdin(Stdio::piped()).stdout(Stdio::piped());
        if let Ok(db) = std::env::var(APP_SERVER_DB_ENV) {
            command.env(APP_SERVER_DB_ENV, db);
        }
        let mut child = command
            .spawn()
            .map_err(|error| format!("failed to start app-server: {error}"))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "app-server stdin unavailable".to_string())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "app-server stdout unavailable".to_string())?;
        let (stdout, stdout_reader) = spawn_stdout_reader(stdout);
        Ok(Self {
            child,
            stdin: Some(stdin),
            stdout,
            stdout_reader: Some(stdout_reader),
            response_timeout: RESPONSE_TIMEOUT,
            next_id: 1,
        })
    }

    // 完成 JSON-RPC initialize/initialized 握手；事件全量接收，无需订阅。
    fn initialize(&mut self) -> Result<(), String> {
        let _ = self.request::<rpc_methods::Initialize>(&InitializeParams {
            client_info: ClientInfo::new(CLI_CLIENT_NAME, CLI_CLIENT_TITLE, CLI_CLIENT_VERSION),
            capabilities: None,
        })?;
        self.notify::<rpc_methods::Initialized>(&EmptyParams::default())?;
        Ok(())
    }

    // 创建 thread，并可选地渲染启动事件。
    fn thread_start(
        &mut self,
        model: Option<String>,
        render: bool,
    ) -> Result<(Thread, Vec<JsonRpcNotification>), String> {
        let reply = self.request::<rpc_methods::ThreadStart>(&ThreadStartParams {
            model,
            cwd: Some(canonical_current_dir()?),
        })?;
        if render {
            render_messages(&reply.notifications, false);
        }
        Ok((reply.result.thread, reply.notifications))
    }

    // 恢复现有 thread，不向 app-server 上传历史。
    fn thread_resume(&mut self, thread_id: &str) -> Result<Thread, String> {
        let reply = self.request::<rpc_methods::ThreadResume>(&ThreadIdParams {
            thread_id: thread_id.to_string(),
        })?;
        Ok(reply.result.thread)
    }

    // 读取 AgentLoop capability 快照。
    fn agent_capability(&mut self) -> Result<AgentCapabilityResult, String> {
        let reply = self.request::<rpc_methods::AgentCapability>(&EmptyParams::default())?;
        Ok(reply.result)
    }

    // 启动 turn、渲染事件，并在必要时轮询到终态。
    fn turn_start(
        &mut self,
        thread_id: &str,
        text: &str,
        render: bool,
    ) -> Result<(Turn, Vec<JsonRpcNotification>), String> {
        let reply = self.request::<rpc_methods::TurnStart>(&TurnStartParams {
            thread_id: thread_id.to_string(),
            input: vec![InputItem::Text {
                text: text.to_string(),
            }],
        })?;
        let turn = render_and_wait_terminal(
            self,
            reply.result.turn.clone(),
            reply.notifications.clone(),
            render,
        )?;
        Ok((turn, reply.notifications))
    }

    // 恢复暂停/挂起的 turn，从持久化 checkpoint 继续执行并渲染事件。
    fn turn_resume(&mut self, turn_id: &str, render: bool) -> Result<Turn, String> {
        let reply = self.request::<rpc_methods::TurnResume>(&TurnIdParams {
            turn_id: turn_id.to_string(),
        })?;
        render_and_wait_terminal(self, reply.result.turn, reply.notifications, render)
    }

    // 暂停 running turn；短请求，不自动轮询。
    fn turn_pause(&mut self, turn_id: &str) -> Result<Turn, String> {
        let reply = self.request::<rpc_methods::TurnPause>(&TurnIdParams {
            turn_id: turn_id.to_string(),
        })?;
        Ok(reply.result.turn)
    }

    // 向非终态 turn 追加真实用户输入，并按 turn_start 相同规则渲染/轮询。
    fn turn_input(
        &mut self,
        turn_id: &str,
        text: &str,
        input_id: &str,
        delivery: TurnInputDelivery,
        render: bool,
    ) -> Result<Turn, String> {
        let reply = self.request::<rpc_methods::TurnInput>(&TurnInputParams {
            turn_id: turn_id.to_string(),
            input_id: input_id.to_string(),
            delivery,
            input: vec![InputItem::Text {
                text: text.to_string(),
            }],
        })?;
        render_and_wait_terminal(self, reply.result.turn, reply.notifications, render)
    }
}

// 渲染 turn 通知、打印状态行，并在 running 时轮询到终态。
// turn_start / turn_resume / turn_input 共享同一渲染与轮询规则。
fn render_and_wait_terminal(
    client: &mut AppServerClient,
    mut turn: Turn,
    notifications: Vec<JsonRpcNotification>,
    render: bool,
) -> Result<Turn, String> {
    if render {
        render_messages(&notifications, should_render_assistant_summary(&turn));
        render_turn(&turn);
    }
    if should_poll_running_turn(&turn) {
        turn = wait_for_turn_terminal(client, &turn.turn_id, render)?;
    }
    Ok(turn)
}

// 按固定间隔查询 running turn，直到出现终态。
fn wait_for_turn_terminal(
    client: &mut AppServerClient,
    turn_id: &str,
    render: bool,
) -> Result<Turn, String> {
    loop {
        thread::sleep(TURN_STATUS_POLL_INTERVAL);
        let turn = client.fetch_turn_status(turn_id)?;
        if turn.status != TurnStatus::Running {
            if render {
                println!(
                    "turn {} {} agent_loop_status={}",
                    turn.turn_id,
                    turn.status.as_storage_text(),
                    turn.agent_loop_status,
                );
            }
            return Ok(turn);
        }
    }
}

impl AppServerClient {
    // 将 turn/status 响应投影为 CLI 所需的最小视图。
    fn fetch_turn_status(&mut self, turn_id: &str) -> Result<Turn, String> {
        let reply = self.request::<rpc_methods::TurnStatus>(&TurnIdParams {
            turn_id: turn_id.to_string(),
        })?;
        Ok(reply.result.turn)
    }

    // 请求并打印持久化 thread 列表。
    fn thread_list(&mut self) -> Result<(), String> {
        let reply = self.request::<rpc_methods::ThreadList>(&EmptyParams::default())?;
        for thread in &reply.result.threads {
            println!("{} {}", thread.thread_id, thread.status.as_storage_text());
        }
        Ok(())
    }

    // 请求并渲染单个 turn 的状态。
    fn turn_status(&mut self, turn_id: &str) -> Result<(), String> {
        let reply = self.request::<rpc_methods::TurnStatus>(&TurnIdParams {
            turn_id: turn_id.to_string(),
        })?;
        render_turn(&reply.result.turn);
        Ok(())
    }

    // 请求中断 turn，并打印服务端返回的状态。
    fn turn_interrupt(&mut self, turn_id: &str) -> Result<(), String> {
        let reply = self.request::<rpc_methods::TurnInterrupt>(&TurnIdParams {
            turn_id: turn_id.to_string(),
        })?;
        println!(
            "turn {} {}{}",
            reply.result.turn_id,
            reply.result.status,
            agent_loop_status_suffix(reply.result.agent_loop_status.as_deref())
        );
        Ok(())
    }

    // 发送请求并只接收匹配 id 的响应，同时保留通知事件。
    fn request<M: RpcMethod>(&mut self, params: &M::Params) -> Result<RpcReply<M::Result>, String> {
        let method = M::METHOD;
        let params_value = serde_json::to_value(params)
            .map_err(|error| format!("failed to serialize {} params: {error}", method.as_str()))?;
        method
            .spec()
            .validate_params(params_value)
            .map_err(|error| format!("invalid {} params: {error}", method.as_str()))?;
        let id = JsonRpcId::Number(self.next_request_id());
        let message = JsonRpcMessage::request(method, id.clone(), params)
            .map_err(|error| format!("failed to serialize app-server request: {error}"))?;
        self.write_message(&message)?;
        let mut notifications = Vec::new();
        loop {
            match self.read_message(self.response_timeout)? {
                JsonRpcMessage::Notification(notification) => {
                    notifications.push(notification);
                }
                JsonRpcMessage::Success(response) if response.id == id => {
                    let result = serde_json::from_value(response.result)
                        .map_err(|error| format!("invalid {} result: {error}", method.as_str()))?;
                    return Ok(RpcReply {
                        result,
                        notifications,
                    });
                }
                JsonRpcMessage::Error(response) if response.id == id => {
                    return Err(format!("app-server error: {}", response.error.message));
                }
                JsonRpcMessage::Success(_) | JsonRpcMessage::Error(_) => {}
                JsonRpcMessage::Request(_) => {
                    return Err("app-server emitted a request on the response channel".to_string());
                }
            }
        }
    }

    // 向 app-server 发送 JSON-RPC notification。
    fn notify<M: RpcMethod>(&mut self, params: &M::Params) -> Result<(), String> {
        let method = M::METHOD;
        let message = JsonRpcMessage::notification(method.as_str(), params)
            .map_err(|error| format!("failed to serialize app-server notification: {error}"))?;
        self.write_message(&message)
    }

    // 序列化、写入并 flush 一条 JSON-RPC 消息。
    fn write_message(&mut self, message: &JsonRpcMessage) -> Result<(), String> {
        let write_result = {
            let stdin = self
                .stdin
                .as_mut()
                .ok_or_else(|| "app-server stdin unavailable".to_string())?;
            writeln!(stdin, "{}", message.to_wire_value())
        };
        if let Err(error) = write_result {
            return Err(
                self.classify_transport_write_error("failed to write app-server request", error)
            );
        }

        let flush_result = {
            let stdin = self
                .stdin
                .as_mut()
                .ok_or_else(|| "app-server stdin unavailable".to_string())?;
            stdin.flush()
        };
        if let Err(error) = flush_result {
            return Err(
                self.classify_transport_write_error("failed to flush app-server request", error)
            );
        }
        Ok(())
    }

    // 写端失败时先确认 app-server 是否已经退出，避免以竞争性的 Broken pipe
    // 覆盖“响应前退出”的稳定传输错误；进程仍存活时保留真实 I/O 原因。
    fn classify_transport_write_error(&mut self, operation: &str, error: std::io::Error) -> String {
        match self.child.try_wait() {
            Ok(Some(status)) => format!("app-server exited before response: {status}"),
            Ok(None) => format!("{operation}: {error}"),
            Err(status_error) => format!(
                "{operation}: {error}; failed to poll app-server process status: {status_error}"
            ),
        }
    }

    // 从 stdout reader 读取一条消息，并区分超时、断开与非法 JSON。
    fn read_message(&mut self, timeout: Duration) -> Result<JsonRpcMessage, String> {
        let line =
            match self.stdout.recv_timeout(timeout) {
                Ok(line) => line?,
                Err(RecvTimeoutError::Timeout) => {
                    if let Some(status) = self.child.try_wait().map_err(|error| {
                        format!("failed to poll app-server process status: {error}")
                    })? {
                        return Err(format!("app-server exited before response: {status}"));
                    }
                    return Err("timed out waiting for app-server response".to_string());
                }
                Err(RecvTimeoutError::Disconnected) => {
                    if let Some(status) = self.child.try_wait().map_err(|error| {
                        format!("failed to poll app-server process status: {error}")
                    })? {
                        return Err(format!("app-server exited before response: {status}"));
                    }
                    return Err("app-server closed stdout".to_string());
                }
            };
        serde_json::from_str(line.trim())
            .map_err(|error| format!("invalid app-server json: {error}"))
    }

    // 分配本地递增的 JSON-RPC request id。
    fn next_request_id(&mut self) -> i64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }
}

// 负责 graceful shutdown 与子进程资源回收。
impl Drop for AppServerClient {
    // 先请求服务端 shutdown，再在超时后回收子进程与 reader。
    fn drop(&mut self) {
        if self.stdin.is_some() {
            let id = self.next_request_id();
            if let Ok(message) =
                JsonRpcMessage::request(Method::ServerShutdown, id, EmptyParams::default())
            {
                let _ = self.write_message(&message);
            }
        }
        let _ = self.stdin.take();
        let deadline = Instant::now() + SHUTDOWN_WAIT_TIMEOUT;
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(10));
                }
                Ok(None) | Err(_) => {
                    let _ = self.child.kill();
                    break;
                }
            }
        }
        let _ = self.child.wait();
        if let Some(stdout_reader) = self.stdout_reader.take() {
            let _ = stdout_reader.join();
        }
    }
}

// 将 app-server stdout 按行转发到客户端接收队列。
fn spawn_stdout_reader(stdout: ChildStdout) -> (Receiver<Result<String, String>>, JoinHandle<()>) {
    let (sender, receiver) = mpsc::channel();
    let handle = thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {
                    if sender.send(Ok(line)).is_err() {
                        break;
                    }
                }
                Err(error) => {
                    let _ =
                        sender.send(Err(format!("failed to read app-server response: {error}")));
                    break;
                }
            }
        }
    });
    (receiver, handle)
}

// 获取并规范化当前工作目录，作为 thread/start 的 cwd。
fn canonical_current_dir() -> Result<String, String> {
    let cwd = std::env::current_dir()
        .map_err(|error| format!("failed to read current directory: {error}"))?
        .canonicalize()
        .map_err(|error| format!("failed to canonicalize current directory: {error}"))?;
    cwd.to_str()
        .map(str::to_string)
        .ok_or_else(|| "current directory is not valid UTF-8".to_string())
}

// 过滤并脱敏可公开渲染的协议事件。
fn protocol_events(messages: Vec<JsonRpcNotification>) -> Vec<Value> {
    messages
        .into_iter()
        .filter_map(safe_protocol_event)
        .collect()
}

// 将单条协议通知投影为不泄露 raw payload 的事件。
fn safe_protocol_event(message: JsonRpcNotification) -> Option<Value> {
    let method = message.method;
    let params = serde_json::from_value::<ItemEventParams>(message.params.clone()).ok();
    let item_id = params
        .as_ref()
        .map(|params| params.item.item_id.as_str())
        .unwrap_or("");
    let mut output = match method.as_str() {
        "item/agentMessage/delta" => Some(json!({
            "method": method,
            "params": {
                "item_id": item_id,
                "delta": params.and_then(|params| params.delta).unwrap_or_default(),
            },
        })),
        "item/started" | "item/completed" => Some(json!({
            "method": method,
            "params": {"item_id": item_id},
        })),
        _ => Some(json!({"method": method})),
    }?;
    if let Some(event) = message
        .params
        .get("event")
        .and_then(|value| serde_json::from_value::<EventMetadata>(value.clone()).ok())
        .and_then(|metadata| serde_json::to_value(metadata).ok())
    {
        output["event"] = event;
    }
    Some(output)
}

// 按协议 method 渲染 thread、turn 与 item 事件。
fn render_messages(messages: &[JsonRpcNotification], render_assistant_summary: bool) {
    for message in messages {
        let method = message.method.as_str();
        match method {
            "thread/started" => {
                if let Ok(params) =
                    serde_json::from_value::<ThreadEventParams>(message.params.clone())
                {
                    println!("thread/started {}", params.thread.thread_id);
                }
            }
            "turn/started" => {
                if let Ok(params) =
                    serde_json::from_value::<TurnEventParams>(message.params.clone())
                {
                    println!("turn/started {}", params.turn.turn_id);
                    render_turn(&params.turn);
                }
            }
            "item/started" | "item/completed" => {
                if let Ok(params) =
                    serde_json::from_value::<ItemEventParams>(message.params.clone())
                {
                    println!("{method} {}", params.item.item_id);
                }
            }
            "item/agentMessage/delta" => {
                if let Ok(params) =
                    serde_json::from_value::<ItemEventParams>(message.params.clone())
                {
                    let text = params.delta.unwrap_or_default();
                    println!("{method} {text}");
                    if render_assistant_summary {
                        println!("assistant {text}");
                    }
                }
            }
            _ => println!("{method}"),
        }
    }
}

// 判断是否应额外输出已完成的 assistant 摘要。
fn should_render_assistant_summary(turn: &Turn) -> bool {
    turn.status == TurnStatus::Completed && turn.agent_loop_status == "completed"
}

// 判断 running turn 是否仍可通过轮询等待终态。
fn should_poll_running_turn(turn: &Turn) -> bool {
    turn.status == TurnStatus::Running
        && matches!(
            turn.agent_loop_status.as_str(),
            "running" | "cancel_requested"
        )
}

// 渲染 turn 的稳定状态行。
fn render_turn(turn: &Turn) {
    if turn.turn_id.is_empty() {
        return;
    }
    println!(
        "turn {} {} agent_loop_status={}",
        turn.turn_id,
        turn.status.as_storage_text(),
        turn.agent_loop_status
    );
}

// 从响应对象提取可选的 AgentLoop 状态后缀。
fn agent_loop_status_suffix(status: Option<&str>) -> String {
    status
        .map(|status| format!(" agent_loop_status={status}"))
        .unwrap_or_default()
}

// 将失败、blocked 或未能安全轮询的 turn 映射为 CLI 错误。
fn fail_for_failed_turn(turn: &Turn) -> Result<(), String> {
    let status = turn.status.as_storage_text();
    let agent_loop_status = turn.agent_loop_status.as_str();
    let non_terminal_running =
        turn.status == TurnStatus::Running && !should_poll_running_turn(turn);
    if non_terminal_running
        || matches!(
            turn.status,
            TurnStatus::Failed | TurnStatus::Blocked | TurnStatus::Interrupted
        )
        || matches!(agent_loop_status, "failed" | "blocked" | "cancelled")
    {
        if turn.turn_id.is_empty() {
            return Err(format!("error {status}: turn {status}"));
        }
        return Err(format!(
            "error {status}: turn {status}; turn {} {status}",
            turn.turn_id
        ));
    }
    Ok(())
}

// 解析显式 app-server 路径或相邻的默认二进制。
fn app_server_bin() -> Result<String, String> {
    if let Ok(path) = std::env::var(APP_SERVER_BIN_ENV) {
        return Ok(path);
    }
    sibling_app_server_bin()
        .map(|path| path.to_string_lossy().to_string())
        .ok_or_else(|| {
            format!(
                "{DEFAULT_APP_SERVER_BIN} not found beside sg; set {APP_SERVER_BIN_ENV} to an explicit app-server binary"
            )
        })
}

// 返回诊断输出使用的 app-server 数据库路径。
fn app_server_db_display() -> String {
    std::env::var(APP_SERVER_DB_ENV)
        .unwrap_or_else(|_| ".singularity/rust-app-server.sqlite3".to_string())
}

// 查找与当前 CLI 可执行文件同目录的 app-server。
fn sibling_app_server_bin() -> Option<PathBuf> {
    let mut path = std::env::current_exe().ok()?;
    path.pop();
    path.push(format!(
        "{DEFAULT_APP_SERVER_BIN}{}",
        std::env::consts::EXE_SUFFIX
    ));
    path.is_file().then_some(path)
}
