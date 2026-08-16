//! stdio transport-level concurrent steer/followUp test with a deterministic
//! fake provider. The same connection keeps a long turn in flight while the
//! client injects turn/steer and turn/followUp.

mod support;

use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Duration;

use serde_json::{Value, json};
use support::AppServerProcess;

/// 按脚本顺序服务固定数量 HTTP 请求的 fake provider。
///
/// 正常路径逐个 accept 并写响应；app-server 出错或测试提前结束时没有
/// 后续连接，worker 通过 stop 标志轮询有界退出，Drop 永不阻塞在 accept
/// 或 join 上。accepted 连接带读写超时：客户端连接后不发或只发半个
/// 请求时，worker 从阻塞读中有界脱出，不会把 Drop 卡死在 join 上。
/// worker 内部错误记录到 `errors` 保留原始原因，供测试断言。
struct ScriptedProvider {
    base_url: String,
    address: std::net::SocketAddr,
    served: Receiver<usize>,
    requests: Arc<Mutex<Vec<Value>>>,
    errors: Arc<Mutex<Vec<String>>>,
    worker: Option<thread::JoinHandle<()>>,
    stop: Arc<AtomicBool>,
}

/// accepted 连接的单次读写时限。
const STREAM_TIMEOUT: Duration = Duration::from_secs(5);

impl ScriptedProvider {
    fn start(responses: Vec<Value>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind provider");
        listener
            .set_nonblocking(true)
            .expect("provider listener nonblocking");
        let address = listener.local_addr().expect("provider address");
        let (served_tx, served_rx) = mpsc::channel();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let errors = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let worker_requests = Arc::clone(&requests);
        let worker_errors = Arc::clone(&errors);
        let worker_stop = Arc::clone(&stop);
        let worker = thread::spawn(move || {
            serve_script(
                listener,
                responses,
                served_tx,
                worker_requests,
                worker_errors,
                worker_stop,
            )
        });
        Self {
            base_url: format!("http://{address}/v1/responses"),
            address,
            served: served_rx,
            requests,
            errors,
            worker: Some(worker),
            stop,
        }
    }

    fn request(&self, index: usize) -> Value {
        self.requests.lock().expect("requests")[index].clone()
    }

    fn errors(&self) -> Vec<String> {
        self.errors.lock().expect("provider errors").clone()
    }
}

impl Drop for ScriptedProvider {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            // worker 只通过 stop 轮询退出；此处 join 是有界的（轮询间隔 5ms）。
            let _ = worker.join();
        }
    }
}

fn record_error(errors: &Mutex<Vec<String>>, context: String, error: impl std::fmt::Display) {
    errors
        .lock()
        .expect("provider errors")
        .push(format!("{context}: {error}"));
}

fn serve_script(
    listener: TcpListener,
    responses: Vec<Value>,
    served_tx: Sender<usize>,
    requests: Arc<Mutex<Vec<Value>>>,
    errors: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
) {
    for (index, completed) in responses.into_iter().enumerate() {
        let (mut stream, _) = loop {
            if stop.load(Ordering::SeqCst) {
                return;
            }
            match listener.accept() {
                Ok(accepted) => break accepted,
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) => {
                    record_error(&errors, format!("accept request {index}"), error);
                    return;
                }
            }
        };
        // 半个 header/body 后停住的客户端不能把 worker 永久卡在读/写里。
        if let Err(error) = stream.set_read_timeout(Some(STREAM_TIMEOUT)) {
            record_error(&errors, format!("set read timeout {index}"), error);
            return;
        }
        if let Err(error) = stream.set_write_timeout(Some(STREAM_TIMEOUT)) {
            record_error(&errors, format!("set write timeout {index}"), error);
            return;
        }
        let request = match read_http_json(&mut stream) {
            Ok(request) => request,
            Err(error) => {
                record_error(&errors, format!("read request {index}"), error);
                return;
            }
        };
        requests.lock().expect("requests").push(request);
        if let Err(error) = write_response_completed(&mut stream, completed) {
            record_error(&errors, format!("write response {index}"), error);
            return;
        }
        // 接收端已 drop 说明测试先结束，退出即可，不算 provider 错误。
        let _ = served_tx.send(index);
    }
}

fn steer_responses() -> Vec<Value> {
    vec![
        json!({
            "type": "response.completed",
            "response": {
                "id": "response_steer_0",
                "object": "response",
                "status": "completed",
                "output": [{
                    "type": "function_call",
                    "call_id": "call_steer_0",
                    "name": "bash",
                    "arguments": "{\"command\":\"sleep 2; echo tool-done\",\"timeout_ms\":10000}"
                }],
                "usage": usage_json()
            }
        }),
        json!({
            "type": "response.completed",
            "response": {
                "id": "response_steer_1",
                "object": "response",
                "status": "completed",
                "output": [{
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "first stop"}]
                }],
                "usage": usage_json()
            }
        }),
        json!({
            "type": "response.completed",
            "response": {
                "id": "response_steer_2",
                "object": "response",
                "status": "completed",
                "output": [{
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "follow-up stop"}]
                }],
                "usage": usage_json()
            }
        }),
    ]
}

fn interrupt_responses() -> Vec<Value> {
    vec![json!({
        "type": "response.completed",
        "response": {
            "id": "response_interrupt_0",
            "object": "response",
            "status": "completed",
            "output": [{
                "type": "function_call",
                "call_id": "call_interrupt_0",
                "name": "bash",
                "arguments": "{\"command\":\"sleep 30\",\"timeout_ms\":600000}"
            }],
            "usage": usage_json()
        }
    })]
}

fn usage_json() -> Value {
    json!({
        "input_tokens": 3,
        "output_tokens": 2,
        "total_tokens": 5,
        "input_tokens_details": {"cached_tokens": 0},
        "output_tokens_details": {"reasoning_tokens": 0}
    })
}

fn write_response_completed(
    stream: &mut TcpStream,
    completed: Value,
) -> Result<(), std::io::Error> {
    write!(
        stream,
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\nevent: response.output_text.delta\r\ndata: {{\"type\":\"response.output_text.delta\",\"delta\":\"thinking \"}}\n\n"
    )?;
    stream.flush()?;
    write!(
        stream,
        "event: response.completed\r\ndata: {completed}\r\n\r\n"
    )?;
    stream.flush()?;
    Ok(())
}

fn read_http_json(stream: &mut TcpStream) -> Result<Value, std::io::Error> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    if !request_line.contains("/v1/responses") {
        return Err(std::io::Error::other(format!(
            "unexpected request line: {request_line}"
        )));
    }
    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line)?;
        if line == "\r\n" || line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            content_length = value.trim().parse().map_err(|error| {
                std::io::Error::other(format!("invalid content length: {error}"))
            })?;
        }
    }
    let mut body = vec![0u8; content_length];
    reader.read_exact(&mut body)?;
    serde_json::from_slice(&body)
        .map_err(|error| std::io::Error::other(format!("request json parse failed: {error}")))
}

fn spawn(workspace: &Path, home: &Path, base_url: &str) -> AppServerProcess {
    AppServerProcess::spawn(workspace, home, base_url)
}

/// drop 必须在没有收到任何连接时有界返回；否则失败路径上的测试会挂在
/// provider worker 的 accept/join 上，而不是报告真实原因。
fn assert_drop_is_bounded(provider: ScriptedProvider) {
    let (done_tx, done_rx) = mpsc::channel();
    thread::spawn(move || {
        drop(provider);
        let _ = done_tx.send(());
    });
    done_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("ScriptedProvider::drop must return without any connection");
}

#[test]
fn scripted_provider_drop_returns_without_any_connection() {
    assert_drop_is_bounded(ScriptedProvider::start(steer_responses()));
}

/// 回归：客户端连接后只发送半个请求并停住；stop 标志无法中断阻塞读，
/// worker 必须靠读写超时有界退出，Drop 的 join 不得永久等待。
#[test]
fn scripted_provider_drop_returns_after_half_request() {
    let provider = ScriptedProvider::start(steer_responses());
    let mut stream = TcpStream::connect(provider.address).expect("connect half request");
    stream
        .write_all(b"POST /v1/responses HTTP/1.1\r\ncontent-le")
        .expect("half header");
    stream.flush().expect("flush half header");
    // 连接保持打开但不完成 header/body。
    assert_drop_is_bounded(provider);
}

#[test]
fn scripted_provider_exits_bounded_when_app_server_fails_before_connecting() {
    let dir = tempfile::tempdir().expect("temp dir");
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let home = dir.path().join("home");
    let provider = ScriptedProvider::start(steer_responses());
    let mut process = spawn(&workspace, &home, &provider.base_url);
    process.initialize();

    // 未知 thread：turn/start 在连接 provider 之前失败。
    process.send_request(
        3,
        "turn/start",
        json!({"threadId": "missing-thread", "input": [{"type": "text", "text": "never runs"}]}),
    );
    let error = process.output.recv_id(3, Duration::from_secs(5));
    assert_eq!(
        error["error"]["code"], -32004,
        "turn must fail not found: {error}"
    );
    assert_drop_is_bounded(provider);
    process.shutdown();
}

#[test]
fn same_stdio_connection_interrupts_running_tool_turn() {
    let dir = tempfile::tempdir().expect("temp dir");
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let home = dir.path().join("home");
    let provider = ScriptedProvider::start(interrupt_responses());
    let mut process = spawn(&workspace, &home, &provider.base_url);
    process.initialize();

    process.send_request(3, "thread/start", json!({"cwd": workspace}));
    let started = process.output.recv_id(3, Duration::from_secs(5));
    let session_id = started["result"]["thread"]["thread_id"]
        .as_str()
        .expect("session id")
        .to_string();
    process.send_request(
        4,
        "turn/start",
        json!({"threadId": session_id, "input": [{"type": "text", "text": "run long tool"}]}),
    );
    provider
        .served
        .recv_timeout(Duration::from_secs(5))
        .expect("request served");
    let turn_id = process
        .output
        .recv_where(Duration::from_secs(5), |message| {
            message["method"] == "turn/started"
        })["params"]["turn"]["turn_id"]
        .as_str()
        .expect("turn id")
        .to_string();
    thread::sleep(Duration::from_millis(200));
    process.send_request(5, "turn/interrupt", json!({"turnId": turn_id}));
    let interrupt = process.output.recv_id(5, Duration::from_secs(5));
    assert_eq!(interrupt["result"]["status"], "cancel_requested");
    let turn_response = process.output.recv_id(4, Duration::from_secs(30));
    assert_eq!(turn_response["result"]["turn"]["status"], "interrupted");
    assert_eq!(
        turn_response["result"]["turn"]["agent_loop_status"],
        "cancelled"
    );
    assert!(
        provider.errors().is_empty(),
        "provider worker must be error-free: {:?}",
        provider.errors()
    );
    process.shutdown();
}

#[test]
fn same_stdio_connection_steers_and_follows_up_during_one_turn() {
    let dir = tempfile::tempdir().expect("temp dir");
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let home = dir.path().join("home");
    let provider = ScriptedProvider::start(steer_responses());
    let mut process = spawn(&workspace, &home, &provider.base_url);
    process.initialize();

    process.send_request(3, "thread/start", json!({"cwd": workspace}));
    let started = process.output.recv_id(3, Duration::from_secs(5));
    let session_id = started["result"]["thread"]["thread_id"]
        .as_str()
        .expect("session id")
        .to_string();

    // 同一连接上：turn/start 仍在执行时发送 steer 与 followUp。
    process.send_request(
        4,
        "turn/start",
        json!({"threadId": session_id, "input": [{"type": "text", "text": "initial"}]}),
    );
    assert_eq!(
        provider
            .served
            .recv_timeout(Duration::from_secs(5))
            .expect("first request served"),
        0
    );
    thread::sleep(Duration::from_millis(200));
    process.send_request(
        5,
        "turn/steer",
        json!({"turnId": "unknown-will-be-replaced", "input": [{"type": "text", "text": "ignored"}]}),
    );
    // 用事件里的真实 turn id 重发：从 turn/started 通知中读取。
    let turn_id = process
        .output
        .recv_where(Duration::from_secs(5), |message| {
            message["method"] == "turn/started"
        })["params"]["turn"]["turn_id"]
        .as_str()
        .expect("turn id")
        .to_string();
    process.send_request(
        6,
        "turn/steer",
        json!({"turnId": turn_id, "input": [{"type": "text", "text": "steer after tools"}]}),
    );
    process.send_request(
        7,
        "turn/followUp",
        json!({"turnId": turn_id, "input": [{"type": "text", "text": "one more round"}]}),
    );

    let turn_response = process.output.recv_id(4, Duration::from_secs(30));
    assert_eq!(turn_response["result"]["turn"]["status"], "completed");
    assert_eq!(
        provider
            .served
            .recv_timeout(Duration::from_secs(5))
            .expect("second request"),
        1
    );
    assert_eq!(
        provider
            .served
            .recv_timeout(Duration::from_secs(5))
            .expect("third request"),
        2
    );

    let second = provider.request(1);
    let second_inputs = second["input"].as_array().expect("second input");
    assert!(
        second_inputs
            .iter()
            .any(|item| item["role"] == "user"
                && item["content"].as_str() == Some("steer after tools")),
        "steer must appear before second model request: {second_inputs:?}"
    );
    let tool_result_pos = second_inputs
        .iter()
        .position(|item| item["type"].as_str() == Some("function_call_output"))
        .unwrap_or_else(|| panic!("tool result in second request: {second_inputs:?}"));
    let steer_pos = second_inputs
        .iter()
        .position(|item| {
            item["role"] == "user" && item["content"].as_str() == Some("steer after tools")
        })
        .expect("steer in second request");
    assert!(
        tool_result_pos < steer_pos,
        "steer must be after the completed tool batch"
    );
    assert!(
        !second_inputs.iter().any(
            |item| item["role"] == "user" && item["content"].as_str() == Some("one more round")
        ),
        "followUp must not leak into the steer request"
    );

    let third = provider.request(2);
    assert!(
        third["input"]
            .as_array()
            .expect("third input")
            .iter()
            .any(|item| {
                item["role"] == "user" && item["content"].as_str() == Some("one more round")
            }),
        "followUp must trigger an extra round: {third}"
    );

    // turn 结束后同方法必须返回 not found，而不是接受后丢弃。
    process.send_request(
        8,
        "turn/steer",
        json!({"turnId": turn_id, "input": [{"type": "text", "text": "late"}]}),
    );
    let late = process.output.recv_id(8, Duration::from_secs(5));
    assert_eq!(
        late["error"]["code"], -32004,
        "late steer must be not found: {late}"
    );

    assert!(
        provider.errors().is_empty(),
        "provider worker must be error-free: {:?}",
        provider.errors()
    );
    process.shutdown();
}
