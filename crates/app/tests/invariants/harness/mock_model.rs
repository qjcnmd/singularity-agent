use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;

use super::client::DEFAULT_TIMEOUT;
use super::protocol_checks::{check_messages_request_invariants, check_responses_request_invariants};

#[derive(Debug, Clone)]
pub struct CapturedRequest {
    pub path: String,
    pub body: Value,
}

pub enum ScriptedResponse {
    Json {
        status: u16,
        headers: Vec<(String, String)>,
        body: Value,
    },
    Sse {
        events: Vec<String>,
        gate_after_first: Option<Arc<Notify>>,
        drop_connection: bool,
    },
}

impl ScriptedResponse {
    pub fn json(status: u16, body: Value) -> Self {
        Self::Json { status, headers: Vec::new(), body }
    }

    pub fn json_with_headers(status: u16, headers: Vec<(&str, &str)>, body: Value) -> Self {
        Self::Json {
            status,
            headers: headers.into_iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            body,
        }
    }

    pub fn sse(events: Vec<String>) -> Self {
        Self::Sse {
            events,
            gate_after_first: None,
            drop_connection: false,
        }
    }

    pub fn sse_gated(events: Vec<String>, gate: Arc<Notify>) -> Self {
        Self::Sse {
            events,
            gate_after_first: Some(gate),
            drop_connection: false,
        }
    }

    pub fn sse_truncated_drop(events: Vec<String>) -> Self {
        Self::Sse {
            events,
            gate_after_first: None,
            drop_connection: true,
        }
    }
}

type DynamicResponder = Arc<dyn Fn(&CapturedRequest, usize) -> ScriptedResponse + Send + Sync>;

struct ServerState {
    requests: Vec<CapturedRequest>,
    queue: VecDeque<ScriptedResponse>,
    fallback: Option<DynamicResponder>,
    models: Vec<String>,
    request_notify: Arc<Notify>,
    violations: Vec<String>,
}

pub struct MockModelServer {
    pub base_url: String,
    state: Arc<Mutex<ServerState>>,
    handle: tokio::task::JoinHandle<()>,
}

impl MockModelServer {
    pub async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind loopback");
        let addr = listener.local_addr().expect("local addr");
        let state = Arc::new(Mutex::new(ServerState {
            requests: Vec::new(),
            queue: VecDeque::new(),
            fallback: None,
            models: vec!["mock-model".to_string()],
            request_notify: Arc::new(Notify::new()),
            violations: Vec::new(),
        }));
        let server_state = Arc::clone(&state);
        let handle = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let state = Arc::clone(&server_state);
                tokio::spawn(async move {
                    let _ = handle_http_connection(stream, state).await;
                });
            }
        });
        Self {
            base_url: format!("http://{addr}/v1"),
            state,
            handle,
        }
    }

    pub fn set_models(&self, models: Vec<&str>) {
        self.state.lock().expect("state lock").models = models.into_iter().map(str::to_string).collect();
    }

    pub fn enqueue(&self, response: ScriptedResponse) {
        self.state.lock().expect("state lock").queue.push_back(response);
    }

    pub fn set_fallback(
        &self,
        responder: impl Fn(&CapturedRequest, usize) -> ScriptedResponse + Send + Sync + 'static,
    ) {
        self.state.lock().expect("state lock").fallback = Some(Arc::new(responder));
    }

    pub fn model_requests(&self) -> Vec<CapturedRequest> {
        self.state
            .lock()
            .expect("state lock")
            .requests
            .iter()
            .filter(|req| req.path.ends_with("/messages") || req.path.ends_with("/responses"))
            .cloned()
            .collect()
    }

    pub fn assert_no_protocol_violations(&self) {
        let violations = self.state.lock().expect("state lock").violations.clone();
        assert!(
            violations.is_empty(),
            "protocol invariant violations detected:\n{}",
            violations.join("\n---\n")
        );
    }

    pub async fn wait_for_request_count(&self, expected: usize) {
        let notify = Arc::clone(&self.state.lock().expect("state lock").request_notify);
        let deadline = tokio::time::Instant::now() + DEFAULT_TIMEOUT;
        loop {
            let notified = notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.model_requests().len() >= expected {
                return;
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            tokio::time::timeout(remaining, notified)
                .await
                .unwrap_or_else(|_| panic!("timed out waiting for {expected} model requests"));
        }
    }
}

impl Drop for MockModelServer {
    fn drop(&mut self) {
        self.handle.abort();
        if !std::thread::panicking() {
            let violations = self.state.lock().map(|g| g.violations.clone()).unwrap_or_default();
            assert!(
                violations.is_empty(),
                "protocol invariant violations detected before MockModelServer drop:\n{}",
                violations.join("\n---\n")
            );
        }
    }
}

enum DispatchPlan {
    Immediate(ScriptedResponse),
    Fallback {
        responder: DynamicResponder,
        index: usize,
    },
}

async fn handle_http_connection(
    mut stream: TcpStream,
    state: Arc<Mutex<ServerState>>,
) -> std::io::Result<()> {
    let mut buffer = Vec::new();
    let mut header_end = None;
    let mut tmp = [0u8; 4096];
    while header_end.is_none() {
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            return Ok(());
        }
        buffer.extend_from_slice(&tmp[..n]);
        header_end = find_bytes(&buffer, b"\r\n\r\n");
    }
    let header_end = header_end.expect("header boundary");
    let header_text = String::from_utf8_lossy(&buffer[..header_end]);
    let mut lines = header_text.lines();
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let path = parts.next().unwrap_or_default().to_string();

    let mut content_length = 0usize;
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            let key = k.trim().to_ascii_lowercase();
            let val = v.trim();
            if key == "content-length" {
                content_length = val.parse().unwrap_or(0);
            }
        }
    }

    let body_start = header_end + 4;
    while buffer.len() < body_start + content_length {
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            break;
        }
        buffer.extend_from_slice(&tmp[..n]);
    }
    let body_bytes = &buffer[body_start..(body_start + content_length).min(buffer.len())];
    let body = if body_bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(body_bytes).expect("valid JSON request body from backend")
    };

    let captured = CapturedRequest { path: path.clone(), body };

    let violation = if path.ends_with("/messages") {
        check_messages_request_invariants(&captured.body).err()
    } else if path.ends_with("/responses") {
        check_responses_request_invariants(&captured.body).err()
    } else {
        None
    };

    let (plan, notify) = {
        let mut guard = state.lock().expect("state lock");
        let index = guard.requests.len();
        guard.requests.push(captured.clone());
        let notify = Arc::clone(&guard.request_notify);
        let plan = if let Some(err) = violation {
            guard.violations.push(format!("[{path} #{index}] {err}"));
            DispatchPlan::Immediate(ScriptedResponse::json(
                500,
                json!({ "error": { "code": "protocol_invariant_violation", "message": err } }),
            ))
        } else if method == "GET" && path.ends_with("/models") {
            let data: Vec<Value> = guard.models.iter().map(|id| json!({ "id": id })).collect();
            DispatchPlan::Immediate(ScriptedResponse::json(200, json!({ "data": data })))
        } else if let Some(next) = guard.queue.pop_front() {
            DispatchPlan::Immediate(next)
        } else if let Some(responder) = guard.fallback.clone() {
            DispatchPlan::Fallback { responder, index }
        } else {
            DispatchPlan::Immediate(ScriptedResponse::json(
                500,
                json!({ "error": { "message": "unexpected request in test server" } }),
            ))
        };
        (plan, notify)
    };
    let response = match plan {
        DispatchPlan::Immediate(response) => response,
        DispatchPlan::Fallback { responder, index } => responder(&captured, index),
    };
    notify.notify_waiters();

    match response {
        ScriptedResponse::Json { status, headers, body } => {
            let bytes = serde_json::to_vec(&body).expect("serialize JSON response");
            let mut head = format!(
                "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
                bytes.len()
            );
            for (k, v) in headers {
                use std::fmt::Write as _;
                let _ = write!(head, "{k}: {v}\r\n");
            }
            head.push_str("\r\n");
            stream.write_all(head.as_bytes()).await?;
            stream.write_all(&bytes).await?;
            stream.flush().await?;
        }
        ScriptedResponse::Sse {
            events,
            gate_after_first,
            drop_connection,
        } => {
            if drop_connection {
                let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n";
                stream.write_all(head.as_bytes()).await?;
                for event in &events {
                    let chunk = format!("{:x}\r\n{event}\r\n", event.len());
                    stream.write_all(chunk.as_bytes()).await?;
                }
                stream.flush().await?;
                drop(stream);
            } else {
                let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n";
                stream.write_all(head.as_bytes()).await?;
                stream.flush().await?;
                for (idx, event) in events.iter().enumerate() {
                    stream.write_all(event.as_bytes()).await?;
                    stream.flush().await?;
                    if idx == 0
                        && let Some(gate) = &gate_after_first
                    {
                        gate.notified().await;
                    }
                }
                let _ = stream.shutdown().await;
            }
        }
    }
    Ok(())
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|window| window == needle)
}
