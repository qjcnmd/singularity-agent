#![cfg(windows)]

mod support;

use serde_json::{Value, json};
use singularity_protocol::TurnStatus;
use singularity_store::SessionStore;
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use support::app_server_bin;

fn create_workspace(root: &Path) -> std::path::PathBuf {
    let workspace = root.join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    workspace
}

struct ControlledProvider {
    base_url: String,
    stop: Arc<AtomicBool>,
    address: std::net::SocketAddr,
    worker: Mutex<Option<thread::JoinHandle<()>>>,
}

impl ControlledProvider {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind provider");
        let address = listener.local_addr().expect("provider address");
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
                // 仅响应 capability probe；保留测试不产生 production 请求。
                if let Some(response) = capability_probe_response(&request) {
                    write_json_response(&mut stream, &response);
                }
            }
        });
        Self {
            base_url: format!("http://{address}/v1/responses"),
            stop,
            address,
            worker: Mutex::new(Some(worker)),
        }
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
                .unwrap_or_else(|error| {
                    panic!(
                        "app-server output message: {error}; buffered messages: {:?}",
                        self.buffered
                    )
                });
            if predicate(&message) {
                return message;
            }
            self.buffered.push_back(message);
        }
    }
}

// Issue #24 批次 A（A3）：thread/start 未显式指定 model 时，Thread.model 冻结为
// 当前配置可无歧义解析的默认 selector（legacy 为裸 model id），防止重启后默认
// 配置变化静默切换模型；provider 未配置时仍保留 NULL 契约（由现有测试覆盖）。
#[test]
fn thread_start_freezes_resolved_default_selector_when_model_is_omitted() {
    let dir = tempfile::tempdir().expect("temp dir");
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let db_path = dir.path().join("sessions.sqlite3");
    let provider = ControlledProvider::start();
    let mut process = Process::spawn(&db_path, &workspace, &provider.base_url);
    process.initialize();
    process.send_request(
        2,
        "thread/start",
        json!({
            "cwd": workspace
        }),
    );
    let response = process.output.recv_id(2, Duration::from_secs(5));
    assert_eq!(
        response["result"]["thread"]["model"], "gpt-test",
        "legacy default model must be frozen into Thread.model: {response}"
    );
    process.shutdown(3);
}

// 启动恢复 E2E：预置一个 ownerless running turn 与一个 suspended turn，同一
// App Server 启动成功后 running turn 收敛为 interrupted（owner 丢失），
// suspended turn 保持可恢复（刻意无 owner，turn/resume 可继续），不阻断启动。
#[test]
fn app_server_startup_recovers_ownerless_running_turn_only() {
    let provider = ControlledProvider::start();
    let dir = tempfile::tempdir().expect("temp dir");
    let workspace = create_workspace(dir.path());
    let db_path = dir.path().join("sessions.sqlite3");

    let store = SessionStore::open(&db_path).expect("preload store");
    let running_thread = store.create_thread(None, None).expect("running thread");
    let running_turn = store
        .create_turn(&running_thread.thread_id, "running")
        .expect("running turn");
    let suspended_thread = store.create_thread(None, None).expect("suspended thread");
    let suspended_turn = store
        .create_turn(&suspended_thread.thread_id, "running")
        .expect("suspended turn");
    store
        .update_turn_state(&suspended_turn.turn_id, TurnStatus::Suspended, "suspended")
        .expect("suspend turn");
    drop(store);

    let mut process = Process::spawn(&db_path, &workspace, &provider.base_url);
    process.initialize();

    process.send_request(2, "turn/status", json!({"turnId": running_turn.turn_id}));
    let running_response = process.output.recv_id(2, Duration::from_secs(5));
    assert_eq!(
        running_response["result"]["turn"]["status"], "interrupted",
        "ownerless running turn must be terminalized during startup: {running_response}"
    );
    assert_eq!(
        running_response["result"]["turn"]["agent_loop_status"],
        "interrupted"
    );

    process.send_request(3, "turn/status", json!({"turnId": suspended_turn.turn_id}));
    let suspended_response = process.output.recv_id(3, Duration::from_secs(5));
    assert_eq!(
        suspended_response["result"]["turn"]["status"], "suspended",
        "suspended turn must remain resumable: {suspended_response}"
    );
    assert_eq!(
        suspended_response["result"]["turn"]["agent_loop_status"],
        "suspended"
    );

    process.shutdown(4);
}

// 非终态 turn 错误映射：thread/archive 与 turn/start 的 JSON-RPC error message
// 携带已确认的 turn ID 与可操作提示（保留 error code，不新增协议字段）。
// suspended 是刻意无 owner 且可 resumable 的状态，启动恢复不会终态化它，
// 因此可用来稳定触发 nonterminal-turn 错误。
#[test]
fn nonterminal_turn_errors_carry_turn_id_for_actionable_cli_hint() {
    let provider = ControlledProvider::start();
    let dir = tempfile::tempdir().expect("temp dir");
    let workspace = create_workspace(dir.path());
    let db_path = dir.path().join("sessions.sqlite3");

    let store = SessionStore::open(&db_path).expect("preload store");
    let thread = store
        .create_thread(None, Some(&workspace.to_string_lossy()))
        .expect("thread");
    let turn = store
        .create_turn(&thread.thread_id, "running")
        .expect("running turn");
    store
        .update_turn_state(&turn.turn_id, TurnStatus::Suspended, "suspended")
        .expect("suspend turn");
    drop(store);

    let mut process = Process::spawn(&db_path, &workspace, &provider.base_url);
    process.initialize();

    // thread/archive 触发 ThreadHasNonterminalTurn：消息含 turn ID 与操作提示。
    process.send_request(2, "thread/archive", json!({ "threadId": thread.thread_id }));
    let archive_error = process.expect_error(2);
    let archive_message = archive_error["error"]["message"]
        .as_str()
        .expect("archive error message");
    assert!(
        archive_message.contains(&turn.turn_id),
        "archive message must carry turn id: {archive_message}"
    );
    assert!(
        archive_message.contains("use sg turn resume/pause/input"),
        "archive message must be actionable: {archive_message}"
    );

    // turn/start 触发 WorkspaceHasNonterminalTurn：同一 thread 已有非终态 turn。
    process.send_request(
        3,
        "turn/start",
        json!({
            "threadId": thread.thread_id,
            "input": [{"type": "text", "text": "next task"}],
        }),
    );
    let start_error = process.expect_error(3);
    let start_message = start_error["error"]["message"]
        .as_str()
        .expect("start error message");
    assert!(
        start_message.contains(&turn.turn_id),
        "turn/start message must carry turn id: {start_message}"
    );
    assert!(
        start_message.contains("use sg turn resume/pause/input"),
        "turn/start message must be actionable: {start_message}"
    );

    process.shutdown(4);
}
