//! `AppServer` 的传输层：stdio JSON-Lines 控制面。
//!
//! CLI 每次命令启动独立 app-server 子进程，经 `tokio::io::stdin()`/`stdout()` 通信；
//! 不保留 TCP daemon、连接复用或空闲自停。输入由 Tokio 单一 owner 读取；
//! turn/start 为每个 active turn 创建独立 worker；同一连接可并发运行不同 session，
//! 但每个 session 同时只允许一个 turn。其余请求在输入 owner 的 blocking 任务中直接处理。
//! 所有输出进入单一 mpsc 队列，由唯一 writer task 顺序写出 JSON 行——单生产者
//! 顺序性保证事件与响应天然有序，无需全局排序或 cursor/gap 机制。

pub(crate) mod error;
pub(crate) mod framing;
pub(crate) mod output;
pub(crate) mod supervisor;

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
use tokio::io::{
    AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader,
};
use tokio::sync::mpsc;

const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);
const OUTPUT_QUEUE_CAPACITY: usize = 256;
/// 单条 JSON-Lines frame（含 JSON-RPC 请求/响应）的字节上限。
const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;
const FILE_BACKED_STORE_REQUIRED: &str =
    "app-server requires a file-backed SINGULARITY_APP_SERVER_DB";
const SAFE_FILE_BACKED_STATE_REQUIRED: &str =
    "app-server requires a canonical regular file-backed state database";

trait ExecutionStop: Send + Sync {
    fn request_execution_stop(&self);
    fn execution_stop_requested(&self) -> bool;
}

impl ExecutionStop for AppServerCancellationHandle {
    fn request_execution_stop(&self) {
        let _ = AppServerCancellationHandle::request_execution_stop(self);
    }

    fn execution_stop_requested(&self) -> bool {
        AppServerCancellationHandle::execution_stop_requested(self)
    }
}

/// 在单一 Tokio runtime 内运行 stdio app-server 控制面。
pub(super) async fn run(runtime_handle: tokio::runtime::Handle) -> Result<(), String> {
    // 未设置 SINGULARITY_APP_SERVER_DB 时由 initialize_app_server 使用
    // AppPaths::resolve() 的用户目录 index.sqlite3；这里不再保留旧的
    // 项目目录 `.singularity/rust-app-server.sqlite3` 默认路径。
    let configured_db_path = std::env::var("SINGULARITY_APP_SERVER_DB").unwrap_or_default();
    run_with_io(
        BufReader::new(tokio::io::stdin()),
        tokio::io::stdout(),
        configured_db_path,
        runtime_handle,
    )
    .await
}

/// 在 stdio 上运行 JSON-Lines 控制面；所有同步 AppServer 工作跨 blocking 边界执行。
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
    // 支持不同 session 的多个 turn/start 并发执行。
    let mut turn_tasks: tokio::task::JoinSet<Result<(), String>> = tokio::task::JoinSet::new();
    let mut reader = reader;
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
            Some(join_result) = turn_tasks.join_next(), if !turn_tasks.is_empty() => {
                match join_result {
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
            line = read_bounded_line(&mut reader) => {
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
                let JsonRpcPayload::Single(item) = payload else {
                    // JSON-RPC batch 没有 stdio 消费者；直接拒绝，避免输入批量扩容。
                    if let Err(error) = send_output_async(
                        output_tx.clone(),
                        cancellation.clone(),
                        JsonRpcMessage::invalid_request(None).to_wire_value(),
                    )
                    .await
                    {
                        terminal_error = Some(error);
                        break;
                    }
                    continue;
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
                            turn_tasks.spawn_blocking(move || {
                                run_turn_request(worker, message, worker_outputs, worker_cancellation)
                            });
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
    while !turn_tasks.is_empty() {
        if let Some(remaining) = shutdown_deadline.checked_duration_since(Instant::now()) {
            match tokio::time::timeout(remaining, turn_tasks.join_next()).await {
                Ok(Some(Ok(Ok(())))) => {}
                Ok(Some(Ok(Err(error)))) => {
                    if worker_error.is_none() {
                        worker_error = Some(error);
                    }
                }
                Ok(Some(Err(error))) => {
                    if worker_error.is_none() {
                        worker_error = Some(format!("turn worker task failed: {error}"));
                    }
                }
                Ok(None) => break,
                Err(_) => {
                    // A started `spawn_blocking` job cannot be force-aborted. All
                    // execution and output seams are cancellation-aware, so after the
                    // bounded grace period keep owning and joining the workers rather
                    // than detaching them from the AppServer lifecycle. The diagnostic
                    // records that the normal deadline was exceeded while preserving
                    // the stronger ownership invariant.
                    worker_error = Some(
                        "turn workers exceeded shutdown grace; waiting for cooperative quiescence"
                            .to_string(),
                    );
                    while let Some(join_result) = turn_tasks.join_next().await {
                        match join_result {
                            Ok(Ok(())) => {}
                            Ok(Err(error)) if worker_error.is_none() => {
                                worker_error = Some(error);
                            }
                            Ok(Err(_)) => {}
                            Err(error) if worker_error.is_none() => {
                                worker_error = Some(format!("turn worker task failed: {error}"));
                            }
                            Err(_) => {}
                        }
                    }
                    break;
                }
            }
        } else {
            worker_error = Some(
                "turn workers exceeded shutdown grace; waiting for cooperative quiescence"
                    .to_string(),
            );
            while let Some(join_result) = turn_tasks.join_next().await {
                match join_result {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) if worker_error.is_none() => worker_error = Some(error),
                    Ok(Err(_)) => {}
                    Err(error) if worker_error.is_none() => {
                        worker_error = Some(format!("turn worker task failed: {error}"));
                    }
                    Err(_) => {}
                }
            }
            break;
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

/// 有界读取一条 JSON-Lines frame（剥离末尾 `\n` / `\r\n`）。
///
/// `AsyncBufReadExt::lines` 会把单条超长 frame 无界读入内存；这里用 `take` 给
/// `read_until` 加硬上限，超限 frame 返回错误并终止连接（fail-closed）。
async fn read_bounded_line<R: AsyncBufRead + Unpin>(
    reader: &mut R,
) -> std::io::Result<Option<String>> {
    read_bounded_line_with_limit(reader, MAX_FRAME_BYTES).await
}

async fn read_bounded_line_with_limit<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    max_frame_bytes: usize,
) -> std::io::Result<Option<String>> {
    let mut bytes = Vec::new();
    let limit = u64::try_from(max_frame_bytes)
        .unwrap_or(u64::MAX)
        .saturating_add(1);
    let mut limited = reader.take(limit);
    let read = limited.read_until(b'\n', &mut bytes).await?;
    if read == 0 {
        return Ok(None);
    }
    if bytes.ends_with(b"\n") {
        bytes.pop();
        if bytes.ends_with(b"\r") {
            bytes.pop();
        }
    } else if read as usize >= max_frame_bytes.saturating_add(1) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("JSON-RPC frame exceeds {max_frame_bytes} bytes"),
        ));
    }
    String::from_utf8(bytes).map(Some).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "JSON-RPC frame is not UTF-8",
        )
    })
}

/// 校验 SINGULARITY_HOME 不在当前仓库内（仓库边界以 `.git` 标记查找，找不到时
/// 以 cwd 为边界）。`home` 可能尚不存在：先对已存在前缀做 canonicalize 再比较。
fn ensure_home_outside_current_repo(home: &std::path::Path) -> Result<(), String> {
    let cwd = std::env::current_dir()
        .map_err(|error| format!("failed to read app-server cwd: {error}"))?;
    ensure_home_outside_repo(home, &cwd)
}

fn ensure_home_outside_repo(home: &std::path::Path, cwd: &std::path::Path) -> Result<(), String> {
    let root = singularity_core::find_workspace_root(cwd)
        .map_err(|error| format!("failed to locate repository boundary: {error}"))?;
    let canonical_home = canonicalize_existing_prefix(home)?;
    let canonical_root = canonicalize_existing_prefix(&root)?;
    if canonical_home.starts_with(&canonical_root) {
        return Err("SINGULARITY_HOME must not be inside the current repository".to_string());
    }
    Ok(())
}

/// 对路径的已存在前缀做 canonicalize，缺失的尾部组件原样保留（用于尚不存在的目录）。
fn canonicalize_existing_prefix(path: &std::path::Path) -> Result<std::path::PathBuf, String> {
    let mut current = path.to_path_buf();
    let mut missing = Vec::new();
    loop {
        match std::fs::canonicalize(&current) {
            Ok(mut canonical) => {
                for component in missing.iter().rev() {
                    canonical.push(component);
                }
                return Ok(canonical);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let component = current.file_name().ok_or_else(|| {
                    format!("cannot canonicalize path prefix: {}", path.display())
                })?;
                missing.push(component.to_os_string());
                if !current.pop() {
                    return Err(format!(
                        "cannot canonicalize path prefix: {}",
                        path.display()
                    ));
                }
            }
            Err(_) => {
                return Err(format!(
                    "cannot canonicalize path prefix: {}",
                    path.display()
                ));
            }
        }
    }
}

fn initialize_app_server(
    configured_db_path: &str,
    runtime_handle: tokio::runtime::Handle,
) -> Result<AppServer, String> {
    // 显式 SINGULARITY_HOME 时，先于任何目录创建校验其不在当前仓库内
    // （model 层配置校验的启动期第一道防线；违规 fail closed）。
    if std::env::var_os("SINGULARITY_HOME").is_some() {
        let home = singularity_core::user_singularity_home()
            .ok_or_else(|| "cannot resolve SINGULARITY_HOME for session index".to_string())?;
        ensure_home_outside_current_repo(&home)?;
    }
    let paths = singularity_app_server::paths::AppPaths::resolve()?;
    paths.prepare()?;
    let db_path = if std::env::var_os("SINGULARITY_APP_SERVER_DB").is_some() {
        prepare_app_server_state_paths(configured_db_path)?
    } else {
        paths
            .index_path
            .to_str()
            .map(str::to_string)
            .ok_or_else(|| SAFE_FILE_BACKED_STATE_REQUIRED.to_string())?
    };
    let store = SessionStore::open_with_initialization(&db_path, |store| {
        singularity_app_server::rebuild_session_index_from_jsonl(store, &paths.sessions_dir)
            .map_err(|error| {
                singularity_store::StoreError::InvalidState(format!(
                    "failed to rebuild app-server session index: {error}"
                ))
            })
    })
    .map_err(|error| format!("failed to open app-server index {db_path}: {error}"))?;
    // 收紧本次实际打开并创建的索引文件权限（在 Unix 系统上应用 0600/0700 权限）。
    if std::env::var_os("SINGULARITY_APP_SERVER_DB").is_some() {
        singularity_store::ensure_owner_only_file(Path::new(&db_path)).map_err(|error| {
            format!("failed to enforce owner-only app-server index {db_path}: {error}")
        })?;
    } else {
        paths.ensure_index_owner_only()?;
    }
    validate_database_file(Path::new(&db_path), false)?;
    let provider_snapshot =
        ProviderConfigSnapshot::capture(|name| std::env::var(name).ok(), Some(runtime_handle));
    Ok(AppServer::new(store, provider_snapshot).with_sessions_dir(paths.sessions_dir))
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

/// 判断单请求是否需要后台 turn worker。
fn is_turn_request(message: &JsonRpcMessage) -> bool {
    !message.is_notification()
        && matches!(
            message.method_name(),
            Some(method) if method == Method::TurnStart.as_str()
        )
}

/// 在单一 turn 工作线程内执行 turn/start，事件与最终响应顺序入队。
fn run_turn_request(
    mut worker: AppServer,
    message: JsonRpcMessage,
    outputs: mpsc::Sender<Value>,
    cancellation: AppServerCancellationHandle,
) -> Result<(), String> {
    let request_id = message.id().cloned();
    let mut output_error = None;
    let mut emit = |output: AppServerOutput| {
        if output_error.is_none()
            && let Err(error) = send_output(&outputs, &cancellation, output)
        {
            output_error = Some(error);
        }
    };
    let result = match message.method_name() {
        Some(method) if method == Method::TurnStart.as_str() => {
            worker.handle_turn_start_streaming_with_output(message, &mut emit)
        }
        _ => Err(AppServerError::Workspace(
            "streaming dispatch requires turn/start".to_string(),
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
    mut message: Value,
) -> Result<(), String> {
    loop {
        match outputs.try_send(message) {
            Ok(()) => return Ok(()),
            Err(mpsc::error::TrySendError::Full(next)) => {
                if cancellation.execution_stop_requested() {
                    return Err("stdout transport stopping".to_string());
                }
                message = next;
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                cancellation.request_execution_stop();
                return Err("stdout transport unavailable".to_string());
            }
        }
    }
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
    use serde_json::json;
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

        fn execution_stop_requested(&self) -> bool {
            self.requests.load(Ordering::SeqCst) > 0
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
    fn home_inside_current_repository_is_rejected_before_state_preparation() {
        let directory = tempfile::tempdir().expect("temp dir");
        let repo = directory.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).expect("git marker");
        let inside = repo.join("nested").join("home");
        let outside = directory.path().join("elsewhere").join("home");
        let nested_cwd = repo.join("src").join("nested");
        std::fs::create_dir_all(&nested_cwd).expect("nested cwd");

        // home 位于仓库边界内（含尚不存在的尾部组件）→ 拒绝。
        let error = ensure_home_outside_repo(&inside, &nested_cwd).expect_err("inside rejected");
        assert!(error.contains("must not be inside"), "{error}");
        // 仓库外 home → 通过。
        ensure_home_outside_repo(&outside, &nested_cwd).expect("outside accepted");
        // 无 `.git` 边界时以 cwd 为边界。若测试临时目录本身位于另一个
        // Git 仓库（例如本机 D:\Temp\.git）内，则该前提不成立，跳过这段。
        let plain = directory.path().join("plain");
        std::fs::create_dir_all(&plain).expect("plain cwd");
        if singularity_core::find_workspace_root(&plain).expect("find plain root") == plain {
            let error =
                ensure_home_outside_repo(&plain.join("home"), &plain).expect_err("cwd inside");
            assert!(error.contains("must not be inside"), "{error}");
            ensure_home_outside_repo(&outside, &plain).expect("outside cwd accepted");
        }
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

    #[test]
    fn bounded_output_queue_unblocks_a_worker_when_shutdown_is_requested() {
        let (outputs, _receiver) = test_output_channel(1);
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
        });
        attempted_receiver.recv().expect("send attempt");
        assert!(
            !sender.is_finished(),
            "full queue must initially backpressure"
        );
        cancellation.request_execution_stop();
        assert_eq!(
            sender.join().expect("sender"),
            Err("stdout transport stopping".to_string())
        );
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
    fn bounded_line_reader_strips_line_endings() {
        let mut reader = tokio::io::BufReader::new(&b"hello\r\nworld\n"[..]);
        assert_eq!(
            block_on(read_bounded_line_with_limit(&mut reader, 32))
                .expect("first line")
                .as_deref(),
            Some("hello")
        );
        assert_eq!(
            block_on(read_bounded_line_with_limit(&mut reader, 32))
                .expect("second line")
                .as_deref(),
            Some("world")
        );
        assert!(
            block_on(read_bounded_line_with_limit(&mut reader, 32))
                .expect("eof")
                .is_none()
        );
    }

    #[test]
    fn bounded_line_reader_rejects_oversized_frames() {
        let mut reader = tokio::io::BufReader::new(&b"123456789\n"[..]);
        let error = block_on(read_bounded_line_with_limit(&mut reader, 4))
            .expect_err("oversized frame must fail closed");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("exceeds 4 bytes"));
    }

    #[test]
    fn bounded_line_reader_accepts_frame_at_limit() {
        let mut reader = tokio::io::BufReader::new(&b"1234\n"[..]);
        assert_eq!(
            block_on(read_bounded_line_with_limit(&mut reader, 4))
                .expect("frame at limit")
                .as_deref(),
            Some("1234")
        );
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
    fn transport_error_carries_original_terminalization_text() {
        let original = "terminal metadata write failed: database locked";
        let response = transport_error_value(
            Some(JsonRpcId::Number(7)),
            &AppServerError::TurnTerminalization {
                stage: TurnFailureStage::TerminalOutcome,
                cause: TurnFailureCause::Store,
                failure: singularity_app_server::TurnTerminalizationFailure::Store,
                original: Some(original.to_string()),
            },
        );

        assert_eq!(response["error"]["code"], -32603);
        assert_eq!(response["error"]["message"], original);
    }

    #[test]
    fn turn_failure_events_are_queued_before_rpc_error_response() {
        let temp = tempfile::tempdir().expect("temp dir");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(&workspace).expect("workspace");
        std::fs::write(
            workspace.join("AGENTS.md"),
            vec![b'x'; singularity_core::PROJECT_INSTRUCTIONS_MAX_FILE_BYTES + 1],
        )
        .expect("oversize AGENTS.md");
        let sessions_dir = temp.path().join("sessions");
        let store = SessionStore::open(temp.path().join("index.sqlite3")).expect("store");
        let snapshot = ProviderConfigSnapshot::capture(
            |name| match name {
                "SINGULARITY_MODEL_PROVIDER" => Some("openai_compatible".to_string()),
                "SINGULARITY_MODEL" => Some("gpt-test".to_string()),
                "SINGULARITY_BASE_URL" => Some("http://127.0.0.1:1/v1".to_string()),
                "SINGULARITY_API_KEY" => Some("test-key".to_string()),
                _ => None,
            },
            None,
        );
        let mut server = AppServer::new(store, snapshot).with_sessions_dir(&sessions_dir);
        server
            .handle_json(
                r#"{"jsonrpc":"2.0","method":"initialize","id":1,"params":{"clientInfo":{"name":"test","title":"Test","version":"0.1.0"}}}"#,
            )
            .expect("initialize");
        server
            .handle_json(r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#)
            .expect("initialized");
        let started = server
            .handle_json(
                &json!({
                    "jsonrpc": "2.0",
                    "method": "thread/start",
                    "id": 2,
                    "params": {"cwd": workspace},
                })
                .to_string(),
            )
            .expect("thread/start");
        let session_id = started[1]["result"]["thread"]["thread_id"]
            .as_str()
            .expect("thread id")
            .to_string();
        let message: JsonRpcMessage = serde_json::from_value(json!({
            "jsonrpc": "2.0",
            "method": "turn/start",
            "id": 3,
            "params": {
                "threadId": session_id,
                "input": [{"type": "text", "text": "run"}],
            }
        }))
        .expect("turn/start");
        let cancellation = server.cancellation_handle();
        let (outputs, mut receiver) = mpsc::channel(16);
        run_turn_request(server, message, outputs, cancellation).expect("turn worker");
        let mut values = Vec::new();
        while let Some(value) = receiver.blocking_recv() {
            values.push(value);
        }
        let rpc_response = values
            .iter()
            .position(|value| value["id"] == 3 && value["result"].is_object())
            .expect("RPC running response");
        let turn_error = values
            .iter()
            .position(|value| value["method"] == "turn/error")
            .expect("turn/error event");
        assert!(
            rpc_response < turn_error,
            "running RPC response must precede background terminal event: {values:?}"
        );
        assert_eq!(values[rpc_response]["result"]["turn"]["status"], "running");
        assert_eq!(
            values[turn_error]["params"]["error"]["cause"],
            "project_instructions"
        );
        assert!(
            values[turn_error]["params"]["error"]["message"]
                .as_str()
                .is_some_and(|message| message.contains("project_instruction_file_too_large"))
        );
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
