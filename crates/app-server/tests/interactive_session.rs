#![cfg(windows)]

use serde_json::{Value, json};
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const ORIGINAL_INPUT: &str = "Complete this turn interactively";
const FIRST_FOLLOW_UP: &str = "First follow-up";
const SECOND_FOLLOW_UP: &str = "Second follow-up";
const STEERING_INPUT: &str = "Do not execute the pending action; explain instead";
const PLAN_CALL_ID: &str = "interactive_plan_call";
const COMMAND_CALL_ID: &str = "interactive_command_call";

#[test]
fn follow_up_is_consumed_once_in_order_before_same_turn_finalizes_and_terminal_input_is_rejected() {
    let provider = ControlledProvider::start();
    let dir = tempfile::tempdir().expect("temp dir");
    let workspace = create_workspace(dir.path());
    let db_path = dir.path().join("sessions.sqlite3");
    let mut process = Process::spawn(&db_path, &workspace, &provider.base_url);
    process.initialize();
    let thread_id = process.start_thread(&workspace, "read-only", "never", 2);
    let turn_id = process.start_turn(&thread_id, ORIGINAL_INPUT, 3);
    provider
        .next_request()
        .complete(replan_response("initial_completed_plan"));
    let first_request = provider.next_request();
    assert_eq!(user_texts(&first_request.request), vec![ORIGINAL_INPUT]);

    process.send_input("follow-up-1", &turn_id, "follow_up", FIRST_FOLLOW_UP, 4);
    process.expect_ok(4);
    process.send_input("follow-up-1", &turn_id, "follow_up", FIRST_FOLLOW_UP, 5);
    process.expect_ok(5);
    process.send_input("follow-up-2", &turn_id, "follow_up", SECOND_FOLLOW_UP, 6);
    process.expect_ok(6);
    first_request.complete(final_response("obsolete first answer"));

    let follow_up_request = provider.next_request();
    assert_eq!(
        user_texts(&follow_up_request.request),
        vec![ORIGINAL_INPUT, FIRST_FOLLOW_UP, SECOND_FOLLOW_UP],
        "follow-up inputIds must be idempotent and distinct inputs must retain arrival order"
    );
    follow_up_request.complete(replan_response("follow_up_replan"));
    provider
        .next_request()
        .complete(final_response("answer including both follow-ups"));
    let completed = process.output.recv_id(3, Duration::from_secs(10));
    assert_eq!(completed["result"]["turn"]["turn_id"], turn_id);
    assert_eq!(completed["result"]["turn"]["status"], "completed");
    let requests_before_terminal_input = provider.production_requests.load(Ordering::SeqCst);

    process.send_input(
        "after-terminal",
        &turn_id,
        "follow_up",
        "must be rejected",
        7,
    );
    process.expect_error(7);
    assert_eq!(
        provider.production_requests.load(Ordering::SeqCst),
        requests_before_terminal_input,
        "terminal input must not reach the provider"
    );
    process.shutdown(8);
}

#[test]
fn steer_waits_for_the_next_safe_boundary_and_invalidates_a_pending_action() {
    let provider = ControlledProvider::start();
    let dir = tempfile::tempdir().expect("temp dir");
    let workspace = create_workspace(dir.path());
    let stale_action = workspace.join("stale-action.txt");
    let db_path = dir.path().join("sessions.sqlite3");
    let mut process = Process::spawn(&db_path, &workspace, &provider.base_url);
    process.initialize();
    let thread_id = process.start_thread(&workspace, "workspace-write", "never", 2);
    let turn_id = process.start_turn(&thread_id, ORIGINAL_INPUT, 3);

    provider.next_request().complete(plan_response());
    let already_issued_request = provider.next_request();
    assert!(
        !user_texts(&already_issued_request.request).contains(&STEERING_INPUT),
        "an already-issued ModelTurnRequest is immutable"
    );
    process.send_input("steer-1", &turn_id, "steer", STEERING_INPUT, 4);
    process.expect_ok(4);
    already_issued_request.complete(stale_command_response());

    let steered_request = provider.next_request();
    assert_eq!(
        user_texts(&steered_request.request),
        vec![ORIGINAL_INPUT, STEERING_INPUT]
    );
    steered_request.complete(replan_response("steer_replan"));
    provider
        .next_request()
        .complete(final_response("steered answer"));
    let completed = process.output.recv_id(3, Duration::from_secs(10));
    assert_eq!(completed["result"]["turn"]["status"], "completed");
    assert!(
        !stale_action.exists(),
        "a tool action returned after steer was queued must not execute"
    );
    process.shutdown(5);
}

#[test]
fn pause_is_distinct_and_resume_consumes_queued_user_messages_without_synthetic_continue() {
    let provider = ControlledProvider::start();
    let dir = tempfile::tempdir().expect("temp dir");
    let workspace = create_workspace(dir.path());
    let db_path = dir.path().join("sessions.sqlite3");
    let mut process = Process::spawn(&db_path, &workspace, &provider.base_url);
    process.initialize();
    let thread_id = process.start_thread(&workspace, "read-only", "never", 2);
    let turn_id = process.start_turn(&thread_id, ORIGINAL_INPUT, 3);
    provider
        .next_request()
        .complete(replan_response("pause_initial_plan"));
    let in_flight_request = provider.next_request();

    process.send_request(4, "turn/pause", json!({"turnId": turn_id}));
    process.expect_ok(4);
    in_flight_request.complete(final_response("must not finalize while paused"));
    let paused = wait_for_turn_status(&mut process, &turn_id, "paused", 5);
    assert_eq!(paused["result"]["turn"]["status"], "paused");
    assert_ne!(paused["result"]["turn"]["status"], "interrupted");
    assert_ne!(paused["result"]["turn"]["status"], "suspended");
    let paused_start = process.output.recv_id(3, Duration::from_secs(10));
    assert_eq!(paused_start["result"]["turn"]["status"], "paused");

    process.send_input(
        "paused-steer-1",
        &turn_id,
        "steer",
        "first paused update",
        6,
    );
    process.expect_ok(6);
    process.send_input(
        "paused-steer-2",
        &turn_id,
        "steer",
        "second paused update",
        7,
    );
    process.expect_ok(7);
    process.send_request(8, "turn/resume", json!({"turnId": turn_id}));
    let resumed_request = provider.next_request();
    assert_eq!(
        user_texts(&resumed_request.request),
        vec![
            ORIGINAL_INPUT,
            "first paused update",
            "second paused update"
        ],
        "turn/resume must preserve queued user order without synthesizing continue"
    );
    resumed_request.complete(replan_response("pause_replan"));
    provider
        .next_request()
        .complete(final_response("resumed answer"));
    let resumed = process.output.recv_id(8, Duration::from_secs(10));
    assert_eq!(resumed["result"]["turn"]["turn_id"], turn_id);
    assert_eq!(resumed["result"]["turn"]["status"], "completed");
    process.shutdown(9);
}

#[test]
fn queued_steer_survives_process_restart_without_mutating_the_issued_model_request() {
    let provider = ControlledProvider::start();
    let dir = tempfile::tempdir().expect("temp dir");
    let workspace = create_workspace(dir.path());
    let db_path = dir.path().join("sessions.sqlite3");
    let mut first_process = Process::spawn(&db_path, &workspace, &provider.base_url);
    first_process.initialize();
    let thread_id = first_process.start_thread(&workspace, "read-only", "never", 2);
    let turn_id = first_process.start_turn(&thread_id, ORIGINAL_INPUT, 3);
    let issued = provider.next_request();

    first_process.send_input("durable-steer", &turn_id, "steer", STEERING_INPUT, 4);
    first_process.expect_ok(4);
    first_process.kill();
    issued.complete(final_response("connection was killed"));

    let mut resumed_process = Process::spawn(&db_path, &workspace, &provider.base_url);
    resumed_process.initialize();
    resumed_process.send_request(5, "turn/resume", json!({"turnId": turn_id}));
    let replayed = provider.next_request();
    assert_eq!(
        user_texts(&replayed.request),
        vec![ORIGINAL_INPUT, STEERING_INPUT],
        "checkpoint recovery must consume durable steer before the next new ModelTurnRequest"
    );
    replayed.complete(replan_response("restart_replan"));
    provider
        .next_request()
        .complete(final_response("completed after durable steer"));
    let completed = resumed_process.output.recv_id(5, Duration::from_secs(10));
    assert_eq!(completed["result"]["turn"]["status"], "completed");
    resumed_process.shutdown(6);
}

#[test]
fn input_queued_during_a_tool_is_not_consumed_or_retried_after_unknown_recovery() {
    let provider = ControlledProvider::start();
    let dir = tempfile::tempdir().expect("temp dir");
    let workspace = create_workspace(dir.path());
    let db_path = dir.path().join("sessions.sqlite3");
    let mut first_process = Process::spawn(&db_path, &workspace, &provider.base_url);
    first_process.initialize();
    let thread_id = first_process.start_thread(&workspace, "read-only", "never", 2);
    let turn_id = first_process.start_turn(&thread_id, "Run the controlled command", 3);
    provider.next_request().complete(long_command_response());
    wait_for_running_tool_trace(&mut first_process, &thread_id);

    first_process.send_input(
        "during-tool",
        &turn_id,
        "follow_up",
        "Only consume this after a known tool outcome",
        4,
    );
    first_process.expect_ok(4);
    first_process.kill();

    let mut resumed_process = Process::spawn(&db_path, &workspace, &provider.base_url);
    resumed_process.initialize();
    resumed_process.send_request(5, "turn/resume", json!({"turnId": turn_id}));
    resumed_process.expect_error(5);
    assert!(
        matches!(
            provider.requests.recv_timeout(Duration::from_millis(500)),
            Err(RecvTimeoutError::Timeout)
        ),
        "Unknown tool execution must not retry the tool or consume queued input into a model request"
    );
    assert_eq!(
        provider.production_requests.load(Ordering::SeqCst),
        1,
        "only the request that produced the original tool call may reach the provider"
    );
    resumed_process.shutdown(6);
}

fn create_workspace(root: &Path) -> std::path::PathBuf {
    let workspace = root.join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    workspace
}

struct ProviderRequest {
    request: Value,
    response: Sender<Value>,
}

impl ProviderRequest {
    fn complete(self, response: Value) {
        self.response
            .send(response)
            .expect("complete provider request");
    }
}

struct ControlledProvider {
    base_url: String,
    requests: Receiver<ProviderRequest>,
    production_requests: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    address: std::net::SocketAddr,
    worker: Mutex<Option<thread::JoinHandle<()>>>,
}

impl ControlledProvider {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind provider");
        let address = listener.local_addr().expect("provider address");
        let (request_tx, request_rx) = mpsc::channel();
        let production_requests = Arc::new(AtomicUsize::new(0));
        let worker_count = Arc::clone(&production_requests);
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let worker = thread::spawn(move || {
            while !worker_stop.load(Ordering::SeqCst) {
                let (mut stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(_) if worker_stop.load(Ordering::SeqCst) => break,
                    Err(error) => panic!("accept provider request: {error}"),
                };
                if worker_stop.load(Ordering::SeqCst) {
                    break;
                }
                let request = read_http_json(&mut stream);
                if let Some(response) = capability_probe_response(&request) {
                    write_json_response(&mut stream, &response);
                    continue;
                }
                worker_count.fetch_add(1, Ordering::SeqCst);
                let (response_tx, response_rx) = mpsc::channel();
                if request_tx
                    .send(ProviderRequest {
                        request,
                        response: response_tx,
                    })
                    .is_err()
                {
                    break;
                }
                loop {
                    match response_rx.recv_timeout(Duration::from_millis(50)) {
                        Ok(response) => {
                            write_stream_response(&mut stream, response);
                            break;
                        }
                        Err(RecvTimeoutError::Timeout) if !worker_stop.load(Ordering::SeqCst) => {}
                        Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => break,
                    }
                }
            }
        });
        Self {
            base_url: format!("http://{address}/v1/responses"),
            requests: request_rx,
            production_requests,
            stop,
            address,
            worker: Mutex::new(Some(worker)),
        }
    }

    fn next_request(&self) -> ProviderRequest {
        self.requests
            .recv_timeout(Duration::from_secs(10))
            .expect("production provider request")
    }
}

impl Drop for ControlledProvider {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.address);
        if let Some(worker) = self.worker.lock().expect("provider worker lock").take() {
            worker.join().expect("provider worker");
        }
    }
}

struct Process {
    child: Child,
    input: ChildStdin,
    output: JsonOutput,
}

impl Process {
    fn spawn(db_path: &Path, workspace: &Path, base_url: &str) -> Self {
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
        Self {
            child,
            input,
            output: JsonOutput {
                receiver,
                buffered: VecDeque::new(),
            },
        }
    }

    fn initialize(&mut self) {
        self.send_request(
            1,
            "initialize",
            json!({
                "clientInfo": {
                    "name": "interactive-session-test",
                    "title": "Interactive Session Test",
                    "version": "0.1.0"
                }
            }),
        );
        self.expect_ok(1);
        send_json(
            &mut self.input,
            json!({"jsonrpc": "2.0", "method": "initialized", "params": {}}),
        );
        self.send_request(
            99,
            "event/subscribe",
            json!({"eventTypes": ["turn/started", "turn/completed"]}),
        );
        self.expect_ok(99);
    }

    fn start_thread(
        &mut self,
        workspace: &Path,
        sandbox_mode: &str,
        approval_policy: &str,
        id: i64,
    ) -> String {
        self.send_request(
            id,
            "thread/start",
            json!({
                "model": "gpt-test",
                "cwd": workspace,
                "sandboxMode": sandbox_mode,
                "approvalPolicy": approval_policy
            }),
        );
        let response = self.output.recv_id(id, Duration::from_secs(5));
        response["result"]["thread"]["thread_id"]
            .as_str()
            .expect("thread id")
            .to_string()
    }

    fn start_turn(&mut self, thread_id: &str, text: &str, id: i64) -> String {
        self.send_request(
            id,
            "turn/start",
            json!({
                "threadId": thread_id,
                "input": [{"type": "text", "text": text}]
            }),
        );
        self.output
            .recv_method("turn/started", Duration::from_secs(5))["params"]["turn"]["turn_id"]
            .as_str()
            .expect("turn id")
            .to_string()
    }

    fn send_input(
        &mut self,
        input_id: &str,
        turn_id: &str,
        delivery: &str,
        text: &str,
        request_id: i64,
    ) {
        self.send_request(
            request_id,
            "turn/input",
            json!({
                "inputId": input_id,
                "turnId": turn_id,
                "delivery": delivery,
                "input": [{"type": "text", "text": text}]
            }),
        );
    }

    fn send_request(&mut self, id: i64, method: &str, params: Value) {
        send_json(
            &mut self.input,
            json!({"jsonrpc": "2.0", "method": method, "id": id, "params": params}),
        );
    }

    fn expect_ok(&mut self, id: i64) -> Value {
        let response = self.output.recv_id(id, Duration::from_secs(5));
        assert!(
            response.get("result").is_some() && response.get("error").is_none(),
            "request {id} failed: {response}"
        );
        response
    }

    fn expect_error(&mut self, id: i64) -> Value {
        let response = self.output.recv_id(id, Duration::from_secs(5));
        assert!(
            response.get("error").is_some(),
            "request {id} unexpectedly succeeded: {response}"
        );
        response
    }

    fn kill(&mut self) {
        if self.child.try_wait().expect("poll app-server").is_none() {
            self.child.kill().expect("kill app-server");
            self.child.wait().expect("reap app-server");
        }
    }

    fn shutdown(&mut self, id: i64) {
        self.send_request(id, "server/shutdown", json!({}));
        assert_eq!(self.expect_ok(id)["result"]["shutdown"], true);
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = self.child.try_wait().expect("poll app-server") {
                assert!(status.success(), "app-server exited with {status}");
                return;
            }
            if Instant::now() >= deadline {
                self.kill();
                panic!("app-server did not exit after shutdown");
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        self.kill();
    }
}

fn wait_for_running_tool_trace(process: &mut Process, thread_id: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut request_id = 20;
    loop {
        process.send_request(request_id, "trace/list", json!({"runId": thread_id}));
        let response = process.output.recv_id(request_id, Duration::from_secs(2));
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

fn wait_for_turn_status(
    process: &mut Process,
    turn_id: &str,
    expected: &str,
    mut request_id: i64,
) -> Value {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        process.send_request(request_id, "turn/status", json!({"turnId": turn_id}));
        let response = process.output.recv_id(request_id, Duration::from_secs(2));
        if response["result"]["turn"]["status"] == expected {
            return response;
        }
        assert!(
            Instant::now() < deadline,
            "turn did not reach {expected}: {response}"
        );
        request_id += 100;
        thread::yield_now();
    }
}

fn user_texts(request: &Value) -> Vec<&str> {
    request["input"]
        .as_array()
        .expect("Responses input array")
        .iter()
        .filter(|item| item["type"] == "message" && item["role"] == "user")
        .filter_map(|item| {
            item["content"].as_str().or_else(|| {
                item["content"].as_array().and_then(|content| {
                    content.iter().find_map(|part| {
                        (part["type"] == "input_text")
                            .then(|| part["text"].as_str())
                            .flatten()
                    })
                })
            })
        })
        .collect()
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
                "steps": [{"step": "execute the pending action", "status": "pending"}]
            }).to_string()
        }],
        "usage": usage()
    })
}

fn replan_response(call_id: &str) -> Value {
    json!({
        "id": "response_plan",
        "object": "response",
        "status": "completed",
        "output": [{
            "type": "function_call",
            "call_id": call_id,
            "name": "update_plan",
            "arguments": json!({
                "steps": [{"step": "address the latest user input", "status": "completed"}]
            }).to_string()
        }],
        "usage": usage()
    })
}

fn stale_command_response() -> Value {
    json!({
        "id": "response_stale_command",
        "object": "response",
        "status": "completed",
        "output": [{
            "type": "function_call",
            "call_id": COMMAND_CALL_ID,
            "name": "command",
            "arguments": json!({
                "command": "pwsh -NoProfile -Command \"Set-Content -LiteralPath 'stale-action.txt' -Value stale\"",
                "cwd": ".",
                "timeout_seconds": 30
            }).to_string()
        }],
        "usage": usage()
    })
}

fn long_command_response() -> Value {
    json!({
        "id": "response_long_command",
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

fn final_response(text: &str) -> Value {
    json!({
        "id": "response_final",
        "object": "response",
        "status": "completed",
        "output": [{
            "type": "message",
            "role": "assistant",
            "content": [{"type": "output_text", "text": text}]
        }],
        "usage": usage()
    })
}

fn usage() -> Value {
    json!({
        "input_tokens": 1,
        "input_tokens_details": {"cached_tokens": 0},
        "output_tokens": 1,
        "output_tokens_details": {"reasoning_tokens": 0}
    })
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
    let _ = write!(
        stream,
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
}

fn write_stream_response(stream: &mut TcpStream, response: Value) {
    let text = response["output"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|item| item["content"].as_array().into_iter().flatten())
        .filter(|part| part["type"] == "output_text")
        .filter_map(|part| part["text"].as_str())
        .collect::<String>();
    if !text.is_empty() {
        let _ = write!(
            stream,
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\nevent: response.output_text.delta\ndata: {}\n\nevent: response.completed\ndata: {}\n\n",
            json!({"type": "response.output_text.delta", "delta": text}),
            json!({"type": "response.completed", "response": response})
        );
        let _ = stream.flush();
        return;
    }
    let _ = write!(
        stream,
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\nevent: response.completed\ndata: {}\n\n",
        json!({"type": "response.completed", "response": response})
    );
    let _ = stream.flush();
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
