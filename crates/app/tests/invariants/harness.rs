//! 黑盒集成测试脚手架：临时目录隔离、`singularity.exe` JSON-RPC 客户端、
//! 本地 HTTP/1.1 SSE 回放与故障注入桩、通用协议不变量校验、测试 MCP stdio 脚本。

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{Notify, mpsc, oneshot};

pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(20);

/// 1x1 像素合法 PNG 的 data URL，供图片上传与持久化测试使用。
pub const TINY_PNG_DATA_URL: &str = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";

/// RAII 临时根目录：结束时自动清理，隔离 `SINGULARITY_HOME` 与测试工作区。
pub struct TestEnv {
    root: PathBuf,
    pub home: PathBuf,
    pub workspace: PathBuf,
}

impl TestEnv {
    pub fn new(label: &str) -> Self {
        let unique = uuid::Uuid::new_v4();
        let root = std::env::temp_dir().join(format!("singularity-test-{label}-{unique}"));
        let home = root.join("home");
        let workspace = root.join("workspace");
        std::fs::create_dir_all(&home).expect("create test home");
        std::fs::create_dir_all(&workspace).expect("create test workspace");
        let home = std::fs::canonicalize(&home).expect("canonicalize home");
        let workspace = std::fs::canonicalize(&workspace).expect("canonicalize workspace");
        Self { root, home, workspace }
    }

    pub fn workspace_str(&self) -> String {
        let raw = self.workspace.to_string_lossy();
        raw.strip_prefix(r"\\?\").unwrap_or(&raw).replace('\\', "/")
    }
}

impl Drop for TestEnv {
    fn drop(&mut self) {
        for _ in 0..5 {
            if std::fs::remove_dir_all(&self.root).is_ok() || !self.root.exists() {
                return;
            }
            std::thread::sleep(Duration::from_millis(80));
        }
    }
}

/// 进程外黑盒 JSON-RPC 客户端，直接驱动编译产物 `singularity.exe`。
pub struct BackendClient {
    child: Child,
    stdin: Option<ChildStdin>,
    next_id: u64,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>,
    events_rx: mpsc::UnboundedReceiver<Value>,
    buffered_events: Vec<Value>,
    stderr: Arc<Mutex<String>>,
}

impl BackendClient {
    pub async fn spawn(home: &Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_singularity"))
            .env("SINGULARITY_HOME", home)
            .env("SINGULARITY_LOG", "warn")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn singularity.exe");

        let stdin = child.stdin.take().expect("child stdin");
        let stdout = child.stdout.take().expect("child stdout");
        let stderr_pipe = child.stderr.take().expect("child stderr");

        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>> = Arc::new(Mutex::new(HashMap::new()));
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let stderr = Arc::new(Mutex::new(String::new()));

        let pending_reader = Arc::clone(&pending);
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if line.trim().is_empty() {
                    continue;
                }
                let parsed: Value = serde_json::from_str(&line).expect("valid JSON line from backend");
                if let Some(id) = parsed.get("id").and_then(Value::as_u64) {
                    let response = parsed.get("response").cloned().unwrap_or(Value::Null);
                    if let Some(tx) = pending_reader.lock().expect("pending lock").remove(&id) {
                        let _ = tx.send(response);
                    }
                } else {
                    let _ = events_tx.send(parsed);
                }
            }
        });

        let stderr_buf = Arc::clone(&stderr);
        tokio::spawn(async move {
            let mut reader = BufReader::new(stderr_pipe);
            let mut chunk = String::new();
            while let Ok(n) = reader.read_line(&mut chunk).await {
                if n == 0 {
                    break;
                }
                stderr_buf.lock().expect("stderr lock").push_str(&chunk);
                chunk.clear();
            }
        });

        let mut client = Self {
            child,
            stdin: Some(stdin),
            next_id: 0,
            pending,
            events_rx,
            buffered_events: Vec::new(),
            stderr,
        };
        client
            .wait_event(DEFAULT_TIMEOUT, |frame| frame.get("type").and_then(Value::as_str) == Some("ready"))
            .await;
        client
    }

    pub fn stderr_text(&self) -> String {
        self.stderr.lock().expect("stderr lock").clone()
    }

    pub async fn rpc_raw(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        let (tx, rx) = oneshot::channel();
        self.pending.lock().expect("pending lock").insert(id, tx);
        let payload = json!({
            "id": id,
            "request": {
                "method": method,
                "params": params,
            }
        });
        let mut line = serde_json::to_vec(&payload).expect("serialize RPC request");
        line.push(b'\n');
        let stdin = self.stdin.as_mut().expect("stdin still open");
        stdin.write_all(&line).await.expect("write RPC line");
        stdin.flush().await.expect("flush RPC line");
        tokio::time::timeout(DEFAULT_TIMEOUT, rx)
            .await
            .unwrap_or_else(|_| panic!("RPC {method} timed out; stderr:\n{}", self.stderr_text()))
            .unwrap_or_else(|_| panic!("RPC {method} channel closed; stderr:\n{}", self.stderr_text()))
    }

    pub async fn rpc_ok(&mut self, method: &str, params: Value) -> Value {
        let response = self.rpc_raw(method, params).await;
        assert_eq!(
            response.get("type").and_then(Value::as_str),
            Some("success"),
            "RPC {method} failed: {response}; stderr:\n{}",
            self.stderr_text()
        );
        response.get("result").cloned().unwrap_or(Value::Null)
    }

    pub async fn rpc_err(&mut self, method: &str, params: Value) -> String {
        let response = self.rpc_raw(method, params).await;
        assert_eq!(
            response.get("type").and_then(Value::as_str),
            Some("error"),
            "expected RPC {method} error, got: {response}"
        );
        response.pointer("/error/message").and_then(Value::as_str).unwrap_or_default().to_string()
    }

    /// 只取出截至最近一次 `wait_event` 匹配为止的已缓冲事件，不拉取后续可能已到达通道的事件。
    pub fn take_buffered_events(&mut self) -> Vec<Value> {
        std::mem::take(&mut self.buffered_events)
    }

    /// 清空已缓冲事件并返回当前历史快照。
    pub fn drain_events(&mut self) -> Vec<Value> {
        while let Ok(frame) = self.events_rx.try_recv() {
            self.buffered_events.push(frame);
        }
        std::mem::take(&mut self.buffered_events)
    }

    pub async fn wait_event(&mut self, timeout: Duration, mut pred: impl FnMut(&Value) -> bool) -> Value {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            let frame = tokio::time::timeout(remaining, self.events_rx.recv())
                .await
                .unwrap_or_else(|_| {
                    panic!(
                        "timed out waiting for stream event; buffered={:?}; stderr:\n{}",
                        self.buffered_events,
                        self.stderr_text()
                    )
                })
                .unwrap_or_else(|| {
                    panic!("backend event stream ended unexpectedly; stderr:\n{}", self.stderr_text())
                });
            let matched = pred(&frame);
            self.buffered_events.push(frame.clone());
            if matched {
                return frame;
            }
        }
    }

    pub async fn wait_settled(&mut self, session_id: &str) -> Value {
        self.wait_event(DEFAULT_TIMEOUT, |frame| {
            frame.get("type").and_then(Value::as_str) == Some("session_settled")
                && frame.get("sessionId").and_then(Value::as_str) == Some(session_id)
        })
        .await
    }

    pub async fn shutdown(mut self) {
        drop(self.stdin.take());
        let status = tokio::time::timeout(DEFAULT_TIMEOUT, self.child.wait())
            .await
            .expect("backend graceful exit timeout")
            .expect("wait backend exit");
        assert!(status.success(), "backend exited with {status}; stderr:\n{}", self.stderr_text());
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn setup_workspace_and_provider(
        &mut self,
        workspace: &str,
        provider_id: &str,
        protocol: &str,
        base_url: &str,
        model_id: &str,
        max_context_tokens: u32,
        max_output_tokens: u32,
    ) -> String {
        let added = self.rpc_ok("workspace.add", json!({ "root": workspace })).await;
        let canonical = added.get("root").and_then(Value::as_str).expect("workspace.add root").to_string();
        self.rpc_ok(
            "model.saveProvider",
            json!({
                "provider": {
                    "apiProtocol": protocol,
                    "providerId": provider_id,
                    "displayName": provider_id,
                    "baseUrl": base_url,
                    "models": [{
                        "modelId": model_id,
                        "maxContextTokens": max_context_tokens,
                        "maxOutputTokens": max_output_tokens,
                        "reasoningVariants": ["off", "low", "high"]
                    }]
                },
                "apiKey": "test-secret-key"
            }),
        )
        .await;
        canonical
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn start_session(
        &mut self,
        workspace: &str,
        provider: &str,
        model: &str,
        reasoning: Option<&str>,
        text: &str,
        images: Vec<Value>,
        disabled_modules: Vec<&str>,
    ) -> String {
        let result = self
            .rpc_ok(
                "session.start",
                json!({
                    "workspaceRoot": workspace,
                    "selection": { "provider": provider, "model": model, "reasoning": reasoning },
                    "text": text,
                    "images": images,
                    "files": [],
                    "disabledModules": disabled_modules,
                }),
            )
            .await;
        let session_id =
            result.pointer("/summary/threadId").and_then(Value::as_str).expect("threadId").to_string();
        self.rpc_ok("session.focus", json!({ "sessionId": session_id })).await;
        session_id
    }
}

/// 捕获的每次上游 HTTP 请求。
#[derive(Debug, Clone)]
pub struct CapturedRequest {
    pub path: String,
    pub body: Value,
}

/// 脚本化响应动作。
pub enum ScriptedResponse {
    Json {
        status: u16,
        headers: Vec<(String, String)>,
        body: Value,
    },
    Sse {
        events: Vec<String>,
        /// 每个事件后可选等待门闸（用于确定性测试运行中取消/入队/删除）。
        gate_after_first: Option<Arc<Notify>>,
        /// 发送完前 N 个事件后直接断开 TCP 连接，模拟半截断流。
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

/// 轻量原生 Tokio HTTP/1.1 本地测试桩：
/// 1. 支持 `/v1/models`、`/v1/messages`、`/v1/responses`；
/// 2. 自动对每次发往 `/messages` 和 `/responses` 的请求体运行协议不变量校验，违规记录并在结束时强断言；
/// 3. 不引入任何额外第三方 HTTP 服务器 crate。
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

    pub fn requests(&self) -> Vec<CapturedRequest> {
        self.state.lock().expect("state lock").requests.clone()
    }

    pub fn model_requests(&self) -> Vec<CapturedRequest> {
        self.requests()
            .into_iter()
            .filter(|req| req.path.ends_with("/messages") || req.path.ends_with("/responses"))
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

    // 通用协议不变量检查：违规记录到 ServerState（主测试线程必失败），避免在后台连接协程里静默吞 panic。
    let violation = if path.ends_with("/messages") {
        check_messages_request_invariants(&captured.body).err()
    } else if path.ends_with("/responses") {
        check_responses_request_invariants(&captured.body).err()
    } else {
        None
    };

    let (response, notify) = {
        let mut guard = state.lock().expect("state lock");
        let index = guard.requests.len();
        guard.requests.push(captured.clone());
        let notify = Arc::clone(&guard.request_notify);
        let response = if let Some(err) = violation {
            guard.violations.push(format!("[{path} #{index}] {err}"));
            ScriptedResponse::json(
                500,
                json!({ "error": { "code": "protocol_invariant_violation", "message": err } }),
            )
        } else if method == "GET" && path.ends_with("/models") {
            let data: Vec<Value> = guard.models.iter().map(|id| json!({ "id": id })).collect();
            ScriptedResponse::json(200, json!({ "data": data }))
        } else if let Some(next) = guard.queue.pop_front() {
            next
        } else if let Some(fallback) = guard.fallback.clone() {
            fallback(&captured, index)
        } else {
            ScriptedResponse::json(
                500,
                json!({ "error": { "message": "unexpected request in test server" } }),
            )
        };
        (response, notify)
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
                head.push_str(&format!("{k}: {v}\r\n"));
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
                // 使用 Transfer-Encoding: chunked 且不发终止块即断开连接，真实模拟网络中途断流。
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

/// Anthropic Messages 协议不变量：
/// 1. 工具 schema 必须全部是合法 object schema；
/// 2. `user` / `assistant` 角色严格交替，不存在空内容消息；
/// 3. 每个 assistant `tool_use` 紧跟的下一条 user 消息里必须包含对应 `tool_result`，且不存在孤儿 `tool_result`。
pub fn check_messages_request_invariants(body: &Value) -> Result<(), String> {
    if let Some(tools) = body.get("tools").and_then(Value::as_array) {
        for tool in tools {
            if tool.get("name").and_then(Value::as_str).is_none_or(str::is_empty) {
                return Err(format!("Messages tool missing name: {tool}"));
            }
            if tool.pointer("/input_schema/type").and_then(Value::as_str) != Some("object") {
                return Err(format!("Messages tool schema must be object: {tool}"));
            }
        }
    }
    let Some(messages) = body.get("messages").and_then(Value::as_array) else {
        return Err(format!("Messages request must have messages array; body={body}"));
    };
    if messages.is_empty() {
        return Err(format!("Messages request must not have empty messages; body={body}"));
    }

    let mut prev_role: Option<&str> = None;
    let mut expected_tool_ids: HashSet<String> = HashSet::new();
    for (idx, message) in messages.iter().enumerate() {
        let Some(role) = message.get("role").and_then(Value::as_str) else {
            return Err(format!("message missing role at index {idx}"));
        };
        if let Some(prev) = prev_role
            && prev == role
        {
            return Err(format!("Messages roles must strictly alternate at index {idx}: {messages:?}"));
        }
        prev_role = Some(role);
        let Some(blocks) = message.get("content").and_then(Value::as_array) else {
            return Err(format!("message content must be array at index {idx}"));
        };
        if blocks.is_empty() {
            return Err(format!("message content must not be empty at index {idx}"));
        }

        if role == "user" {
            let mut seen_results = HashSet::new();
            for block in blocks {
                if block.get("type").and_then(Value::as_str) == Some("tool_result") {
                    let Some(id) = block.get("tool_use_id").and_then(Value::as_str) else {
                        return Err(format!("tool_result missing tool_use_id at index {idx}"));
                    };
                    if !expected_tool_ids.contains(id) {
                        return Err(format!(
                            "orphaned tool_result {id} without preceding tool_use at index {idx}"
                        ));
                    }
                    seen_results.insert(id.to_string());
                }
            }
            if seen_results != expected_tool_ids {
                return Err(format!(
                    "tool_use/tool_result mismatch at index {idx}: expected={expected_tool_ids:?}, seen={seen_results:?}"
                ));
            }
            expected_tool_ids.clear();
        } else if role == "assistant" {
            if !expected_tool_ids.is_empty() {
                return Err(format!("unanswered tool_use before next assistant message at index {idx}"));
            }
            for block in blocks {
                if block.get("type").and_then(Value::as_str) == Some("tool_use") {
                    let Some(id) = block.get("id").and_then(Value::as_str) else {
                        return Err(format!("tool_use missing id at index {idx}"));
                    };
                    expected_tool_ids.insert(id.to_string());
                }
            }
        }
    }
    if !expected_tool_ids.is_empty() {
        return Err(format!("dangling tool_use at end of Messages request: {expected_tool_ids:?}"));
    }
    Ok(())
}

/// OpenAI Responses 协议不变量：
/// 1. 函数工具必须是合法 object schema；
/// 2. 每个 `function_call` 都有匹配的 `function_call_output`，无孤儿输出。
pub fn check_responses_request_invariants(body: &Value) -> Result<(), String> {
    if let Some(tools) = body.get("tools").and_then(Value::as_array) {
        for tool in tools {
            if tool.get("type").and_then(Value::as_str) != Some("function") {
                return Err(format!("Responses tool type must be function: {tool}"));
            }
            if tool.pointer("/parameters/type").and_then(Value::as_str) != Some("object") {
                return Err(format!("Responses tool schema must be object: {tool}"));
            }
        }
    }
    let Some(input) = body.get("input").and_then(Value::as_array) else {
        return Err(format!("Responses request must have input array; body={body}"));
    };
    if input.is_empty() {
        return Err(format!("Responses request must not have empty input; body={body}"));
    }
    let mut call_ids = HashSet::new();
    let mut output_ids = HashSet::new();
    for item in input {
        match item.get("type").and_then(Value::as_str) {
            Some("function_call") => {
                let Some(id) = item.get("call_id").and_then(Value::as_str) else {
                    return Err(format!("Responses function_call missing call_id: {item}"));
                };
                call_ids.insert(id.to_string());
            }
            Some("function_call_output") => {
                let Some(id) = item.get("call_id").and_then(Value::as_str) else {
                    return Err(format!("Responses function_call_output missing call_id: {item}"));
                };
                if !call_ids.contains(id) {
                    return Err(format!("orphaned function_call_output {id}"));
                }
                output_ids.insert(id.to_string());
            }
            _ => {}
        }
    }
    if call_ids != output_ids {
        return Err(format!(
            "Responses function_call/output mismatch: calls={call_ids:?}, outputs={output_ids:?}"
        ));
    }
    Ok(())
}

fn sse_data(payload: &Value) -> String {
    format!("data: {payload}\n\n")
}

/// 构造标准 Messages SSE 文本回复（可选思考与签名、自定义用量与停止原因）。
pub fn messages_text_sse(
    thinking: Option<(&str, &str)>,
    text: &str,
    input_tokens: u64,
    cache_read_tokens: u64,
    output_tokens: u64,
    stop_reason: &str,
) -> Vec<String> {
    let mut events = vec![sse_data(&json!({
        "type": "message_start",
        "message": {
            "usage": {
                "input_tokens": input_tokens,
                "cache_read_input_tokens": cache_read_tokens,
                "output_tokens": 1
            }
        }
    }))];
    let mut index = 0u64;
    if let Some((thought, signature)) = thinking {
        events.push(sse_data(&json!({
            "type": "content_block_start",
            "index": index,
            "content_block": { "type": "thinking", "thinking": "", "signature": "" }
        })));
        events.push(sse_data(&json!({
            "type": "content_block_delta",
            "index": index,
            "delta": { "type": "thinking_delta", "thinking": thought }
        })));
        events.push(sse_data(&json!({
            "type": "content_block_delta",
            "index": index,
            "delta": { "type": "signature_delta", "signature": signature }
        })));
        events.push(sse_data(&json!({ "type": "content_block_stop", "index": index })));
        index += 1;
    }
    if !text.is_empty() {
        events.push(sse_data(&json!({
            "type": "content_block_start",
            "index": index,
            "content_block": { "type": "text", "text": "" }
        })));
        events.push(sse_data(&json!({
            "type": "content_block_delta",
            "index": index,
            "delta": { "type": "text_delta", "text": text }
        })));
        events.push(sse_data(&json!({ "type": "content_block_stop", "index": index })));
    }
    events.push(sse_data(&json!({
        "type": "message_delta",
        "delta": { "stop_reason": stop_reason },
        "usage": { "output_tokens": output_tokens }
    })));
    events.push(sse_data(&json!({ "type": "message_stop" })));
    events
}

/// 构造带一个或多个工具调用的 Messages SSE 响应。
pub fn messages_tools_sse(
    preface: Option<&str>,
    calls: Vec<(&str, &str, Value)>,
    input_tokens: u64,
    output_tokens: u64,
    stop_reason: &str,
) -> Vec<String> {
    let mut events = vec![sse_data(&json!({
        "type": "message_start",
        "message": { "usage": { "input_tokens": input_tokens, "output_tokens": 1 } }
    }))];
    let mut index = 0u64;
    if let Some(text) = preface {
        events.push(sse_data(&json!({
            "type": "content_block_start",
            "index": index,
            "content_block": { "type": "text", "text": text }
        })));
        events.push(sse_data(&json!({ "type": "content_block_stop", "index": index })));
        index += 1;
    }
    for (id, name, args) in calls {
        events.push(sse_data(&json!({
            "type": "content_block_start",
            "index": index,
            "content_block": { "type": "tool_use", "id": id, "name": name, "input": {} }
        })));
        let raw_json = serde_json::to_string(&args).expect("serialize tool args");
        let mid = raw_json.floor_char_boundary(raw_json.len() / 2);
        events.push(sse_data(&json!({
            "type": "content_block_delta",
            "index": index,
            "delta": { "type": "input_json_delta", "partial_json": &raw_json[..mid] }
        })));
        events.push(sse_data(&json!({
            "type": "content_block_delta",
            "index": index,
            "delta": { "type": "input_json_delta", "partial_json": &raw_json[mid..] }
        })));
        events.push(sse_data(&json!({ "type": "content_block_stop", "index": index })));
        index += 1;
    }
    events.push(sse_data(&json!({
        "type": "message_delta",
        "delta": { "stop_reason": stop_reason },
        "usage": { "output_tokens": output_tokens }
    })));
    events.push(sse_data(&json!({ "type": "message_stop" })));
    events
}

/// 构造标准 OpenAI Responses SSE 文本/思考/工具调用响应。
pub fn responses_sse(
    thinking: Option<(&str, &str)>,
    text: Option<&str>,
    calls: Vec<(&str, &str, Value)>,
    input_tokens: u64,
    cached_tokens: u64,
    output_tokens: u64,
    truncated: bool,
) -> Vec<String> {
    let mut events = Vec::new();
    let mut output = Vec::new();
    if let Some((summary, encrypted)) = thinking {
        events.push(sse_data(&json!({
            "type": "response.reasoning_summary_text.delta",
            "delta": summary
        })));
        output.push(json!({
            "id": "rs_test_1",
            "type": "reasoning",
            "summary": [{ "type": "summary_text", "text": summary }],
            "encrypted_content": encrypted
        }));
    }
    if let Some(body) = text {
        events.push(sse_data(&json!({
            "type": "response.output_text.delta",
            "delta": body
        })));
        output.push(json!({
            "id": "msg_test_1",
            "type": "message",
            "role": "assistant",
            "content": [{ "type": "output_text", "text": body }]
        }));
    }
    for (idx, (call_id, name, args)) in calls.into_iter().enumerate() {
        let raw_args = serde_json::to_string(&args).expect("serialize args");
        events.push(sse_data(&json!({
            "type": "response.function_call_arguments.delta",
            "delta": raw_args
        })));
        output.push(json!({
            "id": format!("fc_{idx}"),
            "type": "function_call",
            "call_id": call_id,
            "name": name,
            "arguments": raw_args
        }));
    }
    let terminal_type = if truncated {
        "response.incomplete"
    } else {
        "response.completed"
    };
    let mut response = json!({
        "id": "resp_test_1",
        "status": if truncated { "incomplete" } else { "completed" },
        "output": output,
        "usage": {
            "input_tokens": input_tokens,
            "output_tokens": output_tokens,
            "input_tokens_details": { "cached_tokens": cached_tokens }
        }
    });
    if truncated {
        response["incomplete_details"] = json!({ "reason": "max_output_tokens" });
    }
    events.push(sse_data(&json!({ "type": terminal_type, "response": response })));
    events
}

/// 生成极简测试用 MCP stdio 服务（Node.js），把通用工具调度测试与具体内置工具解耦。
pub fn write_test_mcp_script(dir: &Path) -> PathBuf {
    let script = dir.join("test_mcp_server.mjs");
    let source = r#"
import { createInterface } from 'node:readline';

const tools = [
  {
    name: 'echo',
    description: 'Echo text with optional delay_ms',
    inputSchema: {
      type: 'object',
      properties: {
        text: { type: 'string' },
        delay_ms: { type: 'integer' }
      },
      required: ['text']
    }
  },
  {
    name: 'fail_tool',
    description: 'Always returns an error result',
    inputSchema: { type: 'object', properties: {} }
  },
  {
    name: 'large_output',
    description: 'Returns repetitive text of kb kilobytes',
    inputSchema: {
      type: 'object',
      properties: { kb: { type: 'integer' } },
      required: ['kb']
    }
  },
  {
    name: 'ping',
    description: 'Zero-arg ping tool',
    inputSchema: { type: 'object', properties: {} }
  }
];

function send(msg) {
  process.stdout.write(JSON.stringify(msg) + '\n');
}

const rl = createInterface({ input: process.stdin, crlfDelay: Infinity });
rl.on('line', async (line) => {
  if (!line.trim()) return;
  let req;
  try { req = JSON.parse(line); } catch { return; }
  const { id, method, params } = req;
  if (method === 'initialize') {
    send({
      jsonrpc: '2.0',
      id,
      result: {
        protocolVersion: params?.protocolVersion ?? '2025-03-26',
        capabilities: { tools: {} },
        serverInfo: { name: 'test-mcp', version: '1.0.0' },
        instructions: 'TEST_MCP_INSTRUCTIONS_MARKER'
      }
    });
  } else if (method === 'notifications/initialized') {
    // no-op
  } else if (method === 'tools/list') {
    send({ jsonrpc: '2.0', id, result: { tools } });
  } else if (method === 'tools/call') {
    const name = params?.name;
    const args = params?.arguments ?? {};
    if (name === 'echo') {
      const delay = Number(args.delay_ms ?? 0);
      if (delay > 0) await new Promise((r) => setTimeout(r, delay));
      send({ jsonrpc: '2.0', id, result: { content: [{ type: 'text', text: `ECHO:${args.text ?? ''}` }] } });
    } else if (name === 'fail_tool') {
      send({ jsonrpc: '2.0', id, result: { isError: true, content: [{ type: 'text', text: 'SIMULATED_MCP_FAILURE' }] } });
    } else if (name === 'large_output') {
      const kb = Math.max(1, Number(args.kb ?? 16));
      const chunk = 'L'.repeat(1020) + '\n';
      send({ jsonrpc: '2.0', id, result: { content: [{ type: 'text', text: chunk.repeat(kb) }] } });
    } else if (name === 'ping') {
      send({ jsonrpc: '2.0', id, result: { content: [{ type: 'text', text: 'PONG' }] } });
    } else {
      send({ jsonrpc: '2.0', id, error: { code: -32601, message: `Unknown tool ${name}` } });
    }
  } else if (id !== undefined) {
    send({ jsonrpc: '2.0', id, result: {} });
  }
});
"#;
    std::fs::write(&script, source.trim_start()).expect("write test MCP script");
    let display = script.to_string_lossy();
    PathBuf::from(display.strip_prefix(r"\\?\").unwrap_or(&display))
}
