//! `sg` 的命令行入口：通过 stdio JSON-RPC 调用 app-server 并渲染结果。

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command as ProcessCommand, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use clap::{Parser, Subcommand, ValueEnum};
use serde_json::{Value, json};
use singularity_core::ClientInfo;
use singularity_protocol::{InitializeParams, JsonRpcMessage, Method};

const APP_SERVER_BIN_ENV: &str = "SINGULARITY_APP_SERVER_BIN";
const APP_SERVER_DB_ENV: &str = "SINGULARITY_APP_SERVER_DB";
const EVAL_OUTPUT_DIR_ENV: &str = "SINGULARITY_EVAL_OUTPUT_DIR";
const DEFAULT_APP_SERVER_BIN: &str = "singularity_app_server";
const CLI_CLIENT_NAME: &str = "singularity_cli";
const CLI_CLIENT_TITLE: &str = "Singularity CLI";
const CLI_CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");
const DEFAULT_TRACE_TAIL_LIMIT: usize = 20;
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);
const EVAL_RESPONSE_TIMEOUT: Duration = Duration::from_secs(3600);
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
        #[arg(long, value_enum)]
        sandbox_mode: Option<SandboxModeArg>,
        #[arg(long, value_enum)]
        approval_policy: Option<ApprovalPolicyArg>,
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
    /// Render trace events for a thread or run id.
    Trace(TraceArgs),
    /// Record an approval decision through the app-server protocol.
    Approve {
        request_id: String,
        #[arg(long, value_enum)]
        decision: ApprovalDecisionArg,
        #[arg(long)]
        reason: Option<String>,
    },
    /// List pending approvals through the app-server protocol.
    Approvals,
    /// Configuration and runtime diagnostics.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Validate evaluation manifests and report the current runner path.
    Eval {
        #[command(subcommand)]
        command: EvalCommand,
    },
}

#[derive(Debug, Subcommand)]
// 配置与运行时诊断命令。
enum ConfigCommand {
    /// Print app-server client diagnostics.
    Doctor,
}

#[derive(Debug, Subcommand)]
// Evaluation 的 CLI 子命令。
enum EvalCommand {
    /// Validate and run an evaluation manifest.
    Run {
        manifest: PathBuf,
        #[arg(long)]
        run_id: String,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
// turn 查询与中断命令。
enum TurnCommand {
    /// Print the current turn status.
    Status { turn_id: String },
    /// Interrupt a running turn.
    Interrupt { turn_id: String },
}

#[derive(Debug, Parser)]
// trace 查询参数；无子命令时按 run_id 读取尾部事件。
struct TraceArgs {
    #[command(subcommand)]
    command: Option<TraceCommand>,
    run_id: Option<String>,
    #[arg(long)]
    limit: Option<usize>,
}

#[derive(Debug, Subcommand)]
// trace 的单事件操作。
enum TraceCommand {
    /// Show one trace event by id.
    Show { event_id: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
// approval 决定在 CLI 中的受控枚举表示。
enum ApprovalDecisionArg {
    Allow,
    Deny,
    Defer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
// thread/start 可选择的受控 sandbox 快照。
enum SandboxModeArg {
    ReadOnly,
    WorkspaceWrite,
}

impl SandboxModeArg {
    fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::WorkspaceWrite => "workspace-write",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
// thread/start 可选择的受控 approval 快照。
enum ApprovalPolicyArg {
    OnRequest,
    Never,
}

impl ApprovalPolicyArg {
    fn as_str(self) -> &'static str {
        match self {
            Self::OnRequest => "on-request",
            Self::Never => "never",
        }
    }
}

// approval CLI 枚举到协议 outcome 的转换边界。
impl ApprovalDecisionArg {
    // 将 CLI 枚举映射为 app-server 协议值。
    fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Deny => "deny",
            Self::Defer => "defer",
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
            sandbox_mode,
            approval_policy,
            json,
        } => {
            let mut client = AppServerClient::spawn()?;
            client.response_timeout = AGENT_TURN_RESPONSE_TIMEOUT;
            client.initialize()?;
            ensure_agent_loop_available(&mut client)?;
            let (thread, thread_events) = client.thread_start(
                model,
                sandbox_mode.map(SandboxModeArg::as_str),
                approval_policy.map(ApprovalPolicyArg::as_str),
                !json,
            )?;
            if !json {
                println!(
                    "thread {}",
                    required_str(&thread, &["thread", "thread_id"])?
                );
                render_thread_policy(&thread)?;
            }
            let (turn, turn_events) = client.turn_start(
                required_str(&thread, &["thread", "thread_id"])?,
                &goal,
                !json,
            )?;
            if json {
                let mut events = protocol_events(thread_events);
                events.extend(protocol_events(turn_events));
                println!(
                    "{}",
                    json!({
                        "thread": thread["thread"],
                        "turn": turn["turn"],
                        "events": events,
                    })
                );
            }
            fail_for_failed_turn(&turn["turn"])?;
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
            let thread = client.thread_resume(&thread_id)?;
            println!("thread {thread_id}");
            render_thread_policy(&thread)?;
            let (turn, _events) = client.turn_start(&thread_id, &instruction, true)?;
            fail_for_failed_turn(&turn["turn"])?;
            Ok(())
        }
        Command::Turn { command } => {
            let mut client = AppServerClient::spawn()?;
            client.initialize()?;
            match command {
                TurnCommand::Status { turn_id } => client.turn_status(&turn_id),
                TurnCommand::Interrupt { turn_id } => client.turn_interrupt(&turn_id),
            }
        }
        Command::Threads => {
            let mut client = AppServerClient::spawn()?;
            client.initialize()?;
            client.thread_list()?;
            Ok(())
        }
        Command::Trace(args) => {
            let mut client = AppServerClient::spawn()?;
            client.initialize()?;
            match args.command {
                Some(TraceCommand::Show { event_id }) => client.trace_show(&event_id),
                None => {
                    let run_id = args
                        .run_id
                        .ok_or_else(|| "trace requires a run id or subcommand".to_string())?;
                    client.trace_tail(&run_id, args.limit.unwrap_or(DEFAULT_TRACE_TAIL_LIMIT))
                }
            }
        }
        Command::Approve {
            request_id,
            decision,
            reason,
        } => {
            let mut client = AppServerClient::spawn()?;
            client.response_timeout = AGENT_TURN_RESPONSE_TIMEOUT;
            client.initialize()?;
            client.approval_decision(&request_id, decision, reason.as_deref().unwrap_or(""))
        }
        Command::Approvals => {
            let mut client = AppServerClient::spawn()?;
            client.initialize()?;
            client.approvals()?;
            Ok(())
        }
        Command::Config {
            command: ConfigCommand::Doctor,
        } => {
            println!("app_server_bin={}", app_server_bin()?);
            println!("app_server_db={}", app_server_db_display());
            println!("client=protocol-only");
            print_readiness()?;
            Ok(())
        }
        Command::Eval {
            command:
                EvalCommand::Run {
                    manifest,
                    run_id,
                    json,
                },
        } => run_eval(manifest, &run_id, json),
    }
}

// 在启动新 turn 前确认 AgentLoop 已完成且无 blocker。
fn ensure_agent_loop_available(client: &mut AppServerClient) -> Result<(), String> {
    let capability = client.agent_capability()?;
    let agent_loop = &capability["agentLoop"];
    let available = agent_loop["available"].as_bool().unwrap_or(false);
    let status = agent_loop["status"].as_str().unwrap_or("unknown");
    let blockers = agent_loop_blockers(agent_loop);
    if available && blockers == "none" && status == "completed" {
        return Ok(());
    }
    Err(format!(
        "AgentLoop is not available: status={status}; blockers={blockers}"
    ))
}

// 将 capability 中的 blocker 列表压缩为稳定的诊断文本。
fn agent_loop_blockers(agent_loop: &Value) -> String {
    let blockers = agent_loop["blockers"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(",")
        })
        .unwrap_or_default();
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
    let agent_loop = &capability["agentLoop"];
    println!(
        "agent_loop={}",
        agent_loop["status"].as_str().unwrap_or("unknown")
    );
    println!("evaluation=agent_loop");
    print_provider_configuration(&capability["providerConfiguration"])
}

// 校验并输出 provider capability，始终只暴露字段存在性。
fn print_provider_configuration(provider: &Value) -> Result<(), String> {
    let source = match provider["source"].as_str() {
        Some("process_env") => "process_env",
        Some("project_env") => "project_env",
        None if provider["source"].is_null() => "unconfigured",
        _ => {
            return Err("invalid agent capability: providerConfiguration.source".to_string());
        }
    };
    println!("provider_config_source={source}");
    let snapshot_id = provider["snapshotId"]
        .as_str()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| "invalid agent capability: providerConfiguration.snapshotId".to_string())?;
    println!("provider_snapshot_id={snapshot_id}");
    let configured = provider["configured"]
        .as_bool()
        .ok_or_else(|| "invalid agent capability: providerConfiguration.configured".to_string())?;
    println!("provider_configured={configured}");
    let blocker = match provider.get("configurationBlocker") {
        Some(Value::Null) | None => "none",
        Some(Value::String(blocker)) if !blocker.trim().is_empty() => blocker,
        _ => {
            return Err(
                "invalid agent capability: providerConfiguration.configurationBlocker".to_string(),
            );
        }
    };
    println!("provider_configuration_blocker={blocker}");
    for (name, field) in [
        ("SINGULARITY_API_KEY", "apiKeyPresent"),
        ("SINGULARITY_BASE_URL", "baseUrlPresent"),
        ("SINGULARITY_MODEL", "modelPresent"),
    ] {
        let present = provider[field]
            .as_bool()
            .ok_or_else(|| format!("invalid agent capability: providerConfiguration.{field}"))?;
        let status = if present {
            "present(redacted)"
        } else {
            "missing"
        };
        println!("{name}={status}");
    }
    Ok(())
}

// 通过 app-server 校验并执行指定 evaluation manifest。
fn run_eval(manifest: PathBuf, run_id: &str, json_output: bool) -> Result<(), String> {
    if !manifest.exists() {
        return Err(format!("eval manifest not found: {}", manifest.display()));
    }
    let mut client = AppServerClient::spawn()?;
    client.response_timeout = EVAL_RESPONSE_TIMEOUT;
    client.initialize()?;
    let result = client.eval_run(&manifest, run_id)?;
    if json_output {
        println!("{result}");
    } else {
        println!(
            "eval {} {} runner={}",
            result["run_id"].as_str().unwrap_or(run_id),
            result["status"].as_str().unwrap_or("unknown"),
            result["runner"].as_str().unwrap_or("unknown")
        );
    }
    if result["evaluation_passed"].as_bool().unwrap_or(false) {
        Ok(())
    } else {
        Err(result["blocker"]
            .as_str()
            .unwrap_or("evaluation_failed")
            .to_string())
    }
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

    // 完成 JSON-RPC initialize/initialized 握手。
    fn initialize(&mut self) -> Result<(), String> {
        let id = self.next_request_id();
        self.request(initialize_request(id))?;
        self.notify(Method::Initialized, json!({}))
    }

    // 创建 thread，并可选地渲染启动事件。
    fn thread_start(
        &mut self,
        model: Option<String>,
        sandbox_mode: Option<&str>,
        approval_policy: Option<&str>,
        render: bool,
    ) -> Result<(Value, Vec<Value>), String> {
        let id = self.next_request_id();
        let cwd = canonical_current_dir()?;
        let mut params = json!({"model": model, "cwd": cwd});
        if let Some(sandbox_mode) = sandbox_mode {
            params["sandboxMode"] = json!(sandbox_mode);
        }
        if let Some(approval_policy) = approval_policy {
            params["approvalPolicy"] = json!(approval_policy);
        }
        let responses = self.request(JsonRpcMessage::request(
            Method::ThreadStart,
            json!(id),
            params,
        ))?;
        if render {
            render_messages(&responses, false);
        }
        let result = first_result_ref(&responses)?.clone();
        Ok((result, responses))
    }

    // 提交 evaluation manifest，并返回 app-server 的结果对象。
    fn eval_run(&mut self, manifest: &Path, run_id: &str) -> Result<Value, String> {
        let id = self.next_request_id();
        let mut params = json!({"manifest": manifest.to_string_lossy(), "runId": run_id});
        if let Ok(output_root) = std::env::var(EVAL_OUTPUT_DIR_ENV) {
            params["outputRoot"] = json!(output_root);
        }
        let responses =
            self.request(JsonRpcMessage::request(Method::EvalRun, json!(id), params))?;
        first_result(responses)
    }

    // 恢复现有 thread，不向 app-server 上传历史。
    fn thread_resume(&mut self, thread_id: &str) -> Result<Value, String> {
        let id = self.next_request_id();
        first_result(self.request(JsonRpcMessage::request(
            Method::ThreadResume,
            json!(id),
            json!({"threadId": thread_id}),
        ))?)
    }

    // 读取 AgentLoop capability 快照。
    fn agent_capability(&mut self) -> Result<Value, String> {
        let id = self.next_request_id();
        first_result(self.request(JsonRpcMessage::request(
            Method::AgentCapability,
            json!(id),
            json!({}),
        ))?)
    }

    // 启动 turn、渲染事件，并在必要时轮询到终态。
    fn turn_start(
        &mut self,
        thread_id: &str,
        text: &str,
        render: bool,
    ) -> Result<(Value, Vec<Value>), String> {
        let id = self.next_request_id();
        let responses = self.request(JsonRpcMessage::request(
            Method::TurnStart,
            json!(id),
            json!({"threadId": thread_id, "input": [{"type": "text", "text": text}]}),
        ))?;
        let mut turn = first_result_ref(&responses)?.clone();
        if render {
            render_messages(&responses, should_render_assistant_summary(&turn["turn"]));
            render_turn(&turn["turn"]);
        }
        if should_poll_running_turn(&turn["turn"]) {
            let terminal =
                self.wait_for_turn_terminal(required_str(&turn["turn"], &["turn_id"])?, render)?;
            turn["turn"]["status"] = json!(terminal.status);
            if let Some(agent_loop_status) = terminal.agent_loop_status {
                turn["turn"]["agent_loop_status"] = json!(agent_loop_status);
            }
        }
        Ok((turn, responses))
    }

    // 按固定间隔查询 running turn，直到出现终态。
    fn wait_for_turn_terminal(&mut self, turn_id: &str, render: bool) -> Result<TurnView, String> {
        loop {
            thread::sleep(TURN_STATUS_POLL_INTERVAL);
            let turn = self.fetch_turn_status(turn_id)?;
            if turn.status != "running" {
                if render {
                    println!(
                        "turn {} {}{}",
                        turn.turn_id,
                        turn.status,
                        turn.agent_loop_status
                            .as_deref()
                            .map(|status| format!(" agent_loop_status={status}"))
                            .unwrap_or_default()
                    );
                }
                return Ok(turn);
            }
        }
    }

    // 将 turn/status 响应投影为 CLI 所需的最小视图。
    fn fetch_turn_status(&mut self, turn_id: &str) -> Result<TurnView, String> {
        let id = self.next_request_id();
        let result = first_result(self.request(JsonRpcMessage::request(
            Method::TurnStatus,
            json!(id),
            json!({"turnId": turn_id}),
        ))?)?;
        let turn = &result["turn"];
        Ok(TurnView {
            turn_id: required_str(turn, &["turn_id"])?.to_string(),
            status: required_str(turn, &["status"])?.to_string(),
            agent_loop_status: turn["agent_loop_status"].as_str().map(str::to_string),
        })
    }

    // 请求并打印持久化 thread 列表。
    fn thread_list(&mut self) -> Result<(), String> {
        let id = self.next_request_id();
        let result = first_result(self.request(JsonRpcMessage::request(
            Method::ThreadList,
            json!(id),
            json!({}),
        ))?)?;
        if let Some(threads) = result["threads"].as_array() {
            for thread in threads {
                println!(
                    "{} {}",
                    thread["thread_id"].as_str().unwrap_or(""),
                    thread["status"].as_str().unwrap_or("")
                );
                render_thread_policy_value(thread)?;
            }
        }
        Ok(())
    }

    // 请求并渲染单个 turn 的状态。
    fn turn_status(&mut self, turn_id: &str) -> Result<(), String> {
        let id = self.next_request_id();
        let result = first_result(self.request(JsonRpcMessage::request(
            Method::TurnStatus,
            json!(id),
            json!({"turnId": turn_id}),
        ))?)?;
        render_turn(&result["turn"]);
        Ok(())
    }

    // 请求中断 turn，并打印服务端返回的状态。
    fn turn_interrupt(&mut self, turn_id: &str) -> Result<(), String> {
        let id = self.next_request_id();
        let result = first_result(self.request(JsonRpcMessage::request(
            Method::TurnInterrupt,
            json!(id),
            json!({"turnId": turn_id}),
        ))?)?;
        println!(
            "turn {} {}{}",
            result["turnId"].as_str().unwrap_or(turn_id),
            result["status"].as_str().unwrap_or(""),
            agent_loop_status_suffix(&result)
        );
        Ok(())
    }

    // 请求并按顺序渲染 run 的 trace 尾部。
    fn trace_tail(&mut self, run_id: &str, limit: usize) -> Result<(), String> {
        let id = self.next_request_id();
        let result = first_result(self.request(JsonRpcMessage::request(
            Method::TraceTail,
            json!(id),
            json!({"runId": run_id, "limit": limit}),
        ))?)?;
        if let Some(events) = result["events"].as_array() {
            for event in events {
                render_trace_event(event);
            }
        }
        Ok(())
    }

    // 请求并渲染指定 trace event。
    fn trace_show(&mut self, event_id: &str) -> Result<(), String> {
        let id = self.next_request_id();
        let result = first_result(self.request(JsonRpcMessage::request(
            Method::TraceShow,
            json!(id),
            json!({"eventId": event_id}),
        ))?)?;
        render_trace_event(&result["event"]);
        Ok(())
    }

    // 请求并打印当前 pending approval 列表。
    fn approvals(&mut self) -> Result<(), String> {
        let id = self.next_request_id();
        let result = first_result(self.request(JsonRpcMessage::request(
            Method::ApprovalList,
            json!(id),
            json!({}),
        ))?)?;
        if let Some(approvals) = result["approvals"].as_array() {
            for approval in approvals {
                println!(
                    "{} {}",
                    approval["request_id"].as_str().unwrap_or(""),
                    approval["action"].as_str().unwrap_or("")
                );
            }
        }
        Ok(())
    }

    // 提交 approval 决定并打印已记录的结果。
    fn approval_decision(
        &mut self,
        request_id: &str,
        decision: ApprovalDecisionArg,
        reason: &str,
    ) -> Result<(), String> {
        let id = self.next_request_id();
        let outcome = decision.as_str();
        let result = first_result(self.request(JsonRpcMessage::request(
            Method::ApprovalDecision,
            json!(id),
            json!({
                "request_id": request_id,
                "decision_id": format!("{request_id}_decision"),
                "outcome": outcome,
                "reason": reason,
            }),
        ))?)?;
        let decision = &result["decision"];
        println!(
            "approval {} {}",
            decision["request_id"].as_str().unwrap_or(request_id),
            decision["outcome"].as_str().unwrap_or(outcome)
        );
        Ok(())
    }

    // 发送请求并只接收匹配 id 的响应，同时保留通知事件。
    fn request(&mut self, message: JsonRpcMessage) -> Result<Vec<Value>, String> {
        let id = message
            .id
            .clone()
            .ok_or_else(|| "request id missing".to_string())?;
        self.write_message(&message)?;
        let mut messages = Vec::new();
        loop {
            let value = self.read_message(self.response_timeout)?;
            if value.get("id") == Some(&id) {
                if value.get("error").is_some() {
                    return Err(format!("app-server error: {}", value["error"]["message"]));
                }
                messages.push(value);
                return Ok(messages);
            }
            if value.get("method").is_some() {
                messages.push(value);
            }
        }
    }

    // 向 app-server 发送 JSON-RPC notification。
    fn notify(&mut self, method: Method, params: Value) -> Result<(), String> {
        let message = JsonRpcMessage::notification(method.as_str(), params);
        self.write_message(&message)
    }

    // 序列化、写入并 flush 一条 JSON-RPC 消息。
    fn write_message(&mut self, message: &JsonRpcMessage) -> Result<(), String> {
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| "app-server stdin unavailable".to_string())?;
        writeln!(stdin, "{}", message.to_wire_value())
            .map_err(|error| format!("failed to write app-server request: {error}"))?;
        stdin
            .flush()
            .map_err(|error| format!("failed to flush app-server request: {error}"))
    }

    // 从 stdout reader 读取一条消息，并区分超时、断开与非法 JSON。
    fn read_message(&mut self, timeout: Duration) -> Result<Value, String> {
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
            let _ = self.write_message(&JsonRpcMessage::request(
                Method::ServerShutdown,
                json!(id),
                json!({}),
            ));
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

// 渲染 thread 实际持久化的安全策略快照；旧的协议响应不带该摘要时保持兼容显示。
fn render_thread_policy(envelope: &Value) -> Result<(), String> {
    if let Some(thread) = envelope.get("thread") {
        render_thread_policy_value(thread)?;
    }
    Ok(())
}

fn render_thread_policy_value(thread: &Value) -> Result<(), String> {
    let sandbox_mode = thread.get("sandboxMode").and_then(Value::as_str);
    let approval_policy = thread.get("approvalPolicy").and_then(Value::as_str);
    match (sandbox_mode, approval_policy) {
        (Some(sandbox_mode), Some(approval_policy)) => {
            println!("thread_policy sandbox_mode={sandbox_mode} approval_policy={approval_policy}");
            Ok(())
        }
        (None, None) => Ok(()),
        _ => Err("thread response has an incomplete policy snapshot".to_string()),
    }
}

// 构造 CLI 使用的 initialize 请求。
fn initialize_request(id: i64) -> JsonRpcMessage {
    JsonRpcMessage::request(
        Method::Initialize,
        json!(id),
        serde_json::to_value(InitializeParams {
            client_info: ClientInfo::new(CLI_CLIENT_NAME, CLI_CLIENT_TITLE, CLI_CLIENT_VERSION),
            capabilities: None,
        })
        .expect("serialize initialize params"),
    )
}

// 从响应集合中取出首个 result。
fn first_result(messages: Vec<Value>) -> Result<Value, String> {
    messages
        .into_iter()
        .find_map(|message| message.get("result").cloned())
        .ok_or_else(|| "app-server response did not include result".to_string())
}

// 以借用形式从响应集合中取出首个 result。
fn first_result_ref(messages: &[Value]) -> Result<&Value, String> {
    messages
        .iter()
        .find_map(|message| message.get("result"))
        .ok_or_else(|| "app-server response did not include result".to_string())
}

// 过滤并脱敏可公开渲染的协议事件。
fn protocol_events(messages: Vec<Value>) -> Vec<Value> {
    messages
        .into_iter()
        .filter_map(safe_protocol_event)
        .collect()
}

// 将单条协议通知投影为不泄露 raw payload 的事件。
fn safe_protocol_event(message: Value) -> Option<Value> {
    let method = message["method"].as_str()?;
    let item_id = message["params"]["item"]["item_id"].as_str().unwrap_or("");
    match method {
        "item/agentMessage/delta" => Some(json!({
            "method": method,
            "params": {
                "item_id": item_id,
                "delta": message["params"]["delta"].as_str().unwrap_or(""),
            },
        })),
        "item/started" | "item/completed" => Some(json!({
            "method": method,
            "params": {"item_id": item_id},
        })),
        _ => Some(json!({"method": method})),
    }
}

// 按协议 method 渲染 thread、turn 与 item 事件。
fn render_messages(messages: &[Value], render_assistant_summary: bool) {
    for message in messages {
        if let Some(method) = message["method"].as_str() {
            match method {
                "thread/started" => {
                    if let Some(thread_id) = message["params"]["thread"]["thread_id"].as_str() {
                        println!("thread/started {thread_id}");
                    }
                }
                "turn/started" => {
                    if let Some(turn_id) = message["params"]["turn"]["turn_id"].as_str() {
                        println!("turn/started {turn_id}");
                    }
                    render_turn(&message["params"]["turn"]);
                }
                "item/started" | "item/completed" => {
                    if let Some(item_id) = message["params"]["item"]["item_id"].as_str() {
                        println!("{method} {item_id}");
                    }
                }
                "item/agentMessage/delta" | "item/commandExecution/outputDelta" => {
                    let text = message["params"]["delta"]
                        .as_str()
                        .or_else(|| message["params"]["output"].as_str())
                        .unwrap_or("");
                    println!("{method} {text}");
                    if render_assistant_summary && method == "item/agentMessage/delta" {
                        println!("assistant {text}");
                    }
                }
                _ => println!("{method}"),
            }
        }
    }
}

// 判断是否应额外输出已完成的 assistant 摘要。
fn should_render_assistant_summary(turn: &Value) -> bool {
    turn["status"].as_str() == Some("completed")
        && turn["agent_loop_status"].as_str() == Some("completed")
}

// 判断 running turn 是否仍可通过轮询等待终态。
fn should_poll_running_turn(turn: &Value) -> bool {
    turn["status"].as_str() == Some("running")
        && matches!(
            turn["agent_loop_status"].as_str(),
            Some("running" | "cancel_requested")
        )
}

// 渲染 turn 的稳定状态行。
fn render_turn(turn: &Value) {
    let turn_id = turn["turn_id"].as_str().unwrap_or("");
    if turn_id.is_empty() {
        return;
    }
    println!(
        "turn {} {} agent_loop_status={}",
        turn_id,
        turn["status"].as_str().unwrap_or(""),
        turn["agent_loop_status"].as_str().unwrap_or("")
    );
}

// 从响应对象提取可选的 AgentLoop 状态后缀。
fn agent_loop_status_suffix(value: &Value) -> String {
    value["agent_loop_status"]
        .as_str()
        .map(|status| format!(" agent_loop_status={status}"))
        .unwrap_or_default()
}

// 渲染 trace event 的公开摘要字段。
fn render_trace_event(event: &Value) {
    println!(
        "trace {} {} {}",
        event["event_id"].as_str().unwrap_or(""),
        event["component"].as_str().unwrap_or(""),
        event["summary"].as_str().unwrap_or("")
    );
}

// 将失败、blocked 或未能安全轮询的 turn 映射为 CLI 错误。
fn fail_for_failed_turn(turn: &Value) -> Result<(), String> {
    let status = turn["status"].as_str().unwrap_or("");
    let agent_loop_status = turn["agent_loop_status"].as_str().unwrap_or("");
    let non_terminal_running = status == "running" && !should_poll_running_turn(turn);
    if non_terminal_running
        || matches!(status, "failed" | "blocked" | "interrupted" | "cancelled")
        || matches!(agent_loop_status, "failed" | "blocked" | "cancelled")
    {
        let turn_id = turn["turn_id"].as_str().unwrap_or("");
        if turn_id.is_empty() {
            return Err(format!("error {status}: turn {status}"));
        }
        return Err(format!(
            "error {status}: turn {status}; turn {turn_id} {status}"
        ));
    }
    Ok(())
}

#[derive(Debug)]
// 轮询过程中用于判断 turn 终态的最小字段集合。
struct TurnView {
    turn_id: String,
    status: String,
    agent_loop_status: Option<String>,
}

// 从 JSON 路径读取必需的字符串字段。
fn required_str<'a>(value: &'a Value, path: &[&str]) -> Result<&'a str, String> {
    let mut current = value;
    for key in path {
        current = &current[*key];
    }
    current
        .as_str()
        .ok_or_else(|| format!("missing string field {}", path.join(".")))
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
