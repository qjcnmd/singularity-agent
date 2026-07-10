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
const LEGACY_PYTHON_RUNTIME_ENV_PREFIXES: [&str; 2] =
    ["SINGULARITY_PYTHON_", "SINGULARITY_SIDECAR_"];
const DEFAULT_APP_SERVER_BIN: &str = "singularity_app_server";
const CLI_CLIENT_NAME: &str = "singularity_cli";
const CLI_CLIENT_TITLE: &str = "Singularity Rust CLI";
const CLI_CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");
const DEFAULT_TRACE_TAIL_LIMIT: usize = 20;
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);
const EVAL_RESPONSE_TIMEOUT: Duration = Duration::from_secs(3600);
const AGENT_TURN_RESPONSE_TIMEOUT: Duration = Duration::from_secs(3600);
const SHUTDOWN_WAIT_TIMEOUT: Duration = Duration::from_secs(2);
const TURN_STATUS_POLL_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Debug, Parser)]
#[command(name = "sg")]
#[command(about = "Rust CLI client for the Singularity app-server protocol")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Print the JSON-RPC initialize request used by the protocol client.
    ProtocolInit,
    /// Print a thread/start JSON-RPC request.
    ThreadStart {
        #[arg(long)]
        model: Option<String>,
    },
    /// Run the stdio app-server until stdin closes.
    Daemon {
        #[arg(long)]
        db: Option<PathBuf>,
    },
    /// Start a thread, submit a goal, and render protocol events.
    Run {
        goal: String,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Alias for run with chat-oriented wording.
    Chat {
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
enum ConfigCommand {
    /// Print app-server client diagnostics.
    Doctor,
}

#[derive(Debug, Subcommand)]
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
enum TurnCommand {
    /// Print the current turn status.
    Status { turn_id: String },
    /// Interrupt a running turn.
    Interrupt { turn_id: String },
}

#[derive(Debug, Parser)]
struct TraceArgs {
    #[command(subcommand)]
    command: Option<TraceCommand>,
    run_id: Option<String>,
    #[arg(long)]
    limit: Option<usize>,
}

#[derive(Debug, Subcommand)]
enum TraceCommand {
    /// Show one trace event by id.
    Show { event_id: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum ApprovalDecisionArg {
    Allow,
    Deny,
    Defer,
}

impl ApprovalDecisionArg {
    fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Deny => "deny",
            Self::Defer => "defer",
        }
    }
}

fn main() {
    if let Err(error) = run_cli(Cli::parse()) {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

fn run_cli(cli: Cli) -> Result<(), String> {
    match cli.command.unwrap_or(Command::ProtocolInit) {
        Command::ProtocolInit => print_wire_request(initialize_request(0)),
        Command::ThreadStart { model } => {
            let cwd = canonical_current_dir()?;
            print_wire_request(JsonRpcMessage::request(
                Method::ThreadStart,
                json!(1),
                json!({"model": model, "cwd": cwd}),
            ))
        }
        Command::Daemon { db } => run_daemon(db),
        Command::Run { goal, model, json } | Command::Chat { goal, model, json } => {
            let mut client = AppServerClient::spawn()?;
            client.response_timeout = AGENT_TURN_RESPONSE_TIMEOUT;
            client.initialize()?;
            ensure_native_agent_loop_available(&mut client)?;
            let (thread, thread_events) = client.thread_start(model, !json)?;
            if !json {
                println!(
                    "thread {}",
                    required_str(&thread, &["thread", "thread_id"])?
                );
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
            ensure_native_agent_loop_available(&mut client)?;
            client.thread_resume(&thread_id)?;
            println!("thread {thread_id}");
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

fn ensure_native_agent_loop_available(client: &mut AppServerClient) -> Result<(), String> {
    let capability = client.agent_capability()?;
    let native = &capability["nativeAgentLoop"];
    let available = native["available"].as_bool().unwrap_or(false);
    let status = native["status"].as_str().unwrap_or("unknown");
    let blockers = native_agent_loop_blockers(native);
    if available && blockers == "none" && status == "completed" {
        return Ok(());
    }
    Err(format!(
        "Rust AgentLoop is not available: status={status}; blockers={blockers}"
    ))
}

fn native_agent_loop_blockers(native: &Value) -> String {
    let blockers = native["blockers"]
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

fn print_readiness() -> Result<(), String> {
    let mut client = AppServerClient::spawn()?;
    client.response_timeout = AGENT_TURN_RESPONSE_TIMEOUT;
    client.initialize()?;
    let capability = client.agent_capability()?;
    let native = &capability["nativeAgentLoop"];
    println!(
        "native_agent_loop={}",
        native["status"].as_str().unwrap_or("unknown")
    );
    println!("evaluation=rust_native");
    print_provider_readiness(&capability["providerReadiness"])
}

fn print_provider_readiness(provider: &Value) -> Result<(), String> {
    let source = match provider["source"].as_str() {
        Some("process_env") => "process_env",
        Some("project_env") => "project_env",
        None if provider["source"].is_null() => "unconfigured",
        _ => {
            return Err("invalid agent capability: providerReadiness.source".to_string());
        }
    };
    println!("provider_config_source={source}");
    let snapshot_id = provider["snapshotId"]
        .as_str()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| "invalid agent capability: providerReadiness.snapshotId".to_string())?;
    println!("provider_snapshot_id={snapshot_id}");
    let ready = provider["ready"]
        .as_bool()
        .ok_or_else(|| "invalid agent capability: providerReadiness.ready".to_string())?;
    println!("provider_ready={ready}");
    let blocker = match provider.get("blocker") {
        Some(Value::Null) | None => "none",
        Some(Value::String(blocker)) if !blocker.trim().is_empty() => blocker,
        _ => {
            return Err("invalid agent capability: providerReadiness.blocker".to_string());
        }
    };
    println!("provider_blocker={blocker}");
    for (name, field) in [
        ("SINGULARITY_API_KEY", "apiKeyPresent"),
        ("SINGULARITY_BASE_URL", "baseUrlPresent"),
        ("SINGULARITY_MODEL", "modelPresent"),
    ] {
        let present = provider[field]
            .as_bool()
            .ok_or_else(|| format!("invalid agent capability: providerReadiness.{field}"))?;
        let status = if present {
            "present(redacted)"
        } else {
            "missing"
        };
        println!("{name}={status}");
    }
    Ok(())
}

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

fn print_wire_request(message: JsonRpcMessage) -> Result<(), String> {
    println!("{}", message.to_wire_value());
    Ok(())
}

fn run_daemon(db: Option<PathBuf>) -> Result<(), String> {
    let mut command = ProcessCommand::new(app_server_bin()?);
    if let Some(db) = db {
        command.env(APP_SERVER_DB_ENV, db);
    }
    scrub_legacy_python_runtime_env(&mut command);
    let status = command
        .status()
        .map_err(|error| format!("failed to start app-server daemon: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("app-server daemon exited with {status}"))
    }
}

fn scrub_legacy_python_runtime_env(command: &mut ProcessCommand) {
    for (name, _) in std::env::vars_os() {
        let name_upper = name.to_string_lossy().to_ascii_uppercase();
        if LEGACY_PYTHON_RUNTIME_ENV_PREFIXES
            .iter()
            .any(|prefix| name_upper.starts_with(prefix))
        {
            command.env_remove(name);
        }
    }
}

struct AppServerClient {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: Receiver<Result<String, String>>,
    stdout_reader: Option<JoinHandle<()>>,
    response_timeout: Duration,
    next_id: i64,
}

impl AppServerClient {
    fn spawn() -> Result<Self, String> {
        let mut command = ProcessCommand::new(app_server_bin()?);
        command.stdin(Stdio::piped()).stdout(Stdio::piped());
        if let Ok(db) = std::env::var(APP_SERVER_DB_ENV) {
            command.env(APP_SERVER_DB_ENV, db);
        }
        scrub_legacy_python_runtime_env(&mut command);
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

    fn initialize(&mut self) -> Result<(), String> {
        let id = self.next_request_id();
        self.request(initialize_request(id))?;
        self.notify(Method::Initialized, json!({}))
    }

    fn thread_start(
        &mut self,
        model: Option<String>,
        render: bool,
    ) -> Result<(Value, Vec<Value>), String> {
        let id = self.next_request_id();
        let cwd = canonical_current_dir()?;
        let responses = self.request(JsonRpcMessage::request(
            Method::ThreadStart,
            json!(id),
            json!({"model": model, "cwd": cwd}),
        ))?;
        if render {
            render_messages(&responses, false);
        }
        let result = first_result_ref(&responses)?.clone();
        Ok((result, responses))
    }

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

    fn thread_resume(&mut self, thread_id: &str) -> Result<Value, String> {
        let id = self.next_request_id();
        first_result(self.request(JsonRpcMessage::request(
            Method::ThreadResume,
            json!(id),
            json!({"threadId": thread_id}),
        ))?)
    }

    fn agent_capability(&mut self) -> Result<Value, String> {
        let id = self.next_request_id();
        first_result(self.request(JsonRpcMessage::request(
            Method::AgentCapability,
            json!(id),
            json!({}),
        ))?)
    }

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
            render_messages(&responses, should_render_assistant_alias(&turn["turn"]));
            if should_render_response_turn(&responses, &turn["turn"]) {
                render_turn(&turn["turn"]);
            }
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
            }
        }
        Ok(())
    }

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

    fn notify(&mut self, method: Method, params: Value) -> Result<(), String> {
        let message = JsonRpcMessage::notification(method.as_str(), params);
        self.write_message(&message)
    }

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

    fn next_request_id(&mut self) -> i64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }
}

impl Drop for AppServerClient {
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

fn canonical_current_dir() -> Result<String, String> {
    let cwd = std::env::current_dir()
        .map_err(|error| format!("failed to read current directory: {error}"))?
        .canonicalize()
        .map_err(|error| format!("failed to canonicalize current directory: {error}"))?;
    cwd.to_str()
        .map(str::to_string)
        .ok_or_else(|| "current directory is not valid UTF-8".to_string())
}
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

fn first_result(messages: Vec<Value>) -> Result<Value, String> {
    messages
        .into_iter()
        .find_map(|message| message.get("result").cloned())
        .ok_or_else(|| "app-server response did not include result".to_string())
}

fn first_result_ref(messages: &[Value]) -> Result<&Value, String> {
    messages
        .iter()
        .find_map(|message| message.get("result"))
        .ok_or_else(|| "app-server response did not include result".to_string())
}

fn has_method(messages: &[Value], method: &str) -> bool {
    messages
        .iter()
        .any(|message| message["method"].as_str() == Some(method))
}

fn protocol_events(messages: Vec<Value>) -> Vec<Value> {
    messages
        .into_iter()
        .filter_map(safe_protocol_event)
        .collect()
}

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

fn render_messages(messages: &[Value], render_assistant_alias: bool) {
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
                    if render_assistant_alias && method == "item/agentMessage/delta" {
                        println!("assistant {text}");
                    }
                }
                _ => println!("{method}"),
            }
        }
    }
}

fn should_render_assistant_alias(turn: &Value) -> bool {
    turn["status"].as_str() == Some("completed")
        && turn["agent_loop_status"].as_str() == Some("completed")
}

fn should_render_response_turn(messages: &[Value], turn: &Value) -> bool {
    if !has_method(messages, "turn/started") {
        return true;
    }
    turn["agent_loop_status"].as_str() != Some("not_migrated")
}

fn should_poll_running_turn(turn: &Value) -> bool {
    turn["status"].as_str() == Some("running")
        && matches!(
            turn["agent_loop_status"].as_str(),
            Some("running" | "cancel_requested")
        )
}

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

fn agent_loop_status_suffix(value: &Value) -> String {
    value["agent_loop_status"]
        .as_str()
        .map(|status| format!(" agent_loop_status={status}"))
        .unwrap_or_default()
}

fn render_trace_event(event: &Value) {
    println!(
        "trace {} {} {}",
        event["event_id"].as_str().unwrap_or(""),
        event["component"].as_str().unwrap_or(""),
        event["summary"].as_str().unwrap_or("")
    );
}

fn fail_for_failed_turn(turn: &Value) -> Result<(), String> {
    let status = turn["status"].as_str().unwrap_or("");
    let agent_loop_status = turn["agent_loop_status"].as_str().unwrap_or("");
    let non_terminal_running = status == "running" && !should_poll_running_turn(turn);
    if non_terminal_running
        || matches!(status, "failed" | "blocked" | "interrupted" | "cancelled")
        || matches!(
            agent_loop_status,
            "failed" | "blocked" | "cancelled" | "not_migrated"
        )
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
struct TurnView {
    turn_id: String,
    status: String,
    agent_loop_status: Option<String>,
}

fn required_str<'a>(value: &'a Value, path: &[&str]) -> Result<&'a str, String> {
    let mut current = value;
    for key in path {
        current = &current[*key];
    }
    current
        .as_str()
        .ok_or_else(|| format!("missing string field {}", path.join(".")))
}

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

fn app_server_db_display() -> String {
    std::env::var(APP_SERVER_DB_ENV)
        .unwrap_or_else(|_| ".singularity/rust-app-server.sqlite3".to_string())
}

fn sibling_app_server_bin() -> Option<PathBuf> {
    let mut path = std::env::current_exe().ok()?;
    path.pop();
    path.push(format!(
        "{DEFAULT_APP_SERVER_BIN}{}",
        std::env::consts::EXE_SUFFIX
    ));
    path.is_file().then_some(path)
}
