#![cfg(windows)]

use serde_json::{Value, json};
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

const ORIGINAL_TASK: &str = "Record a durable plan and then finish this same turn";
const PLAN_CALL_ID: &str = "durable_plan_call";
const PLAN_STEP: &str = "finish after process restart";
const COMMAND_CALL_ID: &str = "inflight_command_call";

#[test]
fn turn_resume_after_process_kill_replays_durable_history_and_completes_same_turn() {
    let dir = tempfile::tempdir().expect("temp dir");
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let db_path = dir.path().join("sessions.sqlite3");
    let provider = ResumeProvider::start();

    let (mut first_child, mut first_input, mut first_output) =
        spawn_app_server(&db_path, &workspace, &provider.base_url);
    initialize_process(&mut first_input, &mut first_output);
    let thread_id = start_thread(&mut first_input, &mut first_output, &workspace, 2);
    send_json(
        &mut first_input,
        json!({
            "jsonrpc": "2.0",
            "method": "turn/start",
            "id": 3,
            "params": {
                "threadId": thread_id,
                "input": [{"type": "text", "text": ORIGINAL_TASK}]
            }
        }),
    );
    let started = first_output.recv_method("turn/started", Duration::from_secs(5));
    let turn_id = started["params"]["turn"]["turn_id"]
        .as_str()
        .expect("started turn id")
        .to_string();

    provider
        .checkpoint_request_seen
        .recv_timeout(Duration::from_secs(10))
        .expect("request after completed update_plan result");
    kill_and_reap(&mut first_child);
    provider
        .first_process_killed
        .send(())
        .expect("release killed provider connection");

    let (mut resumed_child, mut resumed_input, mut resumed_output) =
        spawn_app_server(&db_path, &workspace, &provider.base_url);
    initialize_process(&mut resumed_input, &mut resumed_output);
    send_json(
        &mut resumed_input,
        json!({
            "jsonrpc": "2.0",
            "method": "turn/resume",
            "id": 4,
            "params": {"turnId": turn_id}
        }),
    );

    let resumed_request = match provider
        .resumed_request
        .recv_timeout(Duration::from_secs(10))
    {
        Ok(request) => request,
        Err(RecvTimeoutError::Timeout) => {
            let response = resumed_output.recv_id(4, Duration::from_secs(2));
            panic!("turn/resume did not reach the provider: {response}");
        }
        Err(RecvTimeoutError::Disconnected) => panic!("resume provider disconnected"),
    };
    assert_resumed_request(&resumed_request);

    let mut resumed_events = Vec::new();
    let completed = loop {
        let message = resumed_output.recv_next_event(Duration::from_secs(10));
        if message["id"] == 4 {
            break message;
        }
        resumed_events.push(message);
    };
    let item_delta_index = resumed_events
        .iter()
        .position(|event| event["method"] == "item/agentMessage/delta")
        .expect("resumed item delta");
    let item_id = resumed_events[item_delta_index]["params"]["item"]["item_id"]
        .as_str()
        .expect("delta item id");
    let item_started_index = resumed_events
        .iter()
        .position(|event| {
            event["method"] == "item/started" && event["params"]["item"]["item_id"] == item_id
        })
        .expect("resumed item start");
    let item_completed_index = resumed_events
        .iter()
        .position(|event| {
            event["method"] == "item/completed" && event["params"]["item"]["item_id"] == item_id
        })
        .expect("resumed item completion");
    let turn_completed_index = resumed_events
        .iter()
        .position(|event| event["method"] == "turn/completed")
        .expect("resumed turn completion");
    assert!(item_started_index < item_delta_index);
    assert!(item_delta_index < item_completed_index);
    assert!(item_completed_index < turn_completed_index);
    assert_eq!(completed["result"]["turn"]["turn_id"], turn_id);
    assert_eq!(completed["result"]["turn"]["status"], "completed");
    assert_eq!(
        completed["result"]["turn"]["agent_loop_status"],
        "completed"
    );

    send_json(
        &mut resumed_input,
        json!({
            "jsonrpc": "2.0",
            "method": "turn/status",
            "id": 5,
            "params": {"turnId": turn_id}
        }),
    );
    let status = resumed_output.recv_id(5, Duration::from_secs(5));
    assert_eq!(status["result"]["turn"]["turn_id"], turn_id);
    assert_eq!(status["result"]["turn"]["status"], "completed");

    shutdown_process(
        &mut resumed_child,
        &mut resumed_input,
        &mut resumed_output,
        6,
    );
    provider.worker.join().expect("provider worker");
}

#[test]
fn turn_resume_rejects_unknown_inflight_command_without_retrying_it() {
    let dir = tempfile::tempdir().expect("temp dir");
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let db_path = dir.path().join("sessions.sqlite3");
    let provider = CommandProvider::start();

    let (mut first_child, mut first_input, mut first_output) =
        spawn_app_server(&db_path, &workspace, &provider.base_url);
    initialize_process(&mut first_input, &mut first_output);
    let thread_id = start_thread_with_policy(
        &mut first_input,
        &mut first_output,
        &workspace,
        2,
        "read-only",
        "never",
    );
    send_json(
        &mut first_input,
        json!({
            "jsonrpc": "2.0",
            "method": "turn/start",
            "id": 3,
            "params": {
                "threadId": thread_id,
                "input": [{"type": "text", "text": "run the controlled long command"}]
            }
        }),
    );
    let started = first_output.recv_method("turn/started", Duration::from_secs(5));
    let turn_id = started["params"]["turn"]["turn_id"]
        .as_str()
        .expect("turn id")
        .to_string();
    provider
        .command_request_seen
        .recv_timeout(Duration::from_secs(10))
        .expect("command response sent");
    wait_for_running_tool_trace(&mut first_input, &mut first_output, &thread_id);

    kill_and_reap(&mut first_child);

    let (mut resumed_child, mut resumed_input, mut resumed_output) =
        spawn_app_server(&db_path, &workspace, &provider.base_url);
    initialize_process(&mut resumed_input, &mut resumed_output);
    send_json(
        &mut resumed_input,
        json!({
            "jsonrpc": "2.0",
            "method": "turn/resume",
            "id": 4,
            "params": {"turnId": turn_id}
        }),
    );
    let rejected = resumed_output.recv_id(4, Duration::from_secs(5));
    assert!(
        rejected.get("error").is_some(),
        "resume unexpectedly ran: {rejected}"
    );

    let store = singularity_store::SessionStore::open(&db_path).expect("reopen store");
    let execution = store
        .get_tool_execution(&format!("turn:{turn_id}:tool:{COMMAND_CALL_ID}"))
        .expect("read tool execution")
        .expect("durable command execution");
    assert_eq!(
        execution.state,
        singularity_store::ToolExecutionState::Unknown
    );
    assert_eq!(
        provider.production_requests.load(Ordering::SeqCst),
        1,
        "turn/resume must not request the provider or replay the command"
    );

    shutdown_process(
        &mut resumed_child,
        &mut resumed_input,
        &mut resumed_output,
        5,
    );
    provider.stop();
}

#[test]
fn concurrent_turn_resume_allows_only_one_execution_owner_and_provider_path() {
    let dir = tempfile::tempdir().expect("temp dir");
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let db_path = dir.path().join("sessions.sqlite3");
    let provider = ConcurrentResumeProvider::start();

    let (mut first_child, mut first_input, mut first_output) =
        spawn_app_server(&db_path, &workspace, &provider.base_url);
    initialize_process(&mut first_input, &mut first_output);
    let thread_id = start_thread(&mut first_input, &mut first_output, &workspace, 2);
    send_json(
        &mut first_input,
        json!({
            "jsonrpc": "2.0",
            "method": "turn/start",
            "id": 3,
            "params": {
                "threadId": thread_id,
                "input": [{"type": "text", "text": "complete after one owner resumes"}]
            }
        }),
    );
    let started = first_output.recv_method("turn/started", Duration::from_secs(5));
    let turn_id = started["params"]["turn"]["turn_id"]
        .as_str()
        .expect("turn id")
        .to_string();
    provider
        .initial_request_seen
        .recv_timeout(Duration::from_secs(10))
        .expect("initial provider request");
    kill_and_reap(&mut first_child);
    provider
        .first_process_killed
        .send(())
        .expect("release initial provider connection");

    let (mut owner_child, mut owner_input, mut owner_output) =
        spawn_app_server(&db_path, &workspace, &provider.base_url);
    let (mut loser_child, mut loser_input, mut loser_output) =
        spawn_app_server(&db_path, &workspace, &provider.base_url);
    initialize_process(&mut owner_input, &mut owner_output);
    initialize_process(&mut loser_input, &mut loser_output);

    send_json(
        &mut owner_input,
        json!({
            "jsonrpc": "2.0",
            "method": "turn/resume",
            "id": 4,
            "params": {"turnId": turn_id}
        }),
    );
    provider
        .owner_request_seen
        .recv_timeout(Duration::from_secs(10))
        .expect("winning resume provider request");
    send_json(
        &mut loser_input,
        json!({
            "jsonrpc": "2.0",
            "method": "turn/resume",
            "id": 5,
            "params": {"turnId": turn_id}
        }),
    );
    let rejected = loser_output.recv_id(5, Duration::from_secs(5));
    assert!(
        rejected.get("error").is_some(),
        "second resume acquired an owner: {rejected}"
    );
    assert_eq!(
        provider.production_requests.load(Ordering::SeqCst),
        2,
        "only the original request and one resumed request may reach the provider"
    );

    provider
        .release_owner
        .send(())
        .expect("release owner provider");
    let completed = owner_output.recv_id(4, Duration::from_secs(10));
    assert_eq!(completed["result"]["turn"]["turn_id"], turn_id);
    assert_eq!(completed["result"]["turn"]["status"], "completed");

    shutdown_process(&mut owner_child, &mut owner_input, &mut owner_output, 6);
    shutdown_process(&mut loser_child, &mut loser_input, &mut loser_output, 7);
    provider.worker.join().expect("concurrent provider worker");
}

fn assert_resumed_request(request: &Value) {
    let input = request["input"].as_array().expect("Responses input array");
    let user_messages = input
        .iter()
        .filter(|item| item["type"] == "message" && item["role"] == "user")
        .collect::<Vec<_>>();
    assert_eq!(
        user_messages.len(),
        1,
        "resume must not synthesize a user continue message: {request}"
    );
    assert_eq!(user_messages[0]["content"], ORIGINAL_TASK);

    let tool_call = input
        .iter()
        .find(|item| item["type"] == "function_call" && item["call_id"] == PLAN_CALL_ID)
        .expect("durable update_plan ToolCall");
    assert_eq!(tool_call["name"], "update_plan");
    let arguments: Value =
        serde_json::from_str(tool_call["arguments"].as_str().expect("tool arguments"))
            .expect("tool arguments json");
    assert_eq!(arguments["steps"][0]["step"], PLAN_STEP);
    assert!(
        arguments.get("verification").is_none(),
        "read-only plan must not invent a mutation verification plan"
    );

    let tool_result = input
        .iter()
        .find(|item| item["type"] == "function_call_output" && item["call_id"] == PLAN_CALL_ID)
        .expect("durable update_plan ToolResult");
    let output = tool_result["output"].as_str().expect("tool result output");
    let output: Value = serde_json::from_str(output).expect("tool result json");
    assert_eq!(output["content"]["plan"]["steps"][0]["step"], PLAN_STEP);
}

struct ResumeProvider {
    base_url: String,
    checkpoint_request_seen: Receiver<()>,
    first_process_killed: Sender<()>,
    resumed_request: Receiver<Value>,
    worker: thread::JoinHandle<()>,
}

impl ResumeProvider {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind provider");
        let address = listener.local_addr().expect("provider address");
        let (checkpoint_tx, checkpoint_rx) = mpsc::channel();
        let (killed_tx, killed_rx) = mpsc::channel();
        let (resumed_tx, resumed_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            let mut production_request_index = 0;
            loop {
                let (mut stream, _) = listener.accept().expect("accept provider request");
                let request = read_http_json(&mut stream);
                if let Some(response) = capability_probe_response(&request) {
                    write_json_response(&mut stream, &response);
                    continue;
                }
                assert_eq!(request["stream"], true, "production request must stream");
                production_request_index += 1;
                match production_request_index {
                    1 => write_stream_response(&mut stream, plan_response()),
                    2 => {
                        checkpoint_tx
                            .send(())
                            .expect("signal checkpoint-backed request");
                        killed_rx
                            .recv_timeout(Duration::from_secs(15))
                            .expect("first app-server killed");
                    }
                    3 => {
                        resumed_tx.send(request).expect("send resumed request");
                        write_final_stream_response(&mut stream, final_response());
                        break;
                    }
                    other => panic!("unexpected production request {other}"),
                }
            }
        });
        Self {
            base_url: format!("http://{address}/v1/responses"),
            checkpoint_request_seen: checkpoint_rx,
            first_process_killed: killed_tx,
            resumed_request: resumed_rx,
            worker,
        }
    }
}

struct CommandProvider {
    base_url: String,
    command_request_seen: Receiver<()>,
    production_requests: Arc<AtomicUsize>,
    stop: Sender<()>,
    worker: thread::JoinHandle<()>,
}

struct ConcurrentResumeProvider {
    base_url: String,
    initial_request_seen: Receiver<()>,
    first_process_killed: Sender<()>,
    owner_request_seen: Receiver<()>,
    release_owner: Sender<()>,
    production_requests: Arc<AtomicUsize>,
    worker: thread::JoinHandle<()>,
}

impl ConcurrentResumeProvider {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind concurrent provider");
        let address = listener.local_addr().expect("provider address");
        let (initial_tx, initial_rx) = mpsc::channel();
        let (killed_tx, killed_rx) = mpsc::channel();
        let (owner_tx, owner_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let production_requests = Arc::new(AtomicUsize::new(0));
        let worker_count = Arc::clone(&production_requests);
        let worker = thread::spawn(move || {
            loop {
                let (mut stream, _) = listener.accept().expect("accept concurrent provider");
                let request = read_http_json(&mut stream);
                if let Some(response) = capability_probe_response(&request) {
                    write_json_response(&mut stream, &response);
                    continue;
                }
                let index = worker_count.fetch_add(1, Ordering::SeqCst) + 1;
                match index {
                    1 => {
                        initial_tx.send(()).expect("signal initial request");
                        killed_rx
                            .recv_timeout(Duration::from_secs(15))
                            .expect("initial process killed");
                    }
                    2 => {
                        owner_tx.send(()).expect("signal owner request");
                        release_rx
                            .recv_timeout(Duration::from_secs(15))
                            .expect("release owner request");
                        write_final_stream_response(&mut stream, final_response());
                        break;
                    }
                    other => panic!("unexpected concurrent production request {other}"),
                }
            }
        });
        Self {
            base_url: format!("http://{address}/v1/responses"),
            initial_request_seen: initial_rx,
            first_process_killed: killed_tx,
            owner_request_seen: owner_rx,
            release_owner: release_tx,
            production_requests,
            worker,
        }
    }
}

impl CommandProvider {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind command provider");
        listener
            .set_nonblocking(true)
            .expect("nonblocking command provider");
        let address = listener.local_addr().expect("provider address");
        let (command_tx, command_rx) = mpsc::channel();
        let (stop_tx, stop_rx) = mpsc::channel();
        let production_requests = Arc::new(AtomicUsize::new(0));
        let worker_count = Arc::clone(&production_requests);
        let worker = thread::spawn(move || {
            loop {
                if stop_rx.try_recv().is_ok() {
                    break;
                }
                let (mut stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(error) => panic!("accept command provider: {error}"),
                };
                stream
                    .set_nonblocking(false)
                    .expect("blocking command provider stream");
                let request = read_http_json(&mut stream);
                if let Some(response) = capability_probe_response(&request) {
                    write_json_response(&mut stream, &response);
                    continue;
                }
                let index = worker_count.fetch_add(1, Ordering::SeqCst) + 1;
                assert_eq!(index, 1, "unexpected command provider retry");
                write_stream_response(&mut stream, command_response());
                command_tx.send(()).expect("signal command response");
            }
        });
        Self {
            base_url: format!("http://{address}/v1/responses"),
            command_request_seen: command_rx,
            production_requests,
            stop: stop_tx,
            worker,
        }
    }

    fn stop(self) {
        self.stop.send(()).expect("stop command provider");
        self.worker.join().expect("command provider worker");
    }
}

fn plan_response() -> Value {
    json!({
        "id": "response_plan",
        "object": "response",
        "status": "completed",
        "output": [{
            "type": "function_call",
            "call_id": PLAN_CALL_ID,
            "name": "update_plan",
            "arguments": json!({
                "steps": [{"step": PLAN_STEP, "status": "completed"}]
            })
            .to_string()
        }],
        "usage": usage()
    })
}

fn command_response() -> Value {
    json!({
        "id": "response_command",
        "object": "response",
        "status": "completed",
        "output": [{
            "type": "function_call",
            "call_id": COMMAND_CALL_ID,
            "name": "command",
            "arguments": json!({
                "command": "pwsh -NoProfile -Command \"Start-Sleep -Seconds 30\"",
                "cwd": ".",
                "timeout_seconds": 60
            }).to_string()
        }],
        "usage": usage()
    })
}

fn final_response() -> Value {
    json!({
        "id": "response_final",
        "object": "response",
        "status": "completed",
        "output": [{
            "type": "message",
            "role": "assistant",
            "content": [{"type": "output_text", "text": "resumed and completed"}]
        }],
        "usage": usage()
    })
}

fn usage() -> Value {
    json!({
        "input_tokens": 3,
        "output_tokens": 2,
        "total_tokens": 5,
        "input_tokens_details": {"cached_tokens": 0},
        "output_tokens_details": {"reasoning_tokens": 0}
    })
}

fn spawn_app_server(
    db_path: &Path,
    workspace: &Path,
    base_url: &str,
) -> (Child, ChildStdin, JsonOutput) {
    let mut child = Command::new(app_server_bin())
        .current_dir(workspace)
        .env("SINGULARITY_APP_SERVER_DB", db_path)
        .env("SINGULARITY_MODEL_PROVIDER", "openai_compatible")
        .env("SINGULARITY_MODEL", "gpt-test")
        .env("SINGULARITY_BASE_URL", base_url)
        .env("SINGULARITY_API_KEY", "test-secret")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn app-server");
    let input = child.stdin.take().expect("app-server stdin");
    let stdout = child.stdout.take().expect("app-server stdout");
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let line = line.expect("read app-server output");
            if sender
                .send(serde_json::from_str(&line).expect("app-server json line"))
                .is_err()
            {
                break;
            }
        }
    });
    (
        child,
        input,
        JsonOutput {
            receiver,
            buffered: VecDeque::new(),
        },
    )
}

fn initialize_process(input: &mut ChildStdin, output: &mut JsonOutput) {
    send_json(
        input,
        json!({
            "jsonrpc": "2.0",
            "method": "initialize",
            "id": 1,
            "params": {"clientInfo": {"name": "resume-test", "title": "Resume Test", "version": "0.1.0"}}
        }),
    );
    assert!(
        output
            .recv_id(1, Duration::from_secs(5))
            .get("result")
            .is_some()
    );
    send_json(
        input,
        json!({"jsonrpc": "2.0", "method": "initialized", "params": {}}),
    );
    send_json(
        input,
        json!({
            "jsonrpc": "2.0",
            "method": "event/subscribe",
            "id": 99,
            "params": {"eventTypes": [
                "turn/started",
                "turn/completed",
                "item/started",
                "item/agentMessage/delta",
                "item/completed"
            ]}
        }),
    );
    assert!(
        output
            .recv_id(99, Duration::from_secs(5))
            .get("result")
            .is_some()
    );
}

fn start_thread(
    input: &mut ChildStdin,
    output: &mut JsonOutput,
    workspace: &Path,
    id: i64,
) -> String {
    send_json(
        input,
        json!({
            "jsonrpc": "2.0",
            "method": "thread/start",
            "id": id,
            "params": {"model": "gpt-test", "cwd": workspace}
        }),
    );
    output.recv_id(id, Duration::from_secs(5))["result"]["thread"]["thread_id"]
        .as_str()
        .expect("thread id")
        .to_string()
}

fn start_thread_with_policy(
    input: &mut ChildStdin,
    output: &mut JsonOutput,
    workspace: &Path,
    id: i64,
    sandbox_mode: &str,
    approval_policy: &str,
) -> String {
    send_json(
        input,
        json!({
            "jsonrpc": "2.0",
            "method": "thread/start",
            "id": id,
            "params": {
                "model": "gpt-test",
                "cwd": workspace,
                "sandboxMode": sandbox_mode,
                "approvalPolicy": approval_policy
            }
        }),
    );
    output.recv_id(id, Duration::from_secs(5))["result"]["thread"]["thread_id"]
        .as_str()
        .expect("thread id")
        .to_string()
}

fn wait_for_running_tool_trace(input: &mut ChildStdin, output: &mut JsonOutput, thread_id: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut request_id = 20;
    loop {
        send_json(
            input,
            json!({
                "jsonrpc": "2.0",
                "method": "trace/list",
                "id": request_id,
                "params": {"runId": thread_id}
            }),
        );
        let response = output.recv_id(request_id, Duration::from_secs(2));
        if response["result"]["events"]
            .as_array()
            .is_some_and(|events| {
                events.iter().any(|event| {
                    event["payload"]["observation"] == "sandbox_execution"
                        && event["span_phase"] == "start"
                })
            })
        {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "command never reached the sandbox execution boundary: {response}"
        );
        request_id += 1;
        thread::yield_now();
    }
}

fn shutdown_process(child: &mut Child, input: &mut ChildStdin, output: &mut JsonOutput, id: i64) {
    send_json(
        input,
        json!({"jsonrpc": "2.0", "method": "server/shutdown", "id": id, "params": {}}),
    );
    assert_eq!(
        output.recv_id(id, Duration::from_secs(5))["result"]["shutdown"],
        true
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = child.try_wait().expect("poll app-server") {
            assert!(status.success(), "app-server exited with {status}");
            return;
        }
        if Instant::now() >= deadline {
            kill_and_reap(child);
            panic!("app-server did not exit after shutdown");
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn kill_and_reap(child: &mut Child) {
    child.kill().expect("kill app-server");
    child.wait().expect("reap app-server");
}

fn send_json(input: &mut impl Write, message: Value) {
    writeln!(input, "{message}").expect("write app-server request");
    input.flush().expect("flush app-server request");
}

struct JsonOutput {
    receiver: Receiver<Value>,
    buffered: VecDeque<Value>,
}

impl JsonOutput {
    fn recv_next(&mut self, timeout: Duration) -> Value {
        self.buffered.pop_front().unwrap_or_else(|| {
            self.receiver
                .recv_timeout(timeout)
                .expect("app-server output message")
        })
    }

    fn recv_next_event(&mut self, timeout: Duration) -> Value {
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .expect("timed out waiting for app-server event");
            let message = self.recv_next(remaining);
            if message["method"] != "event/gap" {
                return message;
            }
        }
    }

    fn recv_id(&mut self, id: i64, timeout: Duration) -> Value {
        self.recv_where(timeout, |message| message["id"] == id)
    }

    fn recv_method(&mut self, method: &str, timeout: Duration) -> Value {
        self.recv_where(timeout, |message| message["method"] == method)
    }

    fn recv_where(&mut self, timeout: Duration, predicate: impl Fn(&Value) -> bool) -> Value {
        if let Some(index) = self.buffered.iter().position(&predicate) {
            return self.buffered.remove(index).expect("buffered message");
        }
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .expect("timed out waiting for app-server message");
            let message = self
                .receiver
                .recv_timeout(remaining)
                .expect("app-server output message");
            if predicate(&message) {
                return message;
            }
            self.buffered.push_back(message);
        }
    }
}

fn read_http_json(stream: &mut TcpStream) -> Value {
    let mut reader = BufReader::new(stream.try_clone().expect("clone provider stream"));
    let mut request_line = String::new();
    reader
        .read_line(&mut request_line)
        .expect("read provider request line");
    assert!(request_line.contains("/v1/responses"));
    let mut content_length = 0;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).expect("read provider header");
        if line == "\r\n" || line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            content_length = value.trim().parse().expect("provider content length");
        }
    }
    let mut body = vec![0; content_length];
    reader.read_exact(&mut body).expect("read provider body");
    serde_json::from_slice(&body).expect("provider request json")
}

fn capability_probe_response(request: &Value) -> Option<Value> {
    let tools = request.get("tools")?.as_array()?;
    let names = tools
        .iter()
        .filter_map(|tool| tool.get("name").and_then(Value::as_str))
        .collect::<Vec<_>>();
    if !names.contains(&"singularity_capability_probe_a") {
        return None;
    }
    let continuation = request
        .get("input")
        .and_then(Value::as_array)
        .is_some_and(|items| {
            items
                .iter()
                .any(|item| item["type"] == "function_call_output")
        });
    let strict = tools
        .iter()
        .any(|tool| tool.get("strict").and_then(Value::as_bool) == Some(true));
    let arguments = if strict {
        json!({"probe": "schema_sentinel_alpha", "values": [7, 7]})
    } else {
        json!({})
    };
    let mut output = vec![json!({
        "type": "function_call",
        "call_id": if continuation { "probe_call_continuation" } else { "probe_call_a" },
        "name": "singularity_capability_probe_a",
        "arguments": arguments.to_string()
    })];
    if !continuation
        && names.contains(&"singularity_capability_probe_b")
        && request["parallel_tool_calls"] == true
    {
        output.push(json!({
            "type": "function_call",
            "call_id": "probe_call_b",
            "name": "singularity_capability_probe_b",
            "arguments": arguments.to_string()
        }));
    }
    Some(json!({
        "id": if continuation {
            "capability_probe_continuation_response"
        } else {
            "capability_probe_response"
        },
        "object": "response",
        "status": "completed",
        "output": output,
        "usage": usage()
    }))
}

fn write_json_response(stream: &mut TcpStream, body: &Value) {
    let body = body.to_string();
    write!(
        stream,
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
    .expect("write provider response");
}

fn write_stream_response(stream: &mut TcpStream, response: Value) {
    write!(
        stream,
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\nevent: response.completed\ndata: {}\n\n",
        json!({"type": "response.completed", "response": response})
    )
    .expect("write provider stream response");
    stream.flush().expect("flush provider stream response");
}

fn write_final_stream_response(stream: &mut TcpStream, response: Value) {
    let final_text = "resumed and completed";
    write!(
        stream,
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\nevent: response.output_text.delta\ndata: {}\n\nevent: response.completed\ndata: {}\n\n",
        json!({"type": "response.output_text.delta", "delta": final_text}),
        json!({"type": "response.completed", "response": response})
    )
    .expect("write final provider stream response");
    stream
        .flush()
        .expect("flush final provider stream response");
}

fn app_server_bin() -> String {
    std::env::var("CARGO_BIN_EXE_singularity_app_server").unwrap_or_else(|_| {
        let mut path = Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf();
        path.pop();
        path.pop();
        path.push("target");
        path.push("debug");
        path.push(format!(
            "singularity_app_server{}",
            std::env::consts::EXE_SUFFIX
        ));
        path.to_string_lossy().to_string()
    })
}
