//! stdio transport-level concurrent steer/followUp test with a deterministic
//! fake provider. The same connection keeps a long turn in flight while the
//! client injects turn/steer and turn/followUp.

mod support;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use serde_json::{Value, json};
use support::AppServerProcess;

struct SteerProvider {
    base_url: String,
    served: Receiver<usize>,
    requests: Arc<Mutex<Vec<Value>>>,
    worker: Option<thread::JoinHandle<()>>,
}

impl SteerProvider {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind provider");
        let address = listener.local_addr().expect("provider address");
        let (served_tx, served_rx) = mpsc::channel();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let worker_requests = Arc::clone(&requests);
        let worker = thread::spawn(move || {
            for index in 0..3 {
                let (mut stream, _) = listener.accept().expect("accept provider request");
                let request = read_http_json(&mut stream);
                worker_requests
                    .lock()
                    .expect("requests")
                    .push(request.clone());
                match index {
                    0 => write_response_completed(
                        &mut stream,
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
                    ),
                    1 => write_response_completed(
                        &mut stream,
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
                    ),
                    _ => write_response_completed(
                        &mut stream,
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
                    ),
                }
                served_tx.send(index).expect("provider signal");
                thread::sleep(Duration::from_millis(100));
            }
        });
        Self {
            base_url: format!("http://{address}/v1/responses"),
            served: served_rx,
            requests,
            worker: Some(worker),
        }
    }

    fn request(&self, index: usize) -> Value {
        self.requests.lock().expect("requests")[index].clone()
    }
}

impl Drop for SteerProvider {
    fn drop(&mut self) {
        if let Some(worker) = self.worker.take() {
            worker.join().expect("provider worker");
        }
    }
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

fn write_response_completed(stream: &mut TcpStream, completed: Value) {
    write!(
        stream,
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\nevent: response.output_text.delta\r\ndata: {{\"type\":\"response.output_text.delta\",\"delta\":\"thinking \"}}\n\n"
    )
    .expect("write delta");
    stream.flush().expect("flush delta");
    write!(
        stream,
        "event: response.completed\r\ndata: {completed}\r\n\r\n"
    )
    .expect("write completion");
    stream.flush().expect("flush completion");
}

fn read_http_json(stream: &mut TcpStream) -> Value {
    let mut reader = BufReader::new(stream.try_clone().expect("clone provider stream"));
    let mut request_line = String::new();
    reader
        .read_line(&mut request_line)
        .expect("read request line");
    assert!(request_line.contains("/v1/responses"));
    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).expect("read header");
        if line == "\r\n" || line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            content_length = value.trim().parse().expect("content length");
        }
    }
    let mut body = vec![0u8; content_length];
    reader.read_exact(&mut body).expect("read body");
    serde_json::from_slice(&body).expect("request json")
}

fn spawn(workspace: &Path, home: &Path, base_url: &str) -> AppServerProcess {
    AppServerProcess::spawn(workspace, home, base_url)
}

struct InterruptProvider {
    base_url: String,
    served: Receiver<()>,
    worker: Option<thread::JoinHandle<()>>,
}

impl InterruptProvider {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind interrupt provider");
        let address = listener.local_addr().expect("provider address");
        let (served_tx, served_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept interrupt request");
            let _request = read_http_json(&mut stream);
            write_response_completed(
                &mut stream,
                json!({
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
                }),
            );
            served_tx.send(()).expect("provider signal");
            thread::sleep(Duration::from_millis(200));
        });
        Self {
            base_url: format!("http://{address}/v1/responses"),
            served: served_rx,
            worker: Some(worker),
        }
    }
}

impl Drop for InterruptProvider {
    fn drop(&mut self) {
        if let Some(worker) = self.worker.take() {
            worker.join().expect("interrupt provider worker");
        }
    }
}

#[test]
fn same_stdio_connection_interrupts_running_tool_turn() {
    let dir = tempfile::tempdir().expect("temp dir");
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let home = dir.path().join("home");
    let provider = InterruptProvider::start();
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
    process.shutdown();
}

#[test]
fn same_stdio_connection_steers_and_follows_up_during_one_turn() {
    let dir = tempfile::tempdir().expect("temp dir");
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let home = dir.path().join("home");
    let provider = SteerProvider::start();
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

    process.shutdown();
}
