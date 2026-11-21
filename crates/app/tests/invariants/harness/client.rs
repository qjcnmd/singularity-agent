//! 隔离的临时测试环境与进程外 `singularity.exe` JSON-RPC 黑盒客户端。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{mpsc, oneshot};

pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(20);

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
