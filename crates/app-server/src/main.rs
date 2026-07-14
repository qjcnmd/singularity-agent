use std::io::{self, BufRead, Write};
use std::sync::mpsc::{self, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde_json::Value;
use singularity_app_server::{AppServer, AppServerCancellationHandle, AppServerError};
use singularity_core::{ErrorCode, JSON_RPC_INTERNAL_ERROR};
use singularity_model::ProviderConfigSnapshot;
use singularity_protocol::{JsonRpcMessage, Method};
use singularity_store::SessionStore;

const INPUT_POLL_INTERVAL: Duration = Duration::from_millis(10);
const REQUEST_WORKER_SHUTDOWN_GRACE: Duration = Duration::from_secs(5);
const MAX_REQUEST_WORKERS: usize = 16;
const REQUEST_CAPACITY_EXCEEDED: &str = "AppServer request capacity exceeded";

fn main() {
    if let Err(error) = run() {
        eprintln!("app-server error: {error}");
        std::process::exit(1);
    }
}

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
        .recover_unowned_thread_executions()
        .map_err(|error| format!("failed to recover app-server thread executions: {error}"))?;
    let provider_snapshot = ProviderConfigSnapshot::capture(|name| std::env::var(name).ok());
    let mut server = AppServer::new(store, provider_snapshot);
    let cancellation = server.cancellation_handle();
    let (output_tx, output_rx) = mpsc::channel::<Value>();
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
                let _ = writer_cancellation.cancel_active_turns();
                let _ = writer_error_tx.send(error.clone());
                return Err(error);
            }
        }
        Ok(())
    });
    let (input_tx, input_rx) = mpsc::channel::<Result<Option<String>, String>>();
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

    server
        .cancel_active_turns()
        .map_err(|error| format!("failed to cancel active turns during shutdown: {error}"))?;
    join_request_workers_during_shutdown(&server, &mut request_workers)?;
    drop(output_tx);
    let writer_result = writer
        .join()
        .map_err(|_| "stdout writer panicked".to_string())?;
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

fn run_request_worker(
    mut worker: AppServer,
    message: JsonRpcMessage,
    output_tx: Sender<Value>,
    cancellation: AppServerCancellationHandle,
) {
    let request_id = message.id.clone();
    let result = if message.method.as_deref() == Some(Method::TurnStart.as_str()) {
        worker.handle_turn_start_streaming(message, |message| {
            let _ = send_output(&output_tx, &cancellation, message);
        })
    } else {
        worker.handle(message).map(|messages| {
            for message in messages {
                let _ = send_output(&output_tx, &cancellation, message);
            }
        })
    };
    if let Err(error) = result {
        let _ = send_output(
            &output_tx,
            &cancellation,
            transport_error_value(request_id, &error),
        );
    }
}

fn reap_finished_workers(workers: &mut Vec<JoinHandle<()>>) -> Result<(), String> {
    let mut active = Vec::with_capacity(workers.len());
    for worker in workers.drain(..) {
        if worker.is_finished() {
            join_request_worker(worker)?;
        } else {
            active.push(worker);
        }
    }
    *workers = active;
    Ok(())
}

fn join_request_workers_during_shutdown(
    server: &AppServer,
    workers: &mut Vec<JoinHandle<()>>,
) -> Result<(), String> {
    let deadline = Instant::now() + REQUEST_WORKER_SHUTDOWN_GRACE;
    while !workers.is_empty() {
        server
            .cancel_active_turns()
            .map_err(|error| format!("failed to cancel active request during shutdown: {error}"))?;
        reap_finished_workers(workers)?;
        if workers.is_empty() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "timed out waiting for {} request worker(s) during shutdown",
                workers.len()
            ));
        }
        thread::sleep(INPUT_POLL_INTERVAL);
    }
    Ok(())
}

fn join_request_worker(worker: JoinHandle<()>) -> Result<(), String> {
    worker
        .join()
        .map_err(|_| "request worker panicked".to_string())
}

fn send_output(
    sender: &Sender<Value>,
    cancellation: &AppServerCancellationHandle,
    message: Value,
) -> Result<(), String> {
    sender.send(message).map_err(|_| {
        let _ = cancellation.cancel_active_turns();
        "stdout transport unavailable".to_string()
    })
}

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
