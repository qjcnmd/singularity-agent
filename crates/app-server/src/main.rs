//! AppServer 的 stdio 传输层。
//!
//! 输入独立读取；request-worker 准入队列和传输队列均有界，由单一 writer 串行化 JSON 行输出，
//! 并在背压时保持 fail-closed。

use std::io::{self, BufRead, Write};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde_json::Value;
use singularity_app_server::{AppServer, AppServerCancellationHandle, AppServerError};
use singularity_core::{ErrorCode, JSON_RPC_INTERNAL_ERROR};
use singularity_model::ProviderConfigSnapshot;
use singularity_protocol::{JsonRpcMessage, Method};
use singularity_store::SessionStore;

const INPUT_POLL_INTERVAL: Duration = Duration::from_millis(10);
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);
const MAX_REQUEST_WORKERS: usize = 16;
const INPUT_QUEUE_CAPACITY: usize = 64;
const OUTPUT_QUEUE_CAPACITY: usize = 256;
const REQUEST_CAPACITY_EXCEEDED: &str = "AppServer request capacity exceeded";

/// 启动 stdio server；传输或生命周期关闭失败时以非零状态退出。
fn main() {
    if let Err(error) = run() {
        eprintln!("app-server error: {error}");
        std::process::exit(1);
    }
}

/// 负责 stdin 读取、request-worker 准入、stdout 串行化和优雅关闭。
fn run() -> Result<(), String> {
    let db_path = std::env::var("SINGULARITY_APP_SERVER_DB")
        .unwrap_or_else(|_| ".singularity/rust-app-server.sqlite3".to_string());
    if let Some(parent) = std::path::Path::new(&db_path)
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent).map_err(|error| {
            format!(
                "failed to create app-server state directory {}: {error}",
                parent.display()
            )
        })?;
    }
    let store = SessionStore::open(&db_path)
        .map_err(|error| format!("failed to open app-server store {db_path}: {error}"))?;
    store
        .recover_unowned_workspace_executions()
        .map_err(|error| format!("failed to recover app-server thread executions: {error}"))?;
    let provider_snapshot = ProviderConfigSnapshot::capture(|name| std::env::var(name).ok());
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

/// 分发一个由 worker 负责的请求，并通过共享有界队列发送全部响应。
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

/// 入队一个响应；检测到 stdout 背压或断开时停止执行。
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

/// 将一个 JSON-RPC 值严格串行化为一条以换行分隔的 stdout 记录。
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

#[cfg(test)]
mod tests {
    use super::*;

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
