//! stdio app-server client and process lifecycle.

use super::*;
use std::fmt;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, Command as ProcessCommand, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use singularity_core::ClientInfo;
use singularity_protocol::{
    AgentCapabilityResult, EmptyParams, InitializeParams, InputItem, JsonRpcId, JsonRpcMessage,
    JsonRpcNotification, Method, RpcMethod, SessionDeleteResult, SessionIdParams,
    SessionReadParams, SessionReadResult, Thread, ThreadIdParams, ThreadSettingsParams,
    ThreadStartParams, Turn, TurnStartParams, rpc_methods,
};

use crate::render::should_render_assistant_summary;

const APP_SERVER_BIN_ENV: &str = "SINGULARITY_APP_SERVER_BIN";
const APP_SERVER_DB_ENV: &str = "SINGULARITY_APP_SERVER_DB";
const DEFAULT_APP_SERVER_BIN: &str = "singularity_app_server";
const CLI_CLIENT_NAME: &str = "singularity_cli";
const CLI_CLIENT_TITLE: &str = "Singularity CLI";
const CLI_CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);
const SHUTDOWN_WAIT_TIMEOUT: Duration = Duration::from_secs(2);

pub(super) struct AppServerClient {
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    stdout: Receiver<Result<String, String>>,
    stdout_reader: Option<JoinHandle<()>>,
    pub(super) response_timeout: Duration,
    next_id: i64,
}

pub(super) struct RpcReply<R> {
    result: R,
    notifications: Vec<JsonRpcNotification>,
}

#[derive(Debug, Clone)]
pub(super) struct AppServerRpcError {
    message: String,
}

impl fmt::Display for AppServerRpcError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "app-server error: {}", self.message)
    }
}

impl std::error::Error for AppServerRpcError {}

impl From<String> for AppServerRpcError {
    fn from(message: String) -> Self {
        Self { message }
    }
}

impl From<AppServerRpcError> for String {
    fn from(error: AppServerRpcError) -> Self {
        error.to_string()
    }
}

pub(super) fn app_server_bin() -> Result<String, String> {
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

pub(super) fn app_server_db_display() -> String {
    if let Ok(db) = std::env::var(APP_SERVER_DB_ENV) {
        return db;
    }
    singularity_core::user_singularity_home()
        .map(|home| home.join("index.sqlite3").to_string_lossy().to_string())
        .unwrap_or_else(|| "~/.singularity/index.sqlite3".to_string())
}

fn sibling_app_server_bin() -> Option<std::path::PathBuf> {
    let mut path = std::env::current_exe().ok()?;
    path.pop();
    path.push(format!(
        "{DEFAULT_APP_SERVER_BIN}{}",
        std::env::consts::EXE_SUFFIX
    ));
    path.is_file().then_some(path)
}

// 渲染 turn 通知并打印终态行。turn/start 的响应始终在 AgentLoop 完成后返回，
// 不再有跨进程 turn/status 轮询。
pub(super) fn render_and_wait_terminal(
    _client: &mut AppServerClient,
    turn: Turn,
    notifications: Vec<JsonRpcNotification>,
    render: bool,
) -> Result<Turn, String> {
    if render {
        render_messages(&notifications, should_render_assistant_summary(&turn));
        render_turn(&turn);
    }
    Ok(turn)
}

// AppServerClient 的生命周期与 JSON-RPC 操作实现。
impl AppServerClient {
    // 每次命令启动一个独立 stdio app-server 子进程。
    pub(super) fn spawn() -> Result<Self, String> {
        let mut command = ProcessCommand::new(app_server_bin()?);
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
        let (stdout, stdout_reader) = spawn_line_reader(stdout);
        Ok(Self {
            child: Some(child),
            stdin: Some(stdin),
            stdout,
            stdout_reader: Some(stdout_reader),
            response_timeout: RESPONSE_TIMEOUT,
            next_id: 1,
        })
    }

    // 完成 JSON-RPC initialize/initialized 握手；事件全量接收，无需订阅。
    pub(super) fn initialize(&mut self) -> Result<(), String> {
        let _ = self.request::<rpc_methods::Initialize>(&InitializeParams {
            client_info: ClientInfo::new(CLI_CLIENT_NAME, CLI_CLIENT_TITLE, CLI_CLIENT_VERSION),
            capabilities: None,
        })?;
        self.notify::<rpc_methods::Initialized>(&EmptyParams::default())?;
        Ok(())
    }

    // 创建 thread，并可选地渲染启动事件。
    pub(super) fn thread_start(
        &mut self,
        model: Option<String>,
        render: bool,
    ) -> Result<(Thread, Vec<JsonRpcNotification>), String> {
        let reply = self.request::<rpc_methods::ThreadStart>(&ThreadStartParams {
            model,
            cwd: Some(canonical_current_dir()?),
        })?;
        if render {
            render_messages(&reply.notifications, false);
        }
        Ok((reply.result.thread, reply.notifications))
    }

    // 恢复现有 thread，不向 app-server 上传历史。
    pub(super) fn thread_resume(&mut self, thread_id: &str) -> Result<Thread, String> {
        let reply = self.request::<rpc_methods::ThreadResume>(&ThreadIdParams {
            thread_id: thread_id.to_string(),
        })?;
        Ok(reply.result.thread)
    }

    pub(super) fn thread_settings(
        &mut self,
        thread_id: &str,
        model: Option<String>,
    ) -> Result<(), String> {
        if model.is_none() {
            return Ok(());
        }
        let _ = self.request::<rpc_methods::ThreadSettings>(&ThreadSettingsParams {
            thread_id: thread_id.to_string(),
            provider: None,
            model,
            reasoning: None,
        })?;
        Ok(())
    }

    // 读取 AgentLoop capability 快照。
    pub(super) fn agent_capability(&mut self) -> Result<AgentCapabilityResult, String> {
        let reply = self.request::<rpc_methods::AgentCapability>(&EmptyParams::default())?;
        Ok(reply.result)
    }

    // 启动 turn、渲染事件，并在必要时轮询到终态。
    pub(super) fn turn_start(
        &mut self,
        thread_id: &str,
        text: &str,
        render: bool,
    ) -> Result<(Turn, Vec<JsonRpcNotification>), String> {
        let params = TurnStartParams {
            thread_id: thread_id.to_string(),
            input: vec![InputItem::Text {
                text: text.to_string(),
            }],
        };
        let reply = self.request::<rpc_methods::TurnStart>(&params)?;
        let turn = render_and_wait_terminal(
            self,
            reply.result.turn.clone(),
            reply.notifications.clone(),
            render,
        )?;
        Ok((turn, reply.notifications))
    }

    // 请求 session/read 并返回服务端结果。
    pub(super) fn fetch_session_read(
        &mut self,
        session_id: &str,
        limit: Option<u32>,
    ) -> Result<SessionReadResult, String> {
        let reply = self.request::<rpc_methods::SessionRead>(&SessionReadParams {
            session_id: session_id.to_string(),
            recent_limit: limit.unwrap_or(20),
            offset: None,
            kinds: Vec::new(),
        })?;
        Ok(reply.result)
    }

    // 读取会话摘要 + 最近片段，按稳定文本渲染。
    pub(super) fn session_read(
        &mut self,
        session_id: &str,
        limit: Option<u32>,
    ) -> Result<(), String> {
        let result = self.fetch_session_read(session_id, limit)?;
        println!("session {}", result.session_id);
        println!("cwd {}", result.cwd);
        match result.status.as_deref() {
            Some(status) => println!("status {status}"),
            None => println!("status none"),
        }
        if let Some(title) = result.title {
            println!("title {title}");
        }
        if let Some(model) = result.model {
            println!("model {model}");
        }
        println!("created_at {}", result.created_at);
        println!("updated_at {}", result.updated_at);
        println!("total_entries {}", result.total_entries);
        match result.summary {
            Some(summary) => println!("summary {summary}"),
            None => println!("summary none"),
        }
        println!(
            "recent_entries {}",
            serde_json::to_string_pretty(&result.recent_entries)
                .map_err(|error| format!("failed to render session entries: {error}"))?
        );
        Ok(())
    }

    // 删除 JSONL rollout 与 SQLite 索引行。
    pub(super) fn session_delete(&mut self, session_id: &str) -> Result<(), String> {
        let reply = self.request::<rpc_methods::SessionDelete>(&SessionIdParams {
            session_id: session_id.to_string(),
        })?;
        let result: SessionDeleteResult = reply.result;
        println!("session {} deleted={}", result.session_id, result.deleted);
        Ok(())
    }

    // 请求并打印持久化 session 列表（所有项目 + cwd）。
    pub(super) fn thread_list(&mut self) -> Result<(), String> {
        let reply = self.request::<rpc_methods::ThreadList>(&EmptyParams::default())?;
        for thread in &reply.result.threads {
            let last_turn_status = thread
                .last_turn_status
                .map(|status| status.as_storage_text())
                .unwrap_or("");
            println!(
                "{} {} {}",
                thread.thread_id,
                last_turn_status,
                thread.cwd.as_deref().unwrap_or("")
            );
        }
        Ok(())
    }

    // 发送请求并只接收匹配 id 的响应，同时保留通知事件。
    pub(super) fn request<M: RpcMethod>(
        &mut self,
        params: &M::Params,
    ) -> Result<RpcReply<M::Result>, AppServerRpcError> {
        let method = M::METHOD;
        let params_value = serde_json::to_value(params)
            .map_err(|error| format!("failed to serialize {} params: {error}", method.as_str()))?;
        method
            .spec()
            .validate_params(params_value)
            .map_err(|error| format!("invalid {} params: {error}", method.as_str()))?;
        let id = JsonRpcId::Number(self.next_request_id());
        let message = JsonRpcMessage::request(method, id.clone(), params)
            .map_err(|error| format!("failed to serialize app-server request: {error}"))?;
        self.write_message(&message)?;
        let mut notifications = Vec::new();
        loop {
            match self.read_message(self.response_timeout)? {
                JsonRpcMessage::Notification(notification) => {
                    notifications.push(notification);
                }
                JsonRpcMessage::Success(response) if response.id == id => {
                    let result = serde_json::from_value(response.result)
                        .map_err(|error| format!("invalid {} result: {error}", method.as_str()))?;
                    return Ok(RpcReply {
                        result,
                        notifications,
                    });
                }
                JsonRpcMessage::Error(response) if response.id == id => {
                    return Err(AppServerRpcError {
                        message: response.error.message,
                    });
                }
                JsonRpcMessage::Success(_) | JsonRpcMessage::Error(_) => {}
                JsonRpcMessage::Request(_) => {
                    return Err("app-server emitted a request on the response channel"
                        .to_string()
                        .into());
                }
            }
        }
    }

    // 向 app-server 发送 JSON-RPC notification。
    pub(super) fn notify<M: RpcMethod>(&mut self, params: &M::Params) -> Result<(), String> {
        let method = M::METHOD;
        let message = JsonRpcMessage::notification(method.as_str(), params)
            .map_err(|error| format!("failed to serialize app-server notification: {error}"))?;
        self.write_message(&message)
    }

    // 序列化、写入并 flush 一条 JSON-RPC 消息。
    pub(super) fn write_message(&mut self, message: &JsonRpcMessage) -> Result<(), String> {
        let payload = message.to_wire_value();
        let write_result = {
            let writer = self.write_side_mut()?;
            writeln!(writer, "{payload}")
        };
        if let Err(error) = write_result {
            return Err(
                self.classify_transport_write_error("failed to write app-server request", error)
            );
        }

        let flush_result = {
            let writer = self.write_side_mut()?;
            writer.flush()
        };
        if let Err(error) = flush_result {
            return Err(
                self.classify_transport_write_error("failed to flush app-server request", error)
            );
        }
        Ok(())
    }

    // 返回 app-server 子进程的可写 stdin。
    pub(super) fn write_side_mut(&mut self) -> Result<&mut dyn Write, String> {
        self.stdin
            .as_mut()
            .ok_or_else(|| "app-server stdin unavailable".to_string())
            .map(|writer| writer as &mut dyn Write)
    }

    // 写端失败时先确认 app-server 是否已经退出，避免以竞争性的 Broken pipe
    // 覆盖“响应前退出”的稳定传输错误；进程仍存活时保留真实 I/O 原因。
    pub(super) fn classify_transport_write_error(
        &mut self,
        operation: &str,
        error: std::io::Error,
    ) -> String {
        if matches!(
            error.kind(),
            std::io::ErrorKind::BrokenPipe
                | std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::UnexpectedEof
        ) {
            let deadline = Instant::now() + Duration::from_millis(100);
            while let Some(child) = self.child.as_mut() {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        return format!("app-server exited before response: {status}");
                    }
                    Ok(None) if Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    Ok(None) => break,
                    Err(status_error) => {
                        return format!(
                            "{operation}: {error}; failed to poll app-server process status: {status_error}"
                        );
                    }
                }
            }
        }
        match self.child.as_mut() {
            Some(child) => match child.try_wait() {
                Ok(Some(status)) => format!("app-server exited before response: {status}"),
                Ok(None) => format!("{operation}: {error}"),
                Err(status_error) => format!(
                    "{operation}: {error}; failed to poll app-server process status: {status_error}"
                ),
            },
            None => format!("{operation}: {error}"),
        }
    }

    // 从子进程 stdout 读取队列接收一行，并区分超时与断开。
    pub(super) fn recv_line(&mut self, timeout: Duration) -> Result<String, String> {
        let result = self.stdout.recv_timeout(timeout);
        let exit_status = self
            .child
            .as_mut()
            .map(|child| {
                child
                    .try_wait()
                    .map_err(|error| format!("failed to poll app-server process status: {error}"))
            })
            .transpose()?
            .flatten();
        match result {
            Ok(line) => line,
            Err(RecvTimeoutError::Timeout) => {
                if let Some(status) = exit_status {
                    return Err(format!("app-server exited before response: {status}"));
                }
                Err("timed out waiting for app-server response".to_string())
            }
            Err(RecvTimeoutError::Disconnected) => {
                if let Some(status) = exit_status {
                    return Err(format!("app-server exited before response: {status}"));
                }
                Err("app-server closed stdout".to_string())
            }
        }
    }

    // 从读取队列读一条消息，并区分超时、断开与非法 JSON。
    pub(super) fn read_message(&mut self, timeout: Duration) -> Result<JsonRpcMessage, String> {
        loop {
            let line = self.recv_line(timeout)?;
            // 防御：残留子进程可能直写非 JSON 行（含 lossy 替换后的坏字节）；
            // 跳过非 JSON 行，不因垃圾行终止协议（真正的 response 行仍会被解析）。
            match serde_json::from_str(line.trim()) {
                Ok(message) => return Ok(message),
                Err(_) => continue,
            }
        }
    }

    // 分配本地递增的 JSON-RPC request id。
    pub(super) fn next_request_id(&mut self) -> i64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }
}

// 负责 graceful shutdown 与一次性 stdio 子进程资源回收。
impl Drop for AppServerClient {
    fn drop(&mut self) {
        // 请求服务端 shutdown（stdin 存在时说明传输仍存活）。
        if self.stdin.is_some() {
            let id = self.next_request_id();
            if let Ok(message) =
                JsonRpcMessage::request(Method::ServerShutdown, id, EmptyParams::default())
            {
                let _ = self.write_message(&message);
            }
        }
        let _ = self.stdin.take();
        let reader_handle = self.stdout_reader.take();
        // 等待一次性子进程退出，超时则 kill。
        if let Some(child) = self.child.as_mut() {
            let deadline = Instant::now() + SHUTDOWN_WAIT_TIMEOUT;
            loop {
                match child.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) if Instant::now() < deadline => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Ok(None) | Err(_) => {
                        let _ = child.kill();
                        break;
                    }
                }
            }
            let _ = child.wait();
        }
        if let Some(handle) = reader_handle {
            let _ = handle.join();
        }
    }
}

// 将子进程 stdout 按行转发到客户端接收队列。
// 防御：残留子进程可能直写管道写入非 UTF-8 字节（Windows 句柄继承）；遇非法
// 字节用 lossy 替换并继续读下一行，不让单个坏行终止整个客户端。
fn spawn_line_reader(
    reader: impl Read + Send + 'static,
) -> (Receiver<Result<String, String>>, JoinHandle<()>) {
    let (sender, receiver) = mpsc::channel();
    let handle = thread::spawn(move || {
        let mut reader = BufReader::new(reader);
        loop {
            let mut bytes = Vec::new();
            let read_result = reader.read_until(b'\n', &mut bytes);
            match read_result {
                Ok(0) => break,
                Ok(_) => {
                    // lossy：容忍行内非 UTF-8 字节（残留进程直写），避免客户端崩溃。
                    let line = String::from_utf8_lossy(&bytes).into_owned();
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

// 获取并规范化当前工作目录，作为 thread/start 的 cwd。
fn canonical_current_dir() -> Result<String, String> {
    let cwd = std::env::current_dir()
        .map_err(|error| format!("failed to read current directory: {error}"))?
        .canonicalize()
        .map_err(|error| format!("failed to canonicalize current directory: {error}"))?;
    cwd.to_str()
        .map(str::to_string)
        .ok_or_else(|| "current directory is not valid UTF-8".to_string())
}
