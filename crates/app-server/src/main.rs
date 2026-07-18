//! `AppServer` 的标准输入输出（stdio）传输层。
//!
//! 输入独立读取；请求工作线程准入队列和传输队列均有界，由单一写入方串行化 JSON 行输出，
//! 并在背压时拒绝继续处理。

use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use cap_fs_ext::{FollowSymlinks, MetadataExt as CapMetadataExt, OpenOptionsFollowExt};
use cap_std::fs::{Dir as CapabilityDir, OpenOptions as CapabilityOpenOptions};
use serde_json::Value;
use singularity_app_server::{AppServer, AppServerCancellationHandle, AppServerError};
use singularity_core::{ErrorCode, JSON_RPC_INTERNAL_ERROR};
use singularity_model::{PROVIDER_CAPABILITY_CACHE_FILE_NAME, ProviderConfigSnapshot};
use singularity_protocol::{JsonRpcMessage, Method};
use singularity_store::SessionStore;

const INPUT_POLL_INTERVAL: Duration = Duration::from_millis(10);
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);
const MAX_REQUEST_WORKERS: usize = 16;
const INPUT_QUEUE_CAPACITY: usize = 64;
const OUTPUT_QUEUE_CAPACITY: usize = 256;
const REQUEST_CAPACITY_EXCEEDED: &str = "AppServer request capacity exceeded";
const FILE_BACKED_STORE_REQUIRED: &str =
    "app-server requires a file-backed SINGULARITY_APP_SERVER_DB";
const SAFE_FILE_BACKED_STATE_REQUIRED: &str =
    "app-server requires a canonical regular file-backed state database";
const CACHE_TEMP_FILE_PREFIX: &str = ".provider-capability-cache.json.tmp-";
const CACHE_KEY_LOCK_FILE_PREFIX: &str = ".provider-capability-cache.key-lock-";

/// 启动标准输入输出服务；传输或生命周期关闭失败时以非零状态退出。
fn main() {
    if let Err(error) = run() {
        eprintln!("app-server error: {error}");
        std::process::exit(1);
    }
}

/// 负责 `stdin` 读取、请求工作线程准入、`stdout` 串行化和优雅关闭。
fn run() -> Result<(), String> {
    let configured_db_path = std::env::var("SINGULARITY_APP_SERVER_DB")
        .unwrap_or_else(|_| ".singularity/rust-app-server.sqlite3".to_string());
    let (db_path, capability_cache_path) = prepare_app_server_state_paths(&configured_db_path)?;
    let store = SessionStore::open(&db_path)
        .map_err(|error| format!("failed to open app-server store {db_path}: {error}"))?;
    validate_database_file(Path::new(&db_path), false)?;
    store
        .recover_unowned_workspace_executions()
        .map_err(|error| format!("failed to recover app-server thread executions: {error}"))?;
    let provider_snapshot = ProviderConfigSnapshot::capture_with_cache_path(
        |name| std::env::var(name).ok(),
        Some(capability_cache_path),
    );
    let mut server = AppServer::new(store, provider_snapshot);
    let cancellation = server.cancellation_handle();
    let (output_tx, output_rx) = mpsc::sync_channel::<Value>(OUTPUT_QUEUE_CAPACITY);
    let (writer_error_tx, writer_error_rx) = mpsc::channel::<String>();
    let writer_cancellation = cancellation.clone();
    let writer = thread::spawn(move || -> Result<(), String> {
        let mut stdout = io::stdout().lock();
        for message in output_rx {
            let result = write_json_line(&mut stdout, &message)
                .map_err(|error| format!("failed to write response: {error}"))
                .and_then(|()| {
                    stdout
                        .flush()
                        .map_err(|error| format!("failed to flush response: {error}"))
                });
            if let Err(error) = result {
                let _ = writer_cancellation.request_execution_stop();
                let _ = writer_error_tx.send(error.clone());
                return Err(error);
            }
        }
        Ok(())
    });
    let (input_tx, input_rx) =
        mpsc::sync_channel::<Result<Option<String>, String>>(INPUT_QUEUE_CAPACITY);
    thread::spawn(move || {
        for line in io::stdin().lock().lines() {
            if input_tx
                .send(
                    line.map(Some)
                        .map_err(|error| format!("failed to read stdin: {error}")),
                )
                .is_err()
            {
                return;
            }
        }
        let _ = input_tx.send(Ok(None));
    });
    let mut request_workers = Vec::new();
    let mut terminal_error = None;

    loop {
        if let Err(error) = reap_finished_workers(&mut request_workers) {
            terminal_error = Some(error);
            break;
        }
        match writer_error_rx.try_recv() {
            Ok(error) => {
                terminal_error = Some(error);
                break;
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => {
                terminal_error = Some("stdout writer stopped unexpectedly".to_string());
                break;
            }
        }
        let line = match input_rx.recv_timeout(INPUT_POLL_INTERVAL) {
            Ok(Ok(Some(line))) => line,
            Ok(Ok(None)) => break,
            Ok(Err(error)) => {
                terminal_error = Some(error);
                break;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                terminal_error = Some("stdin reader stopped unexpectedly".to_string());
                break;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        let message = match serde_json::from_str::<JsonRpcMessage>(&line) {
            Ok(message) => message,
            Err(error) => {
                if let Err(error) = send_output(
                    &output_tx,
                    &cancellation,
                    transport_error_value(
                        recover_request_id(&line),
                        &AppServerError::InvalidJson(error),
                    ),
                ) {
                    terminal_error = Some(error);
                    break;
                }
                continue;
            }
        };
        let request_id = message.id.clone();
        if is_request_worker_method(&message) && server.ready_for_turn_worker() {
            if request_workers.len() >= MAX_REQUEST_WORKERS {
                if let Err(error) = send_output(
                    &output_tx,
                    &cancellation,
                    JsonRpcMessage::error(
                        request_id,
                        ErrorCode::invalid_request(REQUEST_CAPACITY_EXCEEDED),
                    )
                    .to_wire_value(),
                ) {
                    terminal_error = Some(error);
                    break;
                }
                continue;
            }
            match server.turn_worker() {
                Ok(worker) => {
                    let worker_output_tx = output_tx.clone();
                    let worker_cancellation = cancellation.clone();
                    match thread::Builder::new()
                        .name("singularity-request".to_string())
                        .spawn(move || {
                            run_request_worker(
                                worker,
                                message,
                                worker_output_tx,
                                worker_cancellation,
                            )
                        }) {
                        Ok(worker) => request_workers.push(worker),
                        Err(error) => {
                            if let Err(error) = send_output(
                                &output_tx,
                                &cancellation,
                                internal_error_value(
                                    request_id,
                                    format!("failed to start request worker: {error}"),
                                ),
                            ) {
                                terminal_error = Some(error);
                                break;
                            }
                        }
                    }
                }
                Err(error) => {
                    if let Err(error) = send_output(
                        &output_tx,
                        &cancellation,
                        transport_error_value(request_id, &error),
                    ) {
                        terminal_error = Some(error);
                        break;
                    }
                }
            }
        } else {
            match server.handle(message) {
                Ok(messages) => {
                    for message in messages {
                        if let Err(error) = send_output(&output_tx, &cancellation, message) {
                            terminal_error = Some(error);
                            break;
                        }
                    }
                }
                Err(error) => {
                    if let Err(error) = send_output(
                        &output_tx,
                        &cancellation,
                        transport_error_value(request_id, &error),
                    ) {
                        terminal_error = Some(error);
                    }
                }
            }
            if terminal_error.is_some() {
                break;
            }
        }
        if server.shutdown_requested() {
            break;
        }
    }

    let shutdown_deadline = Instant::now() + SHUTDOWN_GRACE;
    let request_worker_result = server
        .request_execution_stop()
        .map_err(|error| format!("failed to stop executions during shutdown: {error}"))
        .and_then(|()| {
            join_request_workers_during_shutdown(&mut request_workers, shutdown_deadline)
        });
    drop(output_tx);
    let writer_result = join_writer_during_shutdown(writer, shutdown_deadline);
    request_worker_result?;
    if let Some(error) = terminal_error {
        return Err(error);
    }
    writer_result?;
    Ok(())
}

fn is_request_worker_method(message: &JsonRpcMessage) -> bool {
    matches!(
        message.method.as_deref(),
        Some(method)
            if method == Method::TurnStart.as_str()
                || method == Method::ApprovalDecision.as_str()
    )
}

/// 分发一个由工作线程负责的请求，并通过共享有界队列发送全部响应。
fn run_request_worker(
    mut worker: AppServer,
    message: JsonRpcMessage,
    output_tx: SyncSender<Value>,
    cancellation: AppServerCancellationHandle,
) -> Result<(), String> {
    let request_id = message.id.clone();
    let mut output_error = None;
    let result = if message.method.as_deref() == Some(Method::TurnStart.as_str()) {
        worker.handle_turn_start_streaming(message, |message| {
            if output_error.is_none()
                && let Err(error) = send_output(&output_tx, &cancellation, message)
            {
                output_error = Some(error);
            }
        })
    } else {
        worker.handle(message).map(|messages| {
            for message in messages {
                if let Err(error) = send_output(&output_tx, &cancellation, message) {
                    output_error = Some(error);
                    break;
                }
            }
        })
    };
    if let Some(error) = output_error {
        return Err(error);
    }
    if let Err(error) = result {
        send_output(
            &output_tx,
            &cancellation,
            transport_error_value(request_id, &error),
        )?;
    }
    Ok(())
}

fn reap_finished_workers(workers: &mut Vec<JoinHandle<Result<(), String>>>) -> Result<(), String> {
    let mut active = Vec::with_capacity(workers.len());
    let mut first_error = None;
    for worker in workers.drain(..) {
        if worker.is_finished() {
            if let Err(error) = join_request_worker(worker)
                && first_error.is_none()
            {
                first_error = Some(error);
            }
        } else {
            active.push(worker);
        }
    }
    *workers = active;
    match first_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

fn join_request_workers_during_shutdown(
    workers: &mut Vec<JoinHandle<Result<(), String>>>,
    deadline: Instant,
) -> Result<(), String> {
    let mut first_error = None;
    while !workers.is_empty() {
        if let Err(error) = reap_finished_workers(workers)
            && first_error.is_none()
        {
            first_error = Some(error);
        }
        if workers.is_empty() {
            break;
        }
        if Instant::now() >= deadline {
            let timeout = format!(
                "timed out waiting for {} request worker(s) during shutdown",
                workers.len()
            );
            return Err(match first_error {
                Some(error) => format!("{error}; {timeout}"),
                None => timeout,
            });
        }
        thread::sleep(INPUT_POLL_INTERVAL);
    }
    match first_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

fn join_writer_during_shutdown(
    writer: JoinHandle<Result<(), String>>,
    deadline: Instant,
) -> Result<(), String> {
    while !writer.is_finished() {
        if Instant::now() >= deadline {
            return Err("timed out waiting for stdout writer during shutdown".to_string());
        }
        thread::sleep(INPUT_POLL_INTERVAL);
    }
    writer
        .join()
        .map_err(|_| "stdout writer panicked".to_string())?
}

fn join_request_worker(worker: JoinHandle<Result<(), String>>) -> Result<(), String> {
    worker
        .join()
        .map_err(|_| "request worker panicked".to_string())?
}

/// 入队一个响应；检测到 `stdout` 背压或断开时停止执行。
fn send_output(
    sender: &SyncSender<Value>,
    cancellation: &AppServerCancellationHandle,
    message: Value,
) -> Result<(), String> {
    sender.try_send(message).map_err(|error| {
        let _ = cancellation.request_execution_stop();
        match error {
            TrySendError::Full(_) => "stdout transport backpressure exceeded".to_string(),
            TrySendError::Disconnected(_) => "stdout transport unavailable".to_string(),
        }
    })
}

/// 将一个 JSON-RPC 值严格串行化为一条以换行分隔的 `stdout` 记录。
fn write_json_line(stdout: &mut impl Write, value: &Value) -> io::Result<()> {
    serde_json::to_writer(&mut *stdout, value)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    stdout.write_all(b"\n")
}

fn transport_error_value(id: Option<Value>, error: &AppServerError) -> Value {
    internal_error_value(id, error.to_string())
}

fn internal_error_value(id: Option<Value>, message: impl Into<String>) -> Value {
    JsonRpcMessage::error(id, ErrorCode::new(JSON_RPC_INTERNAL_ERROR, message)).to_wire_value()
}

fn recover_request_id(line: &str) -> Option<Value> {
    let value = serde_json::from_str::<Value>(line).ok()?;
    value.get("id").cloned()
}

fn resolve_app_server_state_paths(configured_db_path: &str) -> Result<(String, PathBuf), String> {
    if is_unsupported_sqlite_database_path(configured_db_path) {
        return Err(FILE_BACKED_STORE_REQUIRED.to_string());
    }
    let db_path = configured_db_path.trim();
    let database_name = Path::new(db_path)
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| SAFE_FILE_BACKED_STATE_REQUIRED.to_string())?;
    validate_database_name(database_name)?;
    let parent = Path::new(db_path)
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let cache_path = parent.join(PROVIDER_CAPABILITY_CACHE_FILE_NAME);
    Ok((db_path.to_string(), cache_path))
}

fn is_unsupported_sqlite_database_path(configured_db_path: &str) -> bool {
    let trimmed = configured_db_path.trim();
    let lower = trimmed.to_ascii_lowercase();
    trimmed.eq_ignore_ascii_case(":memory:")
        || lower.starts_with("file:")
        || lower.starts_with("sqlite:")
}

fn prepare_app_server_state_paths(configured_db_path: &str) -> Result<(String, PathBuf), String> {
    let (raw_db_path, _) = resolve_app_server_state_paths(configured_db_path)?;
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
    let cache_path = canonical_parent.join(PROVIDER_CAPABILITY_CACHE_FILE_NAME);
    Ok((
        database_path
            .to_str()
            .ok_or_else(|| SAFE_FILE_BACKED_STATE_REQUIRED.to_string())?
            .to_string(),
        cache_path,
    ))
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
    if normalized.is_empty()
        || normalized == PROVIDER_CAPABILITY_CACHE_FILE_NAME
        || normalized == "provider-capability-cache.lock"
        || normalized.starts_with(CACHE_TEMP_FILE_PREFIX)
        || normalized.starts_with(CACHE_KEY_LOCK_FILE_PREFIX)
    {
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

    #[test]
    fn state_path_rejects_sqlite_uri_before_cache_injection() {
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
    fn state_path_injects_cache_next_to_file_backed_database() {
        let (db_path, cache_path) =
            resolve_app_server_state_paths("state/rust-app-server.sqlite3").expect("state paths");
        assert_eq!(db_path, "state/rust-app-server.sqlite3");
        assert_eq!(
            cache_path,
            PathBuf::from("state").join(PROVIDER_CAPABILITY_CACHE_FILE_NAME)
        );
    }

    #[test]
    fn prepared_state_paths_use_the_canonical_directory() {
        let directory = tempfile::tempdir().expect("state directory");
        let configured = directory.path().join("nested").join("sessions.sqlite3");
        let (db_path, cache_path) =
            prepare_app_server_state_paths(configured.to_str().expect("configured path"))
                .expect("prepared state paths");
        let canonical_parent = std::fs::canonicalize(configured.parent().expect("parent"))
            .expect("canonical state directory");
        assert_eq!(
            Path::new(&db_path).parent(),
            Some(canonical_parent.as_path())
        );
        assert_eq!(cache_path.parent(), Some(canonical_parent.as_path()));
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
    fn state_path_rejects_cache_lock_and_temp_name_collisions() {
        for name in [
            PROVIDER_CAPABILITY_CACHE_FILE_NAME,
            "provider-capability-cache.lock",
            ".provider-capability-cache.key-lock-00.lock",
            ".provider-capability-cache.json.tmp-owned",
        ] {
            let error = resolve_app_server_state_paths(name)
                .expect_err("reserved cache state name rejected");
            assert_eq!(error, SAFE_FILE_BACKED_STATE_REQUIRED);
        }
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
    fn stdout_queue_fails_closed_when_backpressure_limit_is_reached() {
        let store = SessionStore::open(":memory:").expect("store");
        let server = AppServer::new(store, ProviderConfigSnapshot::capture(|_| None));
        let cancellation = server.cancellation_handle();
        let (sender, _receiver) = mpsc::sync_channel(1);
        send_output(&sender, &cancellation, serde_json::json!({"first": true}))
            .expect("first output fits");

        let error = send_output(&sender, &cancellation, serde_json::json!({"second": true}))
            .expect_err("full queue must fail closed");

        assert_eq!(error, "stdout transport backpressure exceeded");
    }

    #[test]
    fn stdout_writer_join_obeys_the_shutdown_deadline() {
        let (release_sender, release_receiver) = mpsc::channel();
        let writer = thread::spawn(move || {
            release_receiver.recv().expect("release writer");
            Ok(())
        });

        let error = join_writer_during_shutdown(writer, Instant::now() + Duration::from_millis(20))
            .expect_err("stalled writer must not outlive the deadline");
        release_sender.send(()).expect("release detached writer");

        assert_eq!(error, "timed out waiting for stdout writer during shutdown");
    }

    #[test]
    fn failed_worker_does_not_drop_other_active_worker_handles() {
        let failed = thread::spawn(|| Err("worker failed".to_string()));
        while !failed.is_finished() {
            thread::yield_now();
        }
        let (release_sender, release_receiver) = mpsc::channel();
        let active = thread::spawn(move || {
            release_receiver.recv().expect("release active worker");
            Ok(())
        });
        let mut workers = vec![failed, active];

        let error = reap_finished_workers(&mut workers).expect_err("failed worker is reported");

        assert_eq!(error, "worker failed");
        assert_eq!(workers.len(), 1);
        release_sender.send(()).expect("release active worker");
        join_request_workers_during_shutdown(&mut workers, Instant::now() + Duration::from_secs(1))
            .expect("remaining worker is still tracked");
    }
}
