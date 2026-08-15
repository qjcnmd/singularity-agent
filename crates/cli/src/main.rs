//! `sg` 的命令行入口：通过 stdio JSON-RPC 调用 app-server 并渲染结果。

use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, IsTerminal, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command as ProcessCommand, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use clap::{Parser, Subcommand, ValueEnum};
use serde_json::{Value, json};
use singularity_core::{APP_ERROR_TRUST_REQUIRED, ClientInfo, user_singularity_home};
use singularity_model::{import_env_to_user_config, read_user_model_catalog};
use singularity_protocol::{
    AgentCapabilityResult, EmptyParams, EventMetadata, InitializeParams, InputItem,
    ItemEventParams, JsonRpcId, JsonRpcMessage, JsonRpcNotification, Method, ProjectTrustDecision,
    ProjectTrustParams, ProjectTrustResult, ProviderConfigurationStatus, RpcMethod, Thread,
    ThreadEventParams, ThreadIdParams, ThreadStartParams, Turn, TurnEventParams, TurnIdParams,
    TurnInputDelivery, TurnInputParams, TurnStartParams, TurnStatus, rpc_methods,
};

mod eval;

const APP_SERVER_BIN_ENV: &str = "SINGULARITY_APP_SERVER_BIN";
const APP_SERVER_DB_ENV: &str = "SINGULARITY_APP_SERVER_DB";
const APP_SERVER_TRANSPORT_ENV: &str = "SINGULARITY_APP_SERVER_TRANSPORT";
const APP_SERVER_LISTEN_ENV: &str = "SINGULARITY_APP_SERVER_LISTEN";
const LISTEN_ANNOUNCE_PREFIX: &str = "SINGULARITY_APP_SERVER_LISTENING ";
/// CLI 在自身 stdin 为 TTY 时传给 app-server 的交互 UI 标记。
const INTERACTIVE_UI_ENV: &str = "SINGULARITY_INTERACTIVE";
const DEFAULT_APP_SERVER_BIN: &str = "singularity_app_server";
const CLI_CLIENT_NAME: &str = "singularity_cli";
const CLI_CLIENT_TITLE: &str = "Singularity CLI";
const CLI_CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);
const AGENT_TURN_RESPONSE_TIMEOUT: Duration = Duration::from_secs(3600);
const SHUTDOWN_WAIT_TIMEOUT: Duration = Duration::from_secs(2);
/// 等待 app-server 在 stderr 上公布 TCP 回环地址的时间上限（对齐 daemon 化规划 START_TIMEOUT）。
const APP_SERVER_START_TIMEOUT: Duration = Duration::from_secs(10);
/// TCP daemon 复用判定里"能否连接/握手"的单次短超时（覆盖 dead/stale daemon 的快速回退）。
const DAEMON_REUSE_CONNECT_TIMEOUT: Duration = Duration::from_millis(200);
/// 另一 CLI 正在启动时，轮询其发布可复用 daemon 的总体上限（对齐规划 START_TIMEOUT）。
const DAEMON_START_WAIT_TIMEOUT: Duration = Duration::from_secs(10);
const DAEMON_START_POLL_INTERVAL: Duration = Duration::from_millis(50);
/// daemon 状态文件与启动锁文件名（放在 `user_singularity_home()` 目录下）。
const DAEMON_STATE_FILE_NAME: &str = "app-server-daemon.json";
const DAEMON_LOCK_FILE_NAME: &str = "app-server-daemon.lock";
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
    /// Query or manage the project trust decision for a folder.
    Trust {
        /// Project folder path (default: current directory).
        path: Option<PathBuf>,
        /// Decision to store: trust, never, or ask (clear the record).
        #[arg(short, long, value_enum)]
        decision: Option<TrustArg>,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
// sg trust 的受控决策值。
enum TrustArg {
    Trust,
    Never,
    Ask,
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
        Command::Run { goal, model, json } => {
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
        Command::Trust { path, decision } => {
            let mut client = AppServerClient::spawn()?;
            client.initialize()?;
            let cwd = match path {
                Some(path) => canonical_directory_arg(&path)?,
                None => canonical_current_dir()?,
            };
            let decision = match decision {
                None => ProjectTrustDecision::Query,
                Some(TrustArg::Trust) => ProjectTrustDecision::Set(true),
                Some(TrustArg::Never) => ProjectTrustDecision::Set(false),
                Some(TrustArg::Ask) => ProjectTrustDecision::Ask,
            };
            let result = client.project_trust(&cwd, decision)?;
            let stored = match result.decision {
                Some(true) => "trusted",
                Some(false) => "never",
                None => "ask",
            };
            println!("project trust: {} => {stored}", result.path);
            Ok(())
        }
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

// 维护 app-server（常驻 daemon 或一次性子进程）及其 JSON-RPC 通道；承载可为
// stdio 或 TCP 回环。TCP daemon 模式下 `child` 在复用既有 daemon 时为 `None`，
// 在本次命令负责拉起新 daemon 时为 `Some(child)`（Drop 时释放连接即可，daemon
// 空闲自停接管退出，CLI 不与 daemon 同生共死）。
struct AppServerClient {
    child: Option<Child>,
    transport: AppServerTransport,
    response_timeout: Duration,
    next_id: i64,
}

/// app-server 传输承载。JSON 行读写一致，仅 I/O 来源不同：stdio 用子进程管道，
/// TCP 用回环 `TcpStream`（入站同样由读取线程转成接收队列，与 stdio 对齐）。
enum AppServerTransport {
    Stdio {
        stdin: Option<ChildStdin>,
        stdout: Receiver<Result<String, String>>,
        stdout_reader: Option<JoinHandle<()>>,
    },
    Tcp {
        /// 写入 JSON 行并 flush 的连接；Drop 时关闭以通知服务端断开（daemon 空闲自停）。
        stream: Option<TcpStream>,
        inbound: Receiver<Result<String, String>>,
        inbound_reader: Option<JoinHandle<()>>,
        /// 保留 stderr 读取队列的接收端，使读取线程持续排空管道，避免子进程写阻塞。
        /// 仅在本命令拉起的新 daemon 上存在；复用既有 daemon 时为 `None`。
        _stderr_rx: Option<Receiver<Result<String, String>>>,
        /// stderr 排空线程；Drop 时 detach（daemon 常驻期间 stderr 不会立即关闭，
        /// join 会阻塞 CLI 直到 daemon 退出，因此不 join）。
        stderr_reader: Option<JoinHandle<()>>,
    },
}

/// 传输选择：`SINGULARITY_APP_SERVER_TRANSPORT` 缺省时使用 TCP（daemon 化默认），
/// `stdio` 显式保留为可回退后备。
enum TransportMode {
    Tcp,
    Stdio,
}

/// 与 typed request id 匹配的结果及其之前到达的通知。
struct RpcReply<R> {
    result: R,
    notifications: Vec<JsonRpcNotification>,
}

/// app-server JSON-RPC 错误响应（保留 code/data，供 -32010 trust_required 流程使用）。
#[derive(Debug, Clone)]
struct AppServerRpcError {
    code: i64,
    message: String,
    data: Option<Value>,
}

impl fmt::Display for AppServerRpcError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "app-server error: {}", self.message)
    }
}

impl std::error::Error for AppServerRpcError {}

impl From<String> for AppServerRpcError {
    fn from(message: String) -> Self {
        Self {
            code: 0,
            message,
            data: None,
        }
    }
}

impl From<AppServerRpcError> for String {
    fn from(error: AppServerRpcError) -> Self {
        error.to_string()
    }
}

// AppServerClient 的生命周期与 JSON-RPC 操作实现。
impl AppServerClient {
    // 确保一个可用的 app-server 并建立连接：TCP 模式下优先复用既有 daemon（见
    // ensure_tcp），否则拉起新 daemon；stdio 模式始终拉一次性子进程。
    fn spawn() -> Result<Self, String> {
        match transport_mode()? {
            TransportMode::Tcp => Self::ensure_tcp(),
            TransportMode::Stdio => Self::spawn_stdio(),
        }
    }

    // 通过 stdio 管道承载：spawn 子进程并建立异步 stdout reader（原有路径）。
    fn spawn_stdio() -> Result<Self, String> {
        let mut command = ProcessCommand::new(app_server_bin()?);
        command.stdin(Stdio::piped()).stdout(Stdio::piped());
        // 自身 stdin 为 TTY 时向 app-server 传递交互 UI 可用性（非交互如 eval 子进程不设置）。
        if std::io::stdin().is_terminal() {
            command.env(INTERACTIVE_UI_ENV, "1");
        }
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
        let (stdout, stdout_reader) = spawn_line_reader(stdout);
        Ok(Self {
            child: Some(child),
            transport: AppServerTransport::Stdio {
                stdin: Some(stdin),
                stdout,
                stdout_reader: Some(stdout_reader),
            },
            response_timeout: RESPONSE_TIMEOUT,
            next_id: 1,
        })
    }

    // 通过 TCP 回环承载：spawn 子进程（SINGULARITY_APP_SERVER_LISTEN=tcp://127.0.0.1:0），
    // 从 stderr 读取公布的实际端口并连接，入站用同一读取线程转到接收队列。
    // TCP daemon 化入口：优先复用既有 daemon（状态文件 + 连通性 + initialize 握手
    // 校验），失败/缺失则经启动锁拉起新 daemon。返回的 client 已连上 daemon，未做
    // initialize 握手（由命令层统一调用 `initialize()`）。
    fn ensure_tcp() -> Result<Self, String> {
        let state = daemon_state()?;
        // 复用路径：状态文件指向的地址可连接且完成握手 → 复用该 daemon，不 spawn。
        if let Some(addr) = verify_reusable_daemon(&state)? {
            return Self::connect_to_addr(addr, None, None, None);
        }
        // 启动路径：取得独占启动锁后拉起新 daemon；拿不到锁说明另一 CLI 正在启动，
        // 轮询等待它发布可直接复用的 daemon。
        match acquire_daemon_start_lock(&state) {
            Ok(_lock) => {
                let (addr, child, stderr_rx, stderr_reader) = spawn_new_daemon()?;
                let client =
                    Self::connect_to_addr(addr, Some(child), Some(stderr_rx), Some(stderr_reader))?;
                drop(_lock);
                Ok(client)
            }
            Err(WouldBlock) => {
                // 另一 CLI 正在启动：轮询其发布的可复用 daemon（上限见
                // wait_for_reusable_daemon）。
                wait_for_reusable_daemon(&state)?
                    .ok_or_else(|| {
                        format!("timed out waiting for another CLI to start the app-server daemon")
                    })
                    .and_then(|addr| Self::connect_to_addr(addr, None, None, None))
            }
        }
    }

    // 连接已确定的 daemon 地址并建立读取线程；`child`/`stderr_rx`/`stderr_reader` 仅在本
    // 命令拉起新 daemon 时存在（复用既有 daemon 时为 None）。
    fn connect_to_addr(
        addr: SocketAddr,
        child: Option<Child>,
        stderr_rx: Option<Receiver<Result<String, String>>>,
        stderr_reader: Option<JoinHandle<()>>,
    ) -> Result<Self, String> {
        let stream = TcpStream::connect(addr).map_err(|error| {
            format!("failed to connect to app-server daemon at {addr}: {error}")
        })?;
        // 同一 socket 派生读写两个方向：读取半进入 reader 线程，写入半留给 write_message。
        let reader_stream = stream
            .try_clone()
            .map_err(|error| format!("failed to clone app-server connection: {error}"))?;
        let (inbound, inbound_reader) = spawn_line_reader(reader_stream);
        Ok(Self {
            child,
            transport: AppServerTransport::Tcp {
                stream: Some(stream),
                inbound,
                inbound_reader: Some(inbound_reader),
                _stderr_rx: stderr_rx,
                stderr_reader,
            },
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
    // -32010 trust_required 时先询问/写回 trust 决策，再重试一次原请求。
    fn turn_start(
        &mut self,
        thread_id: &str,
        text: &str,
        render: bool,
    ) -> Result<(Turn, Vec<JsonRpcNotification>), String> {
        let params = TurnStartParams {
            thread_id: thread_id.to_string(),
            input: vec![InputItem::Text {
                text: text.to_string(),
            }],
        };
        let reply = match self.request::<rpc_methods::TurnStart>(&params) {
            Ok(reply) => reply,
            Err(error) if error.code == APP_ERROR_TRUST_REQUIRED => {
                self.resolve_trust_for_cwd(&error)?;
                self.request::<rpc_methods::TurnStart>(&params)?
            }
            Err(error) => return Err(error.into()),
        };
        let turn = render_and_wait_terminal(
            self,
            reply.result.turn.clone(),
            reply.notifications.clone(),
            render,
        )?;
        Ok((turn, reply.notifications))
    }

    /// 处理 -32010：交互时提示 `Trust project folder? <cwd> [y/N]` 并写回决策；
    /// 非交互（stdin 非 tty）直接按不信任处理（不提示）。
    fn resolve_trust_for_cwd(&mut self, error: &AppServerRpcError) -> Result<(), String> {
        let cwd = error
            .data
            .as_ref()
            .and_then(|data| data.get("cwd"))
            .and_then(Value::as_str)
            .filter(|cwd| !cwd.trim().is_empty())
            .map(str::to_string)
            .unwrap_or(canonical_current_dir()?);
        if std::io::stdin().is_terminal() {
            eprint!("Trust project folder? {cwd} [y/N] ");
            std::io::stderr()
                .flush()
                .map_err(|error| format!("failed to flush trust prompt: {error}"))?;
            let mut answer = String::new();
            std::io::stdin()
                .read_line(&mut answer)
                .map_err(|error| format!("failed to read trust decision: {error}"))?;
            let trusted = matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes");
            self.project_trust(&cwd, ProjectTrustDecision::Set(trusted))?;
        } else {
            self.project_trust(&cwd, ProjectTrustDecision::Set(false))?;
        }
        Ok(())
    }

    // 查询/设置项目信任决策。
    fn project_trust(
        &mut self,
        path: &str,
        decision: ProjectTrustDecision,
    ) -> Result<ProjectTrustResult, String> {
        let reply = self.request::<rpc_methods::ProjectTrust>(&ProjectTrustParams {
            path: path.to_string(),
            decision,
        })?;
        Ok(reply.result)
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
    fn request<M: RpcMethod>(
        &mut self,
        params: &M::Params,
    ) -> Result<RpcReply<M::Result>, AppServerRpcError> {
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
                    return Err(AppServerRpcError {
                        code: response.error.code,
                        message: response.error.message,
                        data: response.error.data,
                    });
                }
                JsonRpcMessage::Success(_) | JsonRpcMessage::Error(_) => {}
                JsonRpcMessage::Request(_) => {
                    return Err("app-server emitted a request on the response channel"
                        .to_string()
                        .into());
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
        let payload = message.to_wire_value();
        let write_result = {
            let writer = self.write_side_mut()?;
            writeln!(writer, "{payload}")
        };
        if let Err(error) = write_result {
            return Err(
                self.classify_transport_write_error("failed to write app-server request", error)
            );
        }

        let flush_result = {
            let writer = self.write_side_mut()?;
            writer.flush()
        };
        if let Err(error) = flush_result {
            return Err(
                self.classify_transport_write_error("failed to flush app-server request", error)
            );
        }
        Ok(())
    }

    // 返回当前传输的可写侧（stdio stdin 或 TCP stream）。
    fn write_side_mut(&mut self) -> Result<&mut dyn Write, String> {
        match &mut self.transport {
            AppServerTransport::Stdio { stdin, .. } => stdin
                .as_mut()
                .ok_or_else(|| "app-server stdin unavailable".to_string())
                .map(|writer| writer as &mut dyn Write),
            AppServerTransport::Tcp { stream, .. } => stream
                .as_mut()
                .ok_or_else(|| "app-server connection unavailable".to_string())
                .map(|stream| stream as &mut dyn Write),
        }
    }

    // 写端失败时先确认 app-server 是否已经退出，避免以竞争性的 Broken pipe
    // 覆盖“响应前退出”的稳定传输错误；进程仍存活时保留真实 I/O 原因。复用既有
    // daemon（无子进程）时只保留真实 I/O 原因。
    fn classify_transport_write_error(&mut self, operation: &str, error: std::io::Error) -> String {
        match self.child.as_mut() {
            Some(child) => match child.try_wait() {
                Ok(Some(status)) => format!("app-server exited before response: {status}"),
                Ok(None) => format!("{operation}: {error}"),
                Err(status_error) => format!(
                    "{operation}: {error}; failed to poll app-server process status: {status_error}"
                ),
            },
            None => format!("{operation}: {error}"),
        }
    }

    // 从当前传输的读取队列接收一行，并区分超时与断开（保持原 stdio 语义）。
    fn recv_line(&mut self, timeout: Duration) -> Result<String, String> {
        let result = match &self.transport {
            AppServerTransport::Stdio { stdout, .. } => stdout.recv_timeout(timeout),
            AppServerTransport::Tcp { inbound, .. } => inbound.recv_timeout(timeout),
        };
        let exit_status = self
            .child
            .as_mut()
            .map(|child| {
                child
                    .try_wait()
                    .map_err(|error| format!("failed to poll app-server process status: {error}"))
            })
            .transpose()?
            .flatten();
        match result {
            Ok(line) => line,
            Err(RecvTimeoutError::Timeout) => {
                if let Some(status) = exit_status {
                    return Err(format!("app-server exited before response: {status}"));
                }
                Err("timed out waiting for app-server response".to_string())
            }
            Err(RecvTimeoutError::Disconnected) => {
                if let Some(status) = exit_status {
                    return Err(format!("app-server exited before response: {status}"));
                }
                Err("app-server closed stdout".to_string())
            }
        }
    }

    // 从读取队列读一条消息，并区分超时、断开与非法 JSON。
    fn read_message(&mut self, timeout: Duration) -> Result<JsonRpcMessage, String> {
        loop {
            let line = self.recv_line(timeout)?;
            // 防御：残留子进程可能直写非 JSON 行（含 lossy 替换后的坏字节）；
            // 跳过非 JSON 行，不因垃圾行终止协议（真正的 response 行仍会被解析）。
            match serde_json::from_str(line.trim()) {
                Ok(message) => return Ok(message),
                Err(_) => continue,
            }
        }
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
    // stdio 一次性子进程：请求 shutdown、回收；TCP daemon：只关闭连接，让空闲自停接管。
    fn drop(&mut self) {
        // stdio 请求服务端 shutdown（stdin 存在时说明传输仍存活）。
        let stdio_open = matches!(
            &self.transport,
            AppServerTransport::Stdio { stdin: Some(_), .. }
        );
        if stdio_open {
            let id = self.next_request_id();
            if let Ok(message) =
                JsonRpcMessage::request(Method::ServerShutdown, id, EmptyParams::default())
            {
                let _ = self.write_message(&message);
            }
        }
        match &mut self.transport {
            AppServerTransport::Stdio {
                stdin,
                stdout_reader,
                ..
            } => {
                let _ = stdin.take();
                let reader_handle = stdout_reader.take();
                // 等待一次性子进程退出，超时则 kill。
                if let Some(child) = self.child.as_mut() {
                    let deadline = Instant::now() + SHUTDOWN_WAIT_TIMEOUT;
                    loop {
                        match child.try_wait() {
                            Ok(Some(_)) => break,
                            Ok(None) if Instant::now() < deadline => {
                                thread::sleep(Duration::from_millis(10));
                            }
                            Ok(None) | Err(_) => {
                                let _ = child.kill();
                                break;
                            }
                        }
                    }
                    let _ = child.wait();
                }
                if let Some(handle) = reader_handle {
                    let _ = handle.join();
                }
            }
            AppServerTransport::Tcp {
                stream,
                inbound_reader,
                stderr_reader,
                ..
            } => {
                // TCP daemon：不发送 shutdown、不 wait/kill 子进程（无论本命令拉起新
                // daemon 还是复用既有 daemon）。关闭连接通知服务端 EOF，daemon 空闲自停
                // 接管退出。显式 shutdown(Both) 让 daemon 立即读到干净 EOF（而非 RST），
                // 从而尽快回到 accept 循环、准确开始空闲计时。入站 stderr 读取线程在
                // CLI 进程退出时会随之关闭句柄，因此不 join（join 会等待 daemon 退出）。
                if let Some(stream) = stream.as_ref() {
                    let _ = stream.shutdown(std::net::Shutdown::Both);
                }
                let _ = stream.take();
                let _ = inbound_reader.take();
                let _ = stderr_reader.take();
            }
        }
    }
}

// 将任意字节流（stdio stdout / stderr / TCP 入站）按行转发到客户端接收队列。
// 防御：残留子进程可能直写管道写入非 UTF-8 字节（Windows 句柄继承）；遇非法
// 字节用 lossy 替换并继续读下一行，不让单个坏行终止整个客户端。
fn spawn_line_reader(
    reader: impl Read + Send + 'static,
) -> (Receiver<Result<String, String>>, JoinHandle<()>) {
    let (sender, receiver) = mpsc::channel();
    let handle = thread::spawn(move || {
        let mut reader = BufReader::new(reader);
        loop {
            let mut bytes = Vec::new();
            let read_result = reader.read_until(b'\n', &mut bytes);
            match read_result {
                Ok(0) => break,
                Ok(_) => {
                    // lossy：容忍行内非 UTF-8 字节（残留进程直写），避免客户端崩溃。
                    let line = String::from_utf8_lossy(&bytes).into_owned();
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

// 读取 `SINGULARITY_APP_SERVER_TRANSPORT` 选择承载；缺省为 TCP（daemon 化默认）。
fn transport_mode() -> Result<TransportMode, String> {
    match std::env::var(APP_SERVER_TRANSPORT_ENV).as_deref() {
        Ok("stdio") => Ok(TransportMode::Stdio),
        Ok("tcp") | Err(_) => Ok(TransportMode::Tcp),
        Ok(other) => Err(format!(
            "unsupported {APP_SERVER_TRANSPORT_ENV}: {other} (expected 'tcp' or 'stdio')"
        )),
    }
}

// 从 stderr 读取队列中等待并解析 listen announce 行
// `SINGULARITY_APP_SERVER_LISTENING <addr>`，得到实际回环地址；超时或子进程
// 提前退出时报错。
fn read_listen_announce(
    stderr_rx: &Receiver<Result<String, String>>,
    child: &mut Child,
    timeout: Duration,
) -> Result<SocketAddr, String> {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match stderr_rx.recv_timeout(remaining) {
            Ok(Ok(line)) => {
                if let Some(addr) = line.trim().strip_prefix(LISTEN_ANNOUNCE_PREFIX) {
                    let addr = addr.parse::<SocketAddr>().map_err(|error| {
                        format!("invalid app-server listen announce '{line}': {error}")
                    })?;
                    return Ok(addr);
                }
                // 其他 stderr 行（如启动期告警）忽略，继续等待 announce。
            }
            Ok(Err(error)) => return Err(error),
            Err(RecvTimeoutError::Timeout) | Err(RecvTimeoutError::Disconnected) => {
                let status = child.try_wait().map_err(|error| {
                    format!("failed to poll app-server process status: {error}")
                })?;
                if let Some(status) = status {
                    return Err(format!(
                        "app-server exited before announcing TCP listen address: {status}"
                    ));
                }
                return Err(format!(
                    "timed out waiting for app-server to announce TCP listen address (>{:.0}s)",
                    timeout.as_secs()
                ));
            }
        }
    }
}

// ===== daemon 状态文件与启动锁 =====

/// daemon 状态/锁文件路径（基于用户级 singularity 目录解析，CLI 与 app-server 同一逻辑）。
struct DaemonStatePaths {
    state_file: PathBuf,
    lock_file: PathBuf,
}

/// 常驻 daemon 发布到状态文件的自身信息（启动方 CLI 写入）。
#[derive(serde::Serialize, serde::Deserialize)]
struct DaemonState {
    pid: u32,
    port: u16,
    started_at: String,
}

/// 独占启动锁：持有 OS 文件锁的 `File`；drop 即释放（进程崩溃由 OS 自动释放，
/// 无需 PID 残活探测——残留空锁文件被下次 `try_lock` 直接获得并覆盖）。
struct DaemonStartLock {
    _file: File,
}

/// 启动锁被另一进程持有（另一 CLI 正在启动 daemon）。
struct WouldBlock;

fn daemon_state() -> Result<DaemonStatePaths, String> {
    let dir = user_singularity_home()
        .ok_or_else(|| "cannot resolve SINGULARITY_HOME for the app-server daemon".to_string())?;
    std::fs::create_dir_all(&dir)
        .map_err(|error| format!("failed to create {DAEMON_STATE_FILE_NAME} directory: {error}"))?;
    Ok(DaemonStatePaths {
        state_file: dir.join(DAEMON_STATE_FILE_NAME),
        lock_file: dir.join(DAEMON_LOCK_FILE_NAME),
    })
}

fn read_daemon_state(paths: &DaemonStatePaths) -> Result<Option<DaemonState>, String> {
    let Ok(contents) = std::fs::read_to_string(&paths.state_file) else {
        return Ok(None);
    };
    Ok(serde_json::from_str(&contents)
        .map_err(|error| format!("invalid {DAEMON_STATE_FILE_NAME}: {error}"))?)
}

/// 原子写 daemon 状态文件（临时文件 + rename）。
fn write_daemon_state(paths: &DaemonStatePaths, pid: u32, port: u16) -> Result<(), String> {
    let state = DaemonState {
        pid,
        port,
        started_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_secs().to_string())
            .unwrap_or_default(),
    };
    let json = serde_json::to_string(&state)
        .map_err(|error| format!("failed to serialize daemon state: {error}"))?;
    let temporary = paths.state_file.with_extension("json.tmp");
    std::fs::write(&temporary, json)
        .map_err(|error| format!("failed to write daemon state: {error}"))?;
    std::fs::rename(&temporary, &paths.state_file)
        .map_err(|error| format!("failed to publish daemon state: {error}"))?;
    Ok(())
}

/// 判定状态文件地址是否可直接复用：128 位回环端口能连接且能完成 app-server
/// initialize 握手，即认为既有 daemon 健康可复用。连接/握手失败（含 stale 状态文件
/// 指向死端口）返回 `None`，由调用方发起新 spawn。
fn verify_reusable_daemon(paths: &DaemonStatePaths) -> Result<Option<SocketAddr>, String> {
    let Some(state) = read_daemon_state(paths)? else {
        return Ok(None);
    };
    let addr = SocketAddr::new(
        std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
        state.port,
    );
    let Ok(stream) = TcpStream::connect_timeout(&addr, DAEMON_REUSE_CONNECT_TIMEOUT) else {
        return Ok(None);
    };
    if probe_daemon_handshake(stream) {
        Ok(Some(addr))
    } else {
        Ok(None)
    }
}

/// 在一条连接上完成一次最小 initialize 握手，用于确认对端是健康可用的 singularity
/// app-server（而非某个碰巧占用同一端口的其他进程）。握手完成即关闭连接。
fn probe_daemon_handshake(mut stream: TcpStream) -> bool {
    use std::io::Write;
    let _ = stream.set_read_timeout(Some(DAEMON_REUSE_CONNECT_TIMEOUT));
    let initialized = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "initialize",
        "id": 0,
        "params": {
            "clientInfo": {"name": CLI_CLIENT_NAME, "title": CLI_CLIENT_TITLE, "version": CLI_CLIENT_VERSION},
            "capabilities": null,
        },
    });
    if std::io::Write::write_all(
        &mut stream,
        &serde_json::to_vec(&initialized).unwrap_or_default(),
    )
    .is_err()
    {
        return false;
    }
    if Write::write_all(&mut stream, b"\n").is_err() {
        return false;
    }
    let _ = Write::flush(&mut stream);
    let Ok(clone) = stream.try_clone() else {
        return false;
    };
    let mut reader = BufReader::new(clone);
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() || line.trim().is_empty() {
        return false;
    }
    let Ok(response) = serde_json::from_str::<Value>(&line) else {
        return false;
    };
    response.get("result").is_some()
}

/// 尝试取得 daemon 启动锁；成功返回持有锁的 guard（drop 释放），被占用返回 `WouldBlock`。
fn acquire_daemon_start_lock(paths: &DaemonStatePaths) -> Result<DaemonStartLock, WouldBlock> {
    let file = match OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&paths.lock_file)
    {
        Ok(file) => file,
        Err(error) => {
            // 锁文件打不开（如目录权限）视为不可用，返回 WouldBlock 让调用方等待。
            let _ = error;
            return Err(WouldBlock);
        }
    };
    match file.try_lock() {
        Ok(()) => Ok(DaemonStartLock { _file: file }),
        Err(std::fs::TryLockError::WouldBlock) => Err(WouldBlock),
        Err(_) => Err(WouldBlock),
    }
}

/// 另一 CLI 正在启动 daemon：轮询 `verify_reusable_daemon` 直到可复用或超时。
fn wait_for_reusable_daemon(paths: &DaemonStatePaths) -> Result<Option<SocketAddr>, String> {
    let deadline = Instant::now() + DAEMON_START_WAIT_TIMEOUT;
    loop {
        if let Some(addr) = verify_reusable_daemon(paths)? {
            return Ok(Some(addr));
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        thread::sleep(DAEMON_START_POLL_INTERVAL);
    }
}

/// 拉起一个全新 daemon：spawn app-server（TCP 监听、端口 0）、从 stderr 读取实际端口、
/// 启动 stderr 排空线程。返回 (实际地址, 子进程, stderr 队列, stderr 排空线程句柄)。
fn spawn_new_daemon() -> Result<
    (
        SocketAddr,
        Child,
        Receiver<Result<String, String>>,
        JoinHandle<()>,
    ),
    String,
> {
    let mut command = ProcessCommand::new(app_server_bin()?);
    // TCP 模式下 daemon 不再使用 stdin/stdout；stderr 仍捕获以读取 announce 并在常驻期排空。
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    if std::io::stdin().is_terminal() {
        command.env(INTERACTIVE_UI_ENV, "1");
    }
    command.env(APP_SERVER_LISTEN_ENV, "tcp://127.0.0.1:0");
    if let Ok(db) = std::env::var(APP_SERVER_DB_ENV) {
        command.env(APP_SERVER_DB_ENV, db);
    }
    let paths = daemon_state()?;
    let mut child = command
        .spawn()
        .map_err(|error| format!("failed to start app-server daemon: {error}"))?;
    let result =
        (|| -> Result<(SocketAddr, Receiver<Result<String, String>>, JoinHandle<()>), String> {
            let stderr = child
                .stderr
                .take()
                .ok_or_else(|| "app-server daemon stderr unavailable".to_string())?;
            let (stderr_rx, stderr_reader) = spawn_line_reader(stderr);
            // 解析 stderr announce 行得到实际 SocketAddr；stderr 读取线程持续排空剩余输出。
            let addr = read_listen_announce(&stderr_rx, &mut child, APP_SERVER_START_TIMEOUT)?;
            // daemon 已就绪：发布自身状态（pid + 实际端口），供后续 CLI 复用。announce 与
            // 状态写入都可能晚于 connect，因此先写状态。
            write_daemon_state(&paths, child.id(), addr.port())
                .map_err(|error| format!("failed to record daemon state: {error}"))?;
            Ok((addr, stderr_rx, stderr_reader))
        })();
    match result {
        Ok((addr, stderr_rx, stderr_reader)) => Ok((addr, child, stderr_rx, stderr_reader)),
        Err(error) => {
            // 拉起失败：回收子进程，避免残留孤儿 daemon。
            let _ = child.kill();
            let _ = child.wait();
            Err(error)
        }
    }
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

// 规范化 sg trust 的目录参数（必须存在且为目录）。
fn canonical_directory_arg(path: &Path) -> Result<String, String> {
    let canonical = std::fs::canonicalize(path)
        .map_err(|error| format!("failed to resolve path {}: {error}", path.display()))?;
    if !canonical.is_dir() {
        return Err(format!("{} is not a directory", path.display()));
    }
    canonical
        .to_str()
        .map(str::to_string)
        .ok_or_else(|| "path is not valid UTF-8".to_string())
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
