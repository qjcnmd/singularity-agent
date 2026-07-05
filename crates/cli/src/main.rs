use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command as ProcessCommand, Stdio};

use clap::{Parser, Subcommand};
use serde_json::{Value, json};
use singularity_core::ClientInfo;
use singularity_protocol::{InitializeParams, JsonRpcMessage, Method};

const APP_SERVER_BIN_ENV: &str = "SINGULARITY_APP_SERVER_BIN";
const APP_SERVER_DB_ENV: &str = "SINGULARITY_APP_SERVER_DB";
const DEFAULT_APP_SERVER_BIN: &str = "singularity_app_server";
const CLI_CLIENT_NAME: &str = "singularity_cli";
const CLI_CLIENT_TITLE: &str = "Singularity Rust CLI";
const CLI_CLIENT_VERSION: &str = "0.1.0";
const DEFAULT_TRACE_TAIL_LIMIT: usize = 20;

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
    },
    /// Alias for run with chat-oriented wording.
    Chat {
        goal: String,
        #[arg(long)]
        model: Option<String>,
    },
    /// Resume an existing thread and submit a new user turn.
    Continue {
        thread_id: String,
        instruction: String,
    },
    /// List persisted threads through the app-server protocol.
    Threads,
    /// Render trace events for a thread or run id.
    Trace {
        run_id: String,
        #[arg(long)]
        limit: Option<usize>,
    },
    /// List pending approvals through the app-server protocol.
    Approvals,
    /// Configuration and runtime diagnostics.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    /// Print app-server client diagnostics.
    Doctor,
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
        Command::ThreadStart { model } => print_wire_request(JsonRpcMessage::request(
            Method::ThreadStart,
            json!(1),
            json!({"model": model}),
        )),
        Command::Daemon { db } => run_daemon(db),
        Command::Run { goal, model } | Command::Chat { goal, model } => {
            let mut client = AppServerClient::spawn()?;
            client.initialize()?;
            let thread = client.thread_start(model)?;
            println!(
                "thread {}",
                required_str(&thread, &["thread", "thread_id"])?
            );
            client.turn_start(required_str(&thread, &["thread", "thread_id"])?, &goal)?;
            Ok(())
        }
        Command::Continue {
            thread_id,
            instruction,
        } => {
            let mut client = AppServerClient::spawn()?;
            client.initialize()?;
            client.thread_read(&thread_id)?;
            println!("thread {thread_id}");
            client.turn_start(&thread_id, &instruction)?;
            Ok(())
        }
        Command::Threads => {
            let mut client = AppServerClient::spawn()?;
            client.initialize()?;
            client.thread_list()?;
            Ok(())
        }
        Command::Trace { run_id, limit } => {
            let mut client = AppServerClient::spawn()?;
            client.initialize()?;
            client.trace_tail(&run_id, limit.unwrap_or(DEFAULT_TRACE_TAIL_LIMIT))?;
            Ok(())
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
            println!("app_server_bin={}", app_server_bin());
            println!("app_server_db={}", app_server_db_display());
            println!("client=protocol-only");
            Ok(())
        }
    }
}

fn print_wire_request(message: JsonRpcMessage) -> Result<(), String> {
    println!("{}", message.to_wire_value());
    Ok(())
}

fn run_daemon(db: Option<PathBuf>) -> Result<(), String> {
    let mut command = ProcessCommand::new(app_server_bin());
    if let Some(db) = db {
        command.env(APP_SERVER_DB_ENV, db);
    }
    let status = command
        .status()
        .map_err(|error| format!("failed to start app-server daemon: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("app-server daemon exited with {status}"))
    }
}

struct AppServerClient {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: i64,
}

impl AppServerClient {
    fn spawn() -> Result<Self, String> {
        let mut command = ProcessCommand::new(app_server_bin());
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
        Ok(Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            next_id: 1,
        })
    }

    fn initialize(&mut self) -> Result<(), String> {
        let id = self.next_request_id();
        self.request(initialize_request(id), 0)?;
        self.notify(Method::Initialized, json!({}))
    }

    fn thread_start(&mut self, model: Option<String>) -> Result<Value, String> {
        let id = self.next_request_id();
        let responses = self.request(
            JsonRpcMessage::request(Method::ThreadStart, json!(id), json!({"model": model})),
            1,
        )?;
        render_messages(&responses);
        first_result(responses)
    }

    fn thread_read(&mut self, thread_id: &str) -> Result<Value, String> {
        let id = self.next_request_id();
        first_result(self.request(
            JsonRpcMessage::request(
                Method::ThreadRead,
                json!(id),
                json!({"threadId": thread_id}),
            ),
            0,
        )?)
    }

    fn turn_start(&mut self, thread_id: &str, text: &str) -> Result<(), String> {
        let id = self.next_request_id();
        let responses = self.request(
            JsonRpcMessage::request(
                Method::TurnStart,
                json!(id),
                json!({"threadId": thread_id, "input": [{"type": "text", "text": text}]}),
            ),
            4,
        )?;
        render_messages(&responses);
        Ok(())
    }

    fn thread_list(&mut self) -> Result<(), String> {
        let id = self.next_request_id();
        let result = first_result(self.request(
            JsonRpcMessage::request(Method::ThreadList, json!(id), json!({})),
            0,
        )?)?;
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

    fn trace_tail(&mut self, run_id: &str, limit: usize) -> Result<(), String> {
        let id = self.next_request_id();
        let result = first_result(self.request(
            JsonRpcMessage::request(
                Method::TraceTail,
                json!(id),
                json!({"runId": run_id, "limit": limit}),
            ),
            0,
        )?)?;
        if let Some(events) = result["events"].as_array() {
            for event in events {
                println!(
                    "{} {}",
                    event["event_id"].as_str().unwrap_or(""),
                    event["summary"].as_str().unwrap_or("")
                );
            }
        }
        Ok(())
    }

    fn approvals(&mut self) -> Result<(), String> {
        let id = self.next_request_id();
        let result = first_result(self.request(
            JsonRpcMessage::request(Method::ApprovalList, json!(id), json!({})),
            0,
        )?)?;
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

    fn request(
        &mut self,
        message: JsonRpcMessage,
        expected_notifications: usize,
    ) -> Result<Vec<Value>, String> {
        let id = message
            .id
            .clone()
            .ok_or_else(|| "request id missing".to_string())?;
        self.write_message(&message)?;
        let mut messages = Vec::new();
        let mut saw_response = false;
        let mut notifications = 0;
        loop {
            let value = self.read_message()?;
            let is_response = value.get("id") == Some(&id);
            if value.get("method").is_some() {
                notifications += 1;
            }
            if value.get("error").is_some() {
                return Err(format!("app-server error: {}", value["error"]["message"]));
            }
            messages.push(value);
            if is_response {
                saw_response = true;
            }
            if saw_response && notifications >= expected_notifications {
                return Ok(messages);
            }
        }
    }

    fn notify(&mut self, method: Method, params: Value) -> Result<(), String> {
        let message = JsonRpcMessage::notification(method.as_str(), params);
        self.write_message(&message)
    }

    fn write_message(&mut self, message: &JsonRpcMessage) -> Result<(), String> {
        writeln!(self.stdin, "{}", message.to_wire_value())
            .map_err(|error| format!("failed to write app-server request: {error}"))?;
        self.stdin
            .flush()
            .map_err(|error| format!("failed to flush app-server request: {error}"))
    }

    fn read_message(&mut self) -> Result<Value, String> {
        let mut line = String::new();
        let bytes = self
            .stdout
            .read_line(&mut line)
            .map_err(|error| format!("failed to read app-server response: {error}"))?;
        if bytes == 0 {
            return Err("app-server closed stdout".to_string());
        }
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
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
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

fn render_messages(messages: &[Value]) {
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
                }
                "item/started" | "item/completed" => {
                    if let Some(item_id) = message["params"]["item"]["item_id"].as_str() {
                        println!("{method} {item_id}");
                    }
                }
                "item/agentMessage/delta" | "item/commandExecution/outputDelta" => {
                    println!(
                        "{method} {}",
                        message["params"]["delta"]
                            .as_str()
                            .or_else(|| message["params"]["output"].as_str())
                            .unwrap_or("")
                    );
                }
                _ => println!("{method}"),
            }
        }
    }
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

fn app_server_bin() -> String {
    std::env::var(APP_SERVER_BIN_ENV).unwrap_or_else(|_| DEFAULT_APP_SERVER_BIN.to_string())
}

fn app_server_db_display() -> String {
    std::env::var(APP_SERVER_DB_ENV)
        .unwrap_or_else(|_| ".singularity/rust-app-server.sqlite3".to_string())
}
