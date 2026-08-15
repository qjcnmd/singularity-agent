//! `AppServer` 的传输层：默认标准输入输出（stdio），可选 TCP 回环监听。
//!
//! 两种承载都走同一个 `run_with_io` 循环，只是"输入流 + 输出流"来源不同——stdio
//! 用 `tokio::io::stdin()`/`stdout()`，TCP 由 `TcpListener` 接受的一个 `TcpStream`
//! 驱动（常驻 daemon：一次接受一个连接处理到断开，再接受下一个，空闲超时自停）。
//! 输入由 Tokio 单一 owner 读取；turn/start 与 turn/resume 由单一工作线程顺序执行
//! （同一时刻至多一个 turn），其余请求在输入 owner 的 blocking 任务中直接处理。
//! 所有输出进入单一 mpsc 队列，由唯一 writer task 顺序写出 JSON 行——单生产者
//! 顺序性保证事件与响应天然有序，无需全局排序或 cursor/gap 机制。

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use cap_fs_ext::{FollowSymlinks, MetadataExt as CapMetadataExt, OpenOptionsFollowExt};
use cap_std::fs::{Dir as CapabilityDir, OpenOptions as CapabilityOpenOptions};
use serde_json::Value;
use singularity_app_server::{
    AppServer, AppServerCancellationHandle, AppServerError, AppServerOutput,
};
use singularity_core::{ErrorCode, JSON_RPC_INTERNAL_ERROR, contains_sensitive_text};
use singularity_model::ProviderConfigSnapshot;
use singularity_protocol::{
    JsonRpcBatchItem, JsonRpcId, JsonRpcMessage, JsonRpcPayload, Method, parse_json_rpc_payload,
};
use singularity_store::SessionStore;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);
const OUTPUT_QUEUE_CAPACITY: usize = 256;
const APP_ERROR_INVALID_STATE: i64 = -32005;
/// TCP daemon 无新连接的默认空闲自停阈值。
const DAEMON_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
/// 覆盖空闲自停阈值（毫秒）的环境变量，测试用它缩短等待。
const DAEMON_IDLE_TIMEOUT_MS_ENV: &str = "SINGULARITY_APP_SERVER_IDLE_TIMEOUT_MS";
const FILE_BACKED_STORE_REQUIRED: &str =
    "app-server requires a file-backed SINGULARITY_APP_SERVER_DB";
const SAFE_FILE_BACKED_STATE_REQUIRED: &str =
    "app-server requires a canonical regular file-backed state database";

trait ExecutionStop: Send + Sync {
    fn request_execution_stop(&self);
}

impl ExecutionStop for AppServerCancellationHandle {
    fn request_execution_stop(&self) {
        let _ = AppServerCancellationHandle::request_execution_stop(self);
    }
}

/// 在单一 Tokio runtime 内运行 app-server 控制面；传输默认 stdio，可通过
/// `SINGULARITY_APP_SERVER_LISTEN=tcp://127.0.0.1:PORT` 切换为 TCP 回环监听。
pub(super) async fn run(runtime_handle: tokio::runtime::Handle) -> Result<(), String> {
    let configured_db_path = std::env::var("SINGULARITY_APP_SERVER_DB")
        .unwrap_or_else(|_| ".singularity/rust-app-server.sqlite3".to_string());
    match app_server_listen()? {
        ListenerConfig::Stdio => {
            run_with_io(
                BufReader::new(tokio::io::stdin()),
                tokio::io::stdout(),
                configured_db_path,
                runtime_handle,
            )
            .await
        }
        ListenerConfig::Tcp { addr } => {
            let listener = TcpListener::bind(addr)
                .await
                .map_err(|error| format!("failed to bind TCP listen address {addr}: {error}"))?;
            let actual = listener
                .local_addr()
                .map_err(|error| format!("failed to read TCP listen address: {error}"))?;
            // listen 地址含端口 0 时向 OS 申请端口，实际端口须上报给启动方/测试。
            // 只上报一次：daemon 常驻期间后续连接不再重复 announce。
            eprintln!("SINGULARITY_APP_SERVER_LISTENING {actual}");
            let idle_timeout = daemon_idle_timeout()?;
            run_tcp_daemon(listener, configured_db_path, runtime_handle, idle_timeout).await
        }
    }
}

/// TCP daemon 主循环：接受多个连接，每连接运行独立的 `run_with_io`（因此每连接
/// 各自走一遍 initialize/initialized 握手，旧连接的握手状态不污染新连接），连接
/// 断开后回到 accept 等待下一个连接；无新连接持续超过 `idle_timeout` 时正常退出
/// （空闲自停，exit 0）。单连接收到 server/shutdown 只结束该连接的处理，不退出
/// 进程——进程退出由空闲自停接管。
async fn run_tcp_daemon(
    listener: TcpListener,
    configured_db_path: String,
    runtime_handle: tokio::runtime::Handle,
    idle_timeout: Duration,
) -> Result<(), String> {
    loop {
        let accepted = tokio::select! {
            biased;
            accepted = listener.accept() => {
                Some(accepted.map_err(|error| {
                    format!("failed to accept app-server client: {error}")
                })?)
            }
            _ = tokio::time::sleep(idle_timeout) => None,
        };
        let Some((stream, _peer)) = accepted else {
            // 空闲超时：无新连接，正常退出。
            break;
        };
        let (reader, writer) = stream.into_split();
        // 单个连接的处理失败（如该连接的协议/传输错误）不结束 daemon，记入 stderr
        // 后继续服务后续连接；idle 与后续连接照常。
        if let Err(error) = run_with_io(
            BufReader::new(reader),
            writer,
            configured_db_path.clone(),
            runtime_handle.clone(),
        )
        .await
        {
            eprintln!("app-server connection error: {error}");
        }
    }
    Ok(())
}

/// 解析空闲自停阈值：`SINGULARITY_APP_SERVER_IDLE_TIMEOUT_MS` 覆盖默认 60s。
fn daemon_idle_timeout() -> Result<Duration, String> {
    let Ok(raw) = std::env::var(DAEMON_IDLE_TIMEOUT_MS_ENV) else {
        return Ok(DAEMON_IDLE_TIMEOUT);
    };
    let millis = raw
        .parse::<u64>()
        .map_err(|_| format!("invalid {DAEMON_IDLE_TIMEOUT_MS_ENV}: {raw}"))?;
    Ok(Duration::from_millis(millis))
}

/// app-server 监听模式：未设置 `SINGULARITY_APP_SERVER_LISTEN` 时保留 stdio 默认。
enum ListenerConfig {
    Stdio,
    Tcp { addr: std::net::SocketAddr },
}

fn app_server_listen() -> Result<ListenerConfig, String> {
    let spec = match std::env::var("SINGULARITY_APP_SERVER_LISTEN") {
        Ok(spec) => spec,
        Err(_) => return Ok(ListenerConfig::Stdio),
    };
    let rest = spec.strip_prefix("tcp://").ok_or_else(|| {
        format!("unsupported SINGULARITY_APP_SERVER_LISTEN scheme (expected tcp://addr): {spec}")
    })?;
    let addr = rest.parse::<std::net::SocketAddr>().map_err(|error| {
        format!("invalid SINGULARITY_APP_SERVER_LISTEN address {spec}: {error}")
    })?;
    Ok(ListenerConfig::Tcp { addr })
}

/// 在任意"输入流 + 输出流"对上运行 JSON-Lines 控制面；stdio 与 TCP 共用此循环。
///
/// `reader`/`writer` 由调用方提供：stdio 传 `stdin()`/`stdout()`，TCP 传按 `TcpStream`
/// 拆出的读/写半。所有同步 AppServer 工作跨 blocking 边界执行。
async fn run_with_io<R, W>(
    reader: R,
    writer: W,
    configured_db_path: String,
    runtime_handle: tokio::runtime::Handle,
) -> Result<(), String>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let server = tokio::task::spawn_blocking(move || {
        initialize_app_server(&configured_db_path, runtime_handle)
    })
    .await
    .map_err(|error| format!("app-server startup task failed: {error}"))??;
    let cancellation = server.cancellation_handle();
    let (output_tx, mut output_rx) = mpsc::channel::<Value>(OUTPUT_QUEUE_CAPACITY);
    let writer_cancellation = cancellation.clone();
    let mut output = writer;
    let mut writer = tokio::spawn(async move {
        write_output_queue(&mut output_rx, &mut output, &writer_cancellation).await
    });
    let mut writer_done = false;
    let mut writer_result = None;
    let mut writer_timeout = false;
    let mut server = Some(server);
    // 单 worker 槽位：同一时刻至多一个 turn/start 或 turn/resume 执行。
    let mut turn_task: Option<JoinHandle<Result<(), String>>> = None;
    let mut lines = reader.lines();
    let mut terminal_error = None;

    loop {
        tokio::select! {
            biased;
            result = &mut writer, if !writer_done => {
                writer_done = true;
                writer_result = Some(result);
                terminal_error = Some(match writer_result.as_ref().expect("writer result") {
                    Ok(Ok(())) => "stdout writer stopped unexpectedly".to_string(),
                    Ok(Err(error)) => error.clone(),
                    Err(error) => format!("stdout writer task failed: {error}"),
                });
                break;
            }
            result = async {
                turn_task.as_mut().expect("active turn task").await
            }, if turn_task.is_some() => {
                turn_task = None;
                match result {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => {
                        terminal_error = Some(error);
                        break;
                    }
                    Err(error) => {
                        terminal_error = Some(format!("turn worker task failed: {error}"));
                        break;
                    }
                }
            }
            line = lines.next_line() => {
                let Some(line) = (match line {
                    Ok(line) => line,
                    Err(error) => {
                        terminal_error = Some(format!("failed to read stdin: {error}"));
                        break;
                    }
                }) else {
                    break;
                };
                if line.trim().is_empty() {
                    continue;
                }
                let payload = match parse_json_rpc_payload(&line) {
                    Ok(payload) => payload,
                    Err(_) => {
                        if let Err(error) = send_output_async(
                            output_tx.clone(),
                            cancellation.clone(),
                            JsonRpcMessage::parse_error().to_wire_value(),
                        )
                        .await
                        {
                            terminal_error = Some(error);
                            break;
                        }
                        continue;
                    }
                };
                if !matches!(payload, JsonRpcPayload::Single(_)) {
                    let batch_outputs = output_tx.clone();
                    let batch_cancellation = cancellation.clone();
                    let current_server = server.take().expect("stdio server owner");
                    let task = tokio::task::spawn_blocking(move || {
                        let mut server = current_server;
                        let result = dispatch_batch(
                            &mut server,
                            payload,
                            &batch_outputs,
                            &batch_cancellation,
                        );
                        (server, result)
                    });
                    let (next_server, result) = match task.await {
                        Ok(result) => result,
                        Err(error) => {
                            terminal_error = Some(format!("batch dispatch task failed: {error}"));
                            break;
                        }
                    };
                    server = Some(next_server);
                    if let Err(error) = result {
                        terminal_error = Some(error);
                        break;
                    }
                    if server.as_ref().expect("stdio server owner").shutdown_requested() {
                        break;
                    }
                    continue;
                }
                let JsonRpcPayload::Single(item) = payload else {
                    unreachable!("non-single JSON-RPC payload reached single dispatcher")
                };
                let message = match item {
                    JsonRpcBatchItem::Message(message) => message,
                    JsonRpcBatchItem::Invalid { id } => {
                        if let Err(error) = send_output_async(
                            output_tx.clone(),
                            cancellation.clone(),
                            JsonRpcMessage::invalid_request(id).to_wire_value(),
                        )
                        .await
                        {
                            terminal_error = Some(error);
                            break;
                        }
                        continue;
                    }
                };
                let request_id = message.id().cloned();
                if is_turn_request(&message)
                    && server
                        .as_ref()
                        .expect("stdio server owner")
                        .ready_for_turn_worker()
                {
                    if turn_task.is_some() {
                        if let Err(error) = send_output_async(
                            output_tx.clone(),
                            cancellation.clone(),
                            turn_slot_busy_value(request_id),
                        )
                        .await
                        {
                            terminal_error = Some(error);
                            break;
                        }
                        continue;
                    }
                    let current_server = server.take().expect("stdio server owner");
                    let task = tokio::task::spawn_blocking(move || {
                        let worker = current_server.turn_worker();
                        (current_server, worker)
                    });
                    let (next_server, worker_result) = match task.await {
                        Ok(result) => result,
                        Err(error) => {
                            terminal_error = Some(format!("turn worker setup failed: {error}"));
                            break;
                        }
                    };
                    server = Some(next_server);
                    match worker_result {
                        Ok(worker) => {
                            let worker_outputs = output_tx.clone();
                            let worker_cancellation = cancellation.clone();
                            turn_task = Some(tokio::task::spawn_blocking(move || {
                                run_turn_request(worker, message, worker_outputs, worker_cancellation)
                            }));
                        }
                        Err(error) => {
                            if let Err(error) = send_output_async(
                                output_tx.clone(),
                                cancellation.clone(),
                                transport_error_value(request_id, &error),
                            )
                            .await
                            {
                                terminal_error = Some(error);
                                break;
                            }
                        }
                    }
                } else {
                    let direct_outputs = output_tx.clone();
                    let direct_cancellation = cancellation.clone();
                    let current_server = server.take().expect("stdio server owner");
                    let task = tokio::task::spawn_blocking(move || {
                        let mut server = current_server;
                        let notification = message.is_notification();
                        let request_id = message.id().cloned();
                        let result = server.handle_with_output(message);
                        let dispatch_result = match result {
                            Ok(messages) => send_app_server_outputs(
                                &direct_outputs,
                                &direct_cancellation,
                                messages,
                            ),
                            Err(error) if !notification => send_output(
                                &direct_outputs,
                                &direct_cancellation,
                                transport_error_value(request_id, &error),
                            )
                            .map(|_| ()),
                            Err(_) => Ok(()),
                        };
                        (server, dispatch_result)
                    });
                    let (next_server, result) = match task.await {
                        Ok(result) => result,
                        Err(error) => {
                            terminal_error = Some(format!("request dispatch task failed: {error}"));
                            break;
                        }
                    };
                    server = Some(next_server);
                    if let Err(error) = result {
                        terminal_error = Some(error);
                        break;
                    }
                }
                if server.as_ref().expect("stdio server owner").shutdown_requested() {
                    break;
                }
            }
        }
    }

    let shutdown_deadline = Instant::now() + SHUTDOWN_GRACE;
    let current_server = server.take().expect("stdio server owner");
    let stop_result = tokio::task::spawn_blocking(move || current_server.request_execution_stop())
        .await
        .map_err(|error| format!("failed to stop executions during shutdown: {error}"))
        .and_then(|result| {
            result.map_err(|error| format!("failed to stop executions during shutdown: {error}"))
        });
    let mut worker_error = None;
    if let Some(mut task) = turn_task.take() {
        if let Some(remaining) = shutdown_deadline.checked_duration_since(Instant::now()) {
            match tokio::time::timeout(remaining, &mut task).await {
                Ok(Ok(Ok(()))) => {}
                Ok(Ok(Err(error))) => worker_error = Some(error),
                Ok(Err(error)) => {
                    worker_error = Some(format!("turn worker task failed: {error}"));
                }
                Err(_) => {
                    task.abort();
                    worker_error =
                        Some("timed out waiting for the turn worker during shutdown".to_string());
                }
            }
        } else {
            task.abort();
            worker_error =
                Some("timed out waiting for the turn worker during shutdown".to_string());
        }
    }
    drop(output_tx);

    if !writer_done {
        if let Some(remaining) = shutdown_deadline.checked_duration_since(Instant::now()) {
            match tokio::time::timeout(remaining, &mut writer).await {
                Ok(result) => {
                    writer_result = Some(result);
                }
                Err(_) => {
                    writer.abort();
                    writer_timeout = true;
                }
            }
        } else {
            writer.abort();
            writer_timeout = true;
        }
    }

    let mut errors = Vec::new();
    if let Err(error) = stop_result {
        errors.push(error);
    }
    if let Some(error) = worker_error {
        errors.push(error);
    }
    if let Some(error) = terminal_error {
        errors.push(error);
    }
    if writer_timeout {
        errors.push("timed out waiting for stdout writer during shutdown".to_string());
    }
    if let Some(Err(error)) = writer_result {
        errors.push(format!("stdout writer task failed: {error}"));
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

fn initialize_app_server(
    configured_db_path: &str,
    runtime_handle: tokio::runtime::Handle,
) -> Result<AppServer, String> {
    let db_path = prepare_app_server_state_paths(configured_db_path)?;
    let store = SessionStore::open(&db_path)
        .map_err(|error| format!("failed to open app-server store {db_path}: {error}"))?;
    validate_database_file(Path::new(&db_path), false)?;
    store
        .recover_unowned_workspace_executions()
        .map_err(|error| format!("failed to recover app-server thread executions: {error}"))?;
    let provider_snapshot =
        ProviderConfigSnapshot::capture(|name| std::env::var(name).ok(), Some(runtime_handle));
    Ok(AppServer::new(store, provider_snapshot))
}

async fn send_output_async(
    outputs: mpsc::Sender<Value>,
    cancellation: AppServerCancellationHandle,
    message: Value,
) -> Result<(), String> {
    tokio::task::spawn_blocking(move || send_output(&outputs, &cancellation, message))
        .await
        .map_err(|error| format!("output dispatch task failed: {error}"))?
}

/// 判断单请求是否属于独占 turn 槽位的 long-running 方法。
fn is_turn_request(message: &JsonRpcMessage) -> bool {
    !message.is_notification()
        && matches!(
            message.method_name(),
            Some(method)
                if method == Method::TurnStart.as_str() || method == Method::TurnResume.as_str()
        )
}

/// 按输入顺序串行分发 batch；副作用项不并行，notification 项不产生控制响应。
///
/// batch 与单请求同路径：无 long-worker 特殊分支，turn 方法在 batch 中顺序执行。
fn dispatch_batch(
    server: &mut AppServer,
    payload: JsonRpcPayload,
    outputs: &mpsc::Sender<Value>,
    cancellation: &dyn ExecutionStop,
) -> Result<(), String> {
    let items = match payload {
        JsonRpcPayload::EmptyBatch => {
            return send_output(
                outputs,
                cancellation,
                JsonRpcMessage::invalid_request(None).to_wire_value(),
            )
            .map(|_| ());
        }
        JsonRpcPayload::Batch(items) => items,
        JsonRpcPayload::Single(_) => {
            return Err("single JSON-RPC payload reached batch dispatcher".to_string());
        }
    };
    let mut notifications = Vec::<AppServerOutput>::new();
    let mut responses = Vec::new();
    for item in items {
        match item {
            JsonRpcBatchItem::Invalid { id } => {
                responses.push(JsonRpcMessage::invalid_request(id).to_wire_value());
            }
            JsonRpcBatchItem::Message(message) => {
                let notification = message.is_notification();
                let request_id = message.id().cloned();
                match server.handle_with_output(message) {
                    Ok(messages) => {
                        for output in messages {
                            match serde_json::from_value::<JsonRpcMessage>(output.clone()) {
                                Ok(JsonRpcMessage::Notification(_)) if !notification => {
                                    notifications.push(output);
                                }
                                Ok(JsonRpcMessage::Success(_) | JsonRpcMessage::Error(_))
                                    if !notification =>
                                {
                                    responses.push(output);
                                }
                                Ok(_) if notification => {}
                                Ok(_) | Err(_) => {
                                    responses.push(internal_error_value(
                                        request_id.clone(),
                                        "dispatcher produced an invalid response envelope",
                                    ));
                                }
                            }
                        }
                    }
                    Err(error) if !notification => {
                        responses.push(transport_error_value(request_id, &error));
                    }
                    Err(_) => {}
                }
            }
        }
    }
    send_app_server_outputs(outputs, cancellation, notifications)?;
    if responses.is_empty() {
        return Ok(());
    }
    send_output(outputs, cancellation, Value::Array(responses)).map(|_| ())
}

/// 在单一 turn 工作线程内执行 turn/start 或 turn/resume，事件与最终响应顺序入队。
fn run_turn_request(
    mut worker: AppServer,
    message: JsonRpcMessage,
    outputs: mpsc::Sender<Value>,
    cancellation: AppServerCancellationHandle,
) -> Result<(), String> {
    let request_id = message.id().cloned();
    let mut output_error = None;
    let mut emit = |output: AppServerOutput| {
        if output_error.is_none() {
            match outputs.blocking_send(output) {
                Ok(()) => {}
                Err(_) => {
                    output_error = Some("stdout transport unavailable".to_string());
                    let _ = cancellation.request_execution_stop();
                }
            }
        }
    };
    let result = match message.method_name() {
        Some(method) if method == Method::TurnStart.as_str() => {
            worker.handle_turn_start_streaming_with_output(message, &mut emit)
        }
        Some(method) if method == Method::TurnResume.as_str() => {
            worker.handle_turn_resume_streaming_with_output(message, &mut emit)
        }
        _ => Err(AppServerError::Workspace(
            "streaming dispatch requires turn/start or turn/resume".to_string(),
        )),
    };
    if let Some(error) = output_error {
        return Err(error);
    }
    if let Err(error) = result {
        send_output(
            &outputs,
            &cancellation,
            request_error_value(request_id, &error),
        )?;
    }
    Ok(())
}

/// 将消息放入唯一输出队列；队列满时阻塞（背压），真实发送失败才触发全局停止。
fn send_output(
    outputs: &mpsc::Sender<Value>,
    cancellation: &dyn ExecutionStop,
    message: Value,
) -> Result<(), String> {
    outputs.blocking_send(message).map_err(|_| {
        cancellation.request_execution_stop();
        "stdout transport unavailable".to_string()
    })
}

fn send_app_server_outputs(
    outputs: &mpsc::Sender<Value>,
    cancellation: &dyn ExecutionStop,
    messages: Vec<AppServerOutput>,
) -> Result<(), String> {
    for message in messages {
        send_output(outputs, cancellation, message)?;
    }
    Ok(())
}

/// 串行写出所有输出 frame；真实写入或 flush 失败才触发全局停止。
async fn write_output_queue<W: AsyncWrite + Unpin>(
    output_rx: &mut mpsc::Receiver<Value>,
    stdout: &mut W,
    cancellation: &dyn ExecutionStop,
) -> Result<(), String> {
    while let Some(message) = output_rx.recv().await {
        let line = match serde_json::to_vec(&message) {
            Ok(line) => line,
            Err(error) => {
                cancellation.request_execution_stop();
                return Err(format!("failed to serialize response: {error}"));
            }
        };
        if let Err(error) = stdout.write_all(&line).await {
            cancellation.request_execution_stop();
            return Err(format!("failed to write response: {error}"));
        }
        if let Err(error) = stdout.write_all(b"\n").await {
            cancellation.request_execution_stop();
            return Err(format!("failed to write response: {error}"));
        }
        if let Err(error) = stdout.flush().await {
            cancellation.request_execution_stop();
            return Err(format!("failed to flush response: {error}"));
        }
    }
    Ok(())
}

fn transport_error_value(id: Option<JsonRpcId>, error: &AppServerError) -> Value {
    let diagnostic = match error {
        AppServerError::TurnExecution { original, .. }
        | AppServerError::TurnTerminalization { original, .. } => {
            original.clone().unwrap_or_else(|| error.to_string())
        }
        other => other.to_string(),
    };
    // 透出真实错误文本供诊断（DB/锁/provider 等）；若文本疑似含密钥则回退脱敏。
    let diagnostic = if contains_sensitive_text(&diagnostic) {
        "Internal error".to_string()
    } else {
        diagnostic
    };
    internal_error_value(id, diagnostic)
}

fn request_error_value(id: Option<JsonRpcId>, error: &AppServerError) -> Value {
    match error {
        AppServerError::InvalidParams(_) => {
            JsonRpcMessage::error(id, ErrorCode::invalid_params("Invalid params")).to_wire_value()
        }
        error => transport_error_value(id, error),
    }
}

/// 单 worker 槽位被占用时拒绝第二个并发 turn 请求。
fn turn_slot_busy_value(id: Option<JsonRpcId>) -> Value {
    JsonRpcMessage::error(
        id,
        ErrorCode::new(APP_ERROR_INVALID_STATE, "another turn is already running"),
    )
    .to_wire_value()
}

fn internal_error_value(id: Option<JsonRpcId>, diagnostic: impl Into<String>) -> Value {
    JsonRpcMessage::error(
        id,
        ErrorCode::new(JSON_RPC_INTERNAL_ERROR, diagnostic.into()),
    )
    .to_wire_value()
}

fn resolve_app_server_state_paths(configured_db_path: &str) -> Result<String, String> {
    if is_unsupported_sqlite_database_path(configured_db_path) {
        return Err(FILE_BACKED_STORE_REQUIRED.to_string());
    }
    let db_path = configured_db_path.trim();
    let database_name = Path::new(db_path)
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| SAFE_FILE_BACKED_STATE_REQUIRED.to_string())?;
    validate_database_name(database_name)?;
    Ok(db_path.to_string())
}

fn is_unsupported_sqlite_database_path(configured_db_path: &str) -> bool {
    let trimmed = configured_db_path.trim();
    let lower = trimmed.to_ascii_lowercase();
    trimmed.eq_ignore_ascii_case(":memory:")
        || lower.starts_with("file:")
        || lower.starts_with("sqlite:")
}

fn prepare_app_server_state_paths(configured_db_path: &str) -> Result<String, String> {
    let raw_db_path = resolve_app_server_state_paths(configured_db_path)?;
    let raw_db_path = Path::new(&raw_db_path);
    let raw_parent = raw_db_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let canonical_parent = prepare_state_directory(raw_parent)?;
    let database_name = raw_db_path
        .file_name()
        .ok_or_else(|| SAFE_FILE_BACKED_STATE_REQUIRED.to_string())?;
    let database_path = canonical_parent.join(database_name);
    validate_database_file(&database_path, true)?;
    database_path
        .to_str()
        .map(str::to_string)
        .ok_or_else(|| SAFE_FILE_BACKED_STATE_REQUIRED.to_string())
}

fn prepare_state_directory(parent: &Path) -> Result<PathBuf, String> {
    validate_existing_state_components(parent)?;
    std::fs::create_dir_all(parent).map_err(|_| SAFE_FILE_BACKED_STATE_REQUIRED.to_string())?;
    validate_existing_state_components(parent)?;
    let canonical =
        std::fs::canonicalize(parent).map_err(|_| SAFE_FILE_BACKED_STATE_REQUIRED.to_string())?;
    let metadata = std::fs::symlink_metadata(&canonical)
        .map_err(|_| SAFE_FILE_BACKED_STATE_REQUIRED.to_string())?;
    if !metadata.is_dir() || metadata_is_reparse(&metadata) {
        return Err(SAFE_FILE_BACKED_STATE_REQUIRED.to_string());
    }
    Ok(canonical)
}

fn validate_existing_state_components(parent: &Path) -> Result<(), String> {
    let absolute = if parent.is_absolute() {
        parent.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|_| SAFE_FILE_BACKED_STATE_REQUIRED.to_string())?
            .join(parent)
    };
    let mut current = PathBuf::new();
    for component in absolute.components() {
        current.push(component);
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) => {
                if !metadata.is_dir() || metadata_is_reparse(&metadata) {
                    return Err(SAFE_FILE_BACKED_STATE_REQUIRED.to_string());
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(SAFE_FILE_BACKED_STATE_REQUIRED.to_string()),
        }
    }
    Ok(())
}

fn validate_database_name(name: &str) -> Result<(), String> {
    let normalized = name
        .to_ascii_lowercase()
        .trim_end_matches([' ', '.'])
        .to_string();
    if normalized.is_empty() {
        return Err(SAFE_FILE_BACKED_STATE_REQUIRED.to_string());
    }
    #[cfg(windows)]
    if name.ends_with([' ', '.']) || name.contains('~') {
        return Err(SAFE_FILE_BACKED_STATE_REQUIRED.to_string());
    }
    Ok(())
}

fn metadata_is_reparse(metadata: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    {
        let _ = metadata;
        false
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StateFileIdentity {
    device: u64,
    inode: u64,
    links: u64,
}

fn state_file_identity(metadata: &cap_std::fs::Metadata) -> Result<StateFileIdentity, String> {
    let identity = StateFileIdentity {
        device: CapMetadataExt::dev(metadata),
        inode: CapMetadataExt::ino(metadata),
        links: CapMetadataExt::nlink(metadata),
    };
    (identity.links == 1)
        .then_some(identity)
        .ok_or_else(|| SAFE_FILE_BACKED_STATE_REQUIRED.to_string())
}

fn open_state_file(path: &Path) -> Result<(std::fs::File, StateFileIdentity), String> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| SAFE_FILE_BACKED_STATE_REQUIRED.to_string())?;
    let name = path
        .file_name()
        .ok_or_else(|| SAFE_FILE_BACKED_STATE_REQUIRED.to_string())?;
    let directory = CapabilityDir::open_ambient_dir(parent, cap_std::ambient_authority())
        .map_err(|_| SAFE_FILE_BACKED_STATE_REQUIRED.to_string())?;
    let mut options = CapabilityOpenOptions::new();
    options.read(true).write(true).follow(FollowSymlinks::No);
    let file = directory
        .open_with(name, &options)
        .map_err(|_| SAFE_FILE_BACKED_STATE_REQUIRED.to_string())?;
    let identity = state_file_identity(
        &file
            .metadata()
            .map_err(|_| SAFE_FILE_BACKED_STATE_REQUIRED.to_string())?,
    )?;
    Ok((file.into_std(), identity))
}

fn validate_database_file(path: &Path, allow_missing: bool) -> Result<(), String> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if allow_missing && error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(());
        }
        Err(_) => return Err(SAFE_FILE_BACKED_STATE_REQUIRED.to_string()),
    };
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata_is_reparse(&metadata) {
        return Err(SAFE_FILE_BACKED_STATE_REQUIRED.to_string());
    }
    let (file, identity) = open_state_file(path)?;
    let opened = file
        .metadata()
        .map_err(|_| SAFE_FILE_BACKED_STATE_REQUIRED.to_string())?;
    if !opened.is_file() || metadata_is_reparse(&opened) {
        return Err(SAFE_FILE_BACKED_STATE_REQUIRED.to_string());
    }
    let (_, reopened_identity) = open_state_file(path)?;
    if identity != reopened_identity {
        return Err(SAFE_FILE_BACKED_STATE_REQUIRED.to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use singularity_agent::agent::AgentError;
    use singularity_app_server::{TurnFailureCause, TurnFailureStage};
    use singularity_store::StoreError;
    use std::future::Future;
    use std::io;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc as std_mpsc;
    use std::task::{Context, Poll};
    use std::thread;
    use tokio::io::AsyncWrite;

    fn block_on<F: Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test Tokio runtime")
            .block_on(future)
    }

    #[derive(Default)]
    struct VecWriter(Vec<u8>);

    impl AsyncWrite for VecWriter {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buffer: &[u8],
        ) -> Poll<io::Result<usize>> {
            self.0.extend_from_slice(buffer);
            Poll::Ready(Ok(buffer.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[derive(Clone, Default)]
    struct CancellationProbe {
        requests: Arc<AtomicUsize>,
    }

    impl CancellationProbe {
        fn request_count(&self) -> usize {
            self.requests.load(Ordering::SeqCst)
        }
    }

    impl ExecutionStop for CancellationProbe {
        fn request_execution_stop(&self) {
            self.requests.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn test_output_channel(capacity: usize) -> (mpsc::Sender<Value>, mpsc::Receiver<Value>) {
        mpsc::channel(capacity)
    }

    fn progress_event() -> Value {
        JsonRpcMessage::notification(
            "item/agentMessage/delta",
            serde_json::json!({
                "item": {"item_id": "item_progress"},
                "delta": "progress",
                "event": singularity_protocol::EventMetadata {
                    class: singularity_protocol::EventClass::Progress,
                    delivery: singularity_protocol::EventDelivery::BestEffort,
                },
            }),
        )
        .expect("progress event")
        .to_wire_value()
    }

    #[test]
    fn state_path_rejects_sqlite_uri_before_state_preparation() {
        for path in [
            ":memory:",
            " :MEMORY: ",
            "file::memory:?cache=shared",
            "file:memory-db?mode=memory&cache=shared",
            "file:memory-db?cache=shared&mode=MEMORY",
            "file:memory-db?mode=ro",
            "file:///state/rust-app-server.sqlite3",
            "FILE://localhost/state/rust-app-server.sqlite3",
        ] {
            let error = resolve_app_server_state_paths(path).expect_err("memory store rejected");
            assert_eq!(error, FILE_BACKED_STORE_REQUIRED);
        }
        assert!(resolve_app_server_state_paths("state/rust-app-server.sqlite3").is_ok());
    }

    #[test]
    fn prepared_state_paths_use_the_canonical_directory() {
        let directory = tempfile::tempdir().expect("state directory");
        let configured = directory.path().join("nested").join("sessions.sqlite3");
        let db_path = prepare_app_server_state_paths(configured.to_str().expect("configured path"))
            .expect("prepared state path");
        let canonical_parent = std::fs::canonicalize(configured.parent().expect("parent"))
            .expect("canonical state directory");
        assert_eq!(
            Path::new(&db_path).parent(),
            Some(canonical_parent.as_path())
        );
        assert!(!Path::new(&db_path).exists());
    }

    #[test]
    fn state_path_rejects_database_hard_link_before_store_open() {
        let directory = tempfile::tempdir().expect("state directory");
        let parent = directory.path().join("state");
        std::fs::create_dir(&parent).expect("create state directory");
        let source = directory.path().join("source.sqlite3");
        let database = parent.join("sessions.sqlite3");
        std::fs::write(&source, b"not a sqlite database").expect("source file");
        std::fs::hard_link(&source, &database).expect("database hard link");

        let error = prepare_app_server_state_paths(database.to_str().expect("database path"))
            .expect_err("hard-linked database rejected");
        assert_eq!(error, SAFE_FILE_BACKED_STATE_REQUIRED);
    }

    #[test]
    fn sqlite_uri_rejection_has_no_directory_side_effect() {
        let directory = tempfile::tempdir().expect("state directory");
        let missing_parent = directory.path().join("must-not-be-created");
        let configured = format!("file:{}?mode=memory", missing_parent.display());
        let error = prepare_app_server_state_paths(&configured)
            .expect_err("SQLite URI rejected before preparation");
        assert_eq!(error, FILE_BACKED_STORE_REQUIRED);
        assert!(!missing_parent.exists());
    }

    #[cfg(unix)]
    #[test]
    fn state_path_rejects_database_symlink_before_store_open() {
        let directory = tempfile::tempdir().expect("state directory");
        let parent = directory.path().join("state");
        std::fs::create_dir(&parent).expect("create state directory");
        let source = directory.path().join("source.sqlite3");
        let database = parent.join("sessions.sqlite3");
        std::fs::write(&source, b"not a sqlite database").expect("source file");
        std::os::unix::fs::symlink(&source, &database).expect("database symlink");

        let error = prepare_app_server_state_paths(database.to_str().expect("database path"))
            .expect_err("symlinked database rejected");
        assert_eq!(error, SAFE_FILE_BACKED_STATE_REQUIRED);
    }

    #[cfg(windows)]
    #[test]
    fn state_path_rejects_database_reparse_link_before_store_open() {
        let directory = tempfile::tempdir().expect("state directory");
        let parent = directory.path().join("state");
        std::fs::create_dir(&parent).expect("create state directory");
        let source = directory.path().join("source.sqlite3");
        let database = parent.join("sessions.sqlite3");
        std::fs::write(&source, b"not a sqlite database").expect("source file");
        match std::os::windows::fs::symlink_file(&source, &database) {
            Ok(()) => {}
            Err(error) if error.raw_os_error() == Some(1314) => return,
            Err(error) => panic!("database reparse link: {error}"),
        }

        let error = prepare_app_server_state_paths(database.to_str().expect("database path"))
            .expect_err("reparse-linked database rejected");
        assert_eq!(error, SAFE_FILE_BACKED_STATE_REQUIRED);
    }

    #[test]
    fn bounded_output_queue_backpressures_without_stopping_execution() {
        let (outputs, mut receiver) = test_output_channel(1);
        let cancellation = CancellationProbe::default();
        send_output(&outputs, &cancellation, serde_json::json!({"first": true}))
            .expect("first output fits");

        let (attempted_sender, attempted_receiver) = std_mpsc::channel();
        let sender_outputs = outputs.clone();
        let sender_cancellation = cancellation.clone();
        let sender = thread::spawn(move || {
            attempted_sender.send(()).expect("send attempted");
            send_output(
                &sender_outputs,
                &sender_cancellation,
                serde_json::json!({"second": true}),
            )
            .expect("bounded send");
        });
        attempted_receiver.recv().expect("send attempt");
        assert!(!sender.is_finished(), "full queue must backpressure");
        assert_eq!(cancellation.request_count(), 0);
        assert_eq!(
            receiver.blocking_recv().expect("first output")["first"],
            true
        );
        sender.join().expect("sender");
        assert_eq!(
            receiver.blocking_recv().expect("second output")["second"],
            true
        );
        assert_eq!(cancellation.request_count(), 0);
    }

    struct DisconnectedWriter;

    impl AsyncWrite for DisconnectedWriter {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buffer: &[u8],
        ) -> Poll<io::Result<usize>> {
            Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "stdout closed",
            )))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[test]
    fn stdout_writer_disconnect_stops_all_execution() {
        let (outputs, mut receiver) = test_output_channel(1);
        let cancellation = CancellationProbe::default();
        send_output(
            &outputs,
            &cancellation,
            serde_json::json!({"kind": "control"}),
        )
        .expect("control fits");
        drop(outputs);

        let mut stdout = DisconnectedWriter;
        let error = block_on(write_output_queue(
            &mut receiver,
            &mut stdout,
            &cancellation,
        ))
        .expect_err("writer disconnect is transport-fatal");

        assert!(error.starts_with("failed to write response:"));
        assert_eq!(cancellation.request_count(), 1);
    }

    #[test]
    fn writer_drains_events_and_controls_in_send_order() {
        let (outputs, mut receiver) = test_output_channel(4);
        let cancellation = CancellationProbe::default();
        send_output(&outputs, &cancellation, progress_event()).expect("event fits");
        send_output(
            &outputs,
            &cancellation,
            serde_json::json!({"kind": "control"}),
        )
        .expect("control fits");
        drop(outputs);

        let mut stdout = VecWriter::default();
        block_on(write_output_queue(
            &mut receiver,
            &mut stdout,
            &cancellation,
        ))
        .expect("writer drains frames");
        let values = String::from_utf8(stdout.0)
            .expect("writer output is UTF-8")
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).expect("JSONL frame"))
            .collect::<Vec<_>>();
        assert_eq!(values.len(), 2);
        assert_eq!(values[0]["method"], "item/agentMessage/delta");
        assert_eq!(values[1]["kind"], "control");
        assert_eq!(cancellation.request_count(), 0);
    }

    #[test]
    fn mixed_batch_is_sequential_and_only_requests_produce_ordered_responses() {
        let store = SessionStore::open(":memory:").expect("store");
        let mut server = AppServer::new(store, ProviderConfigSnapshot::capture(|_| None, None));
        let cancellation = server.cancellation_handle();
        let (outputs, mut receiver) = test_output_channel(8);
        let payload = parse_json_rpc_payload(
            r#"[
                {"jsonrpc":"2.0","method":"initialize","id":1,"params":{"clientInfo":{"name":"test","title":"Test","version":"0.1.0"}}},
                {"jsonrpc":"2.0","method":"initialized","params":{}},
                {"jsonrpc":"2.0","method":"server/capabilities","id":2,"params":{}},
                {"jsonrpc":"2.0","method":"thread/read","params":{}},
                {"jsonrpc":"2.0","method":"unknown","params":{}},
                {"jsonrpc":"2.0","method":"unknown","id":3,"params":{}},
                {"jsonrpc":"2.0","method":"thread/read","id":4,"params":{}}
            ]"#,
        )
        .expect("batch parses");

        dispatch_batch(&mut server, payload, &outputs, &cancellation).expect("dispatch batch");

        let output = receiver.try_recv().expect("batch response");
        let responses = output.as_array().expect("batch response array");
        assert_eq!(responses.len(), 4);
        assert_eq!(responses[0]["id"], 1);
        assert_eq!(responses[1]["id"], 2);
        assert_eq!(responses[2]["id"], 3);
        assert_eq!(responses[2]["error"]["code"], -32601);
        assert_eq!(responses[3]["id"], 4);
        assert_eq!(responses[3]["error"]["code"], -32602);
        assert!(
            responses
                .iter()
                .all(|response| response["jsonrpc"] == "2.0")
        );
    }

    #[test]
    fn batch_dispatches_turn_methods_like_any_other_request() {
        let store = SessionStore::open(":memory:").expect("store");
        let mut server = AppServer::new(store, ProviderConfigSnapshot::capture(|_| None, None));
        let cancellation = server.cancellation_handle();
        let (outputs, mut receiver) = test_output_channel(16);
        let payload = parse_json_rpc_payload(
            r#"[
                {"jsonrpc":"2.0","method":"initialize","id":1,"params":{"clientInfo":{"name":"test","title":"Test","version":"0.1.0"}}},
                {"jsonrpc":"2.0","method":"initialized","params":{}},
                {"jsonrpc":"2.0","method":"server/capabilities","id":2,"params":{}},
                {"jsonrpc":"2.0","method":"turn/start","id":3,"params":{}},
                {"jsonrpc":"2.0","method":"turn/resume","id":4,"params":{}},
                {"jsonrpc":"2.0","method":"turn/start","params":{}},
                {"jsonrpc":"2.0","method":"server/capabilities","id":6,"params":{}}
            ]"#,
        )
        .expect("batch parses");

        dispatch_batch(&mut server, payload, &outputs, &cancellation).expect("dispatch batch");

        let output = receiver.try_recv().expect("batch response");
        let responses = output.as_array().expect("batch response array");
        assert_eq!(responses.len(), 5);
        assert_eq!(responses[0]["id"], 1);
        assert_eq!(responses[1]["id"], 2);
        assert_eq!(
            responses[1]["result"]["transports"][0]["transport"],
            "stdio"
        );
        // 无 long-worker 特殊分支：turn 方法与普通请求同路径，参数校验失败返回
        // Invalid params，而不是 batch 拒绝。
        for (response, id) in responses.iter().skip(2).zip([3, 4]) {
            assert_eq!(response["id"], id);
            assert_eq!(response["error"]["code"], -32602);
            assert_eq!(response["error"]["message"], "Invalid params");
        }
        assert_eq!(responses[4]["id"], 6);
        assert!(responses[4]["result"]["transports"].is_array());
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn all_notification_batch_has_no_output_even_for_unknown_method_or_invalid_params() {
        let store = SessionStore::open(":memory:").expect("store");
        let mut server = AppServer::new(store, ProviderConfigSnapshot::capture(|_| None, None));
        let cancellation = server.cancellation_handle();
        let (outputs, mut receiver) = test_output_channel(8);
        let payload = parse_json_rpc_payload(
            r#"[
                {"jsonrpc":"2.0","method":"thread/read","params":{}},
                {"jsonrpc":"2.0","method":"unknown","params":{}}
            ]"#,
        )
        .expect("notification batch parses");

        dispatch_batch(&mut server, payload, &outputs, &cancellation)
            .expect("dispatch notification batch");

        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn notification_only_request_is_invalid_without_changing_batch_notification_contract() {
        let store = SessionStore::open(":memory:").expect("store");
        let mut server = AppServer::new(store, ProviderConfigSnapshot::capture(|_| None, None));
        let cancellation = server.cancellation_handle();
        let (outputs, mut receiver) = test_output_channel(2);
        let payload = parse_json_rpc_payload(
            r#"[
                {"jsonrpc":"2.0","method":"initialized","id":1,"params":{}},
                {"jsonrpc":"2.0","method":"initialized","params":{}},
                {"jsonrpc":"2.0","method":"thread/read","params":{}}
            ]"#,
        )
        .expect("mixed notification batch parses");

        dispatch_batch(&mut server, payload, &outputs, &cancellation)
            .expect("dispatch mixed notification batch");

        let output = receiver.try_recv().expect("invalid request response");
        let responses = output.as_array().expect("batch response array");
        assert_eq!(responses.len(), 1);
        assert_eq!(responses[0]["id"], 1);
        assert_eq!(responses[0]["error"]["code"], -32600);
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn empty_batch_returns_standard_invalid_request() {
        let store = SessionStore::open(":memory:").expect("store");
        let mut server = AppServer::new(store, ProviderConfigSnapshot::capture(|_| None, None));
        let cancellation = server.cancellation_handle();
        let (outputs, mut receiver) = test_output_channel(1);

        dispatch_batch(
            &mut server,
            JsonRpcPayload::EmptyBatch,
            &outputs,
            &cancellation,
        )
        .expect("dispatch empty batch");

        let response = receiver.try_recv().expect("invalid request response");
        assert_eq!(response["jsonrpc"], "2.0");
        assert_eq!(response["id"], Value::Null);
        assert_eq!(response["error"]["code"], -32600);
    }

    #[test]
    fn streaming_worker_maps_invalid_params_without_exposing_diagnostics() {
        let response = request_error_value(
            Some(JsonRpcId::Number(7)),
            &AppServerError::InvalidParams("secret-shaped diagnostic".to_string()),
        );

        assert_eq!(response["jsonrpc"], "2.0");
        assert_eq!(response["id"], 7);
        assert_eq!(response["error"]["code"], -32602);
        assert_eq!(response["error"]["message"], "Invalid params");
        assert!(!response.to_string().contains("secret-shaped"));
    }

    #[test]
    fn transport_error_exposes_store_agent_and_workspace_text() {
        let cases: Vec<(&str, AppServerError)> = vec![
            (
                "locked by another process",
                AppServerError::Store(StoreError::InvalidState(
                    "locked by another process".to_string(),
                )),
            ),
            ("provider unavailable", {
                AppServerError::Agent(AgentError::Loop("provider unavailable".to_string()))
            }),
            ("workspace error: write denied", {
                AppServerError::Workspace("write denied".to_string())
            }),
        ];

        for (expected, error) in cases {
            let response = transport_error_value(Some(JsonRpcId::Number(7)), &error);
            assert_eq!(response["error"]["code"], -32603, "{expected}");
            let message = response["error"]["message"].as_str().expect("message");
            assert_ne!(message, "Internal error", "real text must not be masked");
            assert!(
                message.contains(expected),
                "expected real text {expected:?}, got {message:?}"
            );
        }
    }

    #[test]
    fn transport_error_carries_original_turn_execution_text() {
        let original = "agent loop failed: provider returned 500";
        let response = transport_error_value(
            Some(JsonRpcId::Number(7)),
            &AppServerError::TurnExecution {
                stage: TurnFailureStage::AgentLoop,
                cause: TurnFailureCause::Internal,
                original: Some(original.to_string()),
            },
        );

        assert_eq!(response["error"]["code"], -32603);
        assert_eq!(response["error"]["message"], original);
    }

    #[test]
    fn transport_error_redacts_sensitive_diagnostic_text() {
        let error = AppServerError::Workspace(
            "cannot open SINGULARITY_API_KEY=sk-shape must-not-leak".to_string(),
        );
        let response = transport_error_value(Some(JsonRpcId::Number(7)), &error);

        assert_eq!(response["error"]["code"], -32603);
        assert_eq!(response["error"]["message"], "Internal error");
        assert!(!response.to_string().contains("sk-shape"));
    }

    #[test]
    fn turn_slot_busy_has_a_stable_typed_response() {
        let response = turn_slot_busy_value(Some(JsonRpcId::String("request-7".into())));

        assert_eq!(response["jsonrpc"], "2.0");
        assert_eq!(response["id"], "request-7");
        assert_eq!(response["error"]["code"], -32005);
        assert_eq!(
            response["error"]["message"],
            "another turn is already running"
        );
    }

    #[test]
    fn stdout_writer_join_obeys_the_shutdown_deadline() {
        let error = block_on(async {
            let (_release_sender, release_receiver) = tokio::sync::oneshot::channel::<()>();
            let mut writer = tokio::spawn(async move {
                let _ = release_receiver.await;
                Ok::<(), String>(())
            });
            match tokio::time::timeout(Duration::from_millis(20), &mut writer).await {
                Err(_) => {
                    writer.abort();
                    Err("timed out waiting for stdout writer during shutdown".to_string())
                }
                Ok(result) => result
                    .map_err(|error| error.to_string())
                    .and_then(|result| result),
            }
        })
        .expect_err("stalled writer must not outlive the deadline");

        assert_eq!(error, "timed out waiting for stdout writer during shutdown");
    }
}
