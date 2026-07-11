use std::io::{self, BufRead, Write};
use std::sync::mpsc::{self, Sender};
use std::thread::{self, JoinHandle};

use serde_json::Value;
use singularity_app_server::{AppServer, AppServerError};
use singularity_core::{ErrorCode, JSON_RPC_INTERNAL_ERROR};
use singularity_model::ProviderConfigSnapshot;
use singularity_protocol::{JsonRpcMessage, Method};
use singularity_store::SessionStore;

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
        .recover_incomplete_approval_executions()
        .map_err(|error| format!("failed to recover app-server approval executions: {error}"))?;
    let provider_snapshot = ProviderConfigSnapshot::capture(|name| std::env::var(name).ok());
    let mut server = AppServer::new(store, provider_snapshot);
    let (output_tx, output_rx) = mpsc::channel::<Value>();
    let writer = thread::spawn(move || -> Result<(), String> {
        let mut stdout = io::stdout().lock();
        for message in output_rx {
            write_json_line(&mut stdout, &message)
                .map_err(|error| format!("failed to write response: {error}"))?;
            stdout
                .flush()
                .map_err(|error| format!("failed to flush response: {error}"))?;
        }
        Ok(())
    });
    let mut turn_workers = Vec::new();
    let stdin = io::stdin();

    for line in stdin.lock().lines() {
        reap_finished_workers(&mut turn_workers)?;
        let line = line.map_err(|error| format!("failed to read stdin: {error}"))?;
        if line.trim().is_empty() {
            continue;
        }
        let message = match serde_json::from_str::<JsonRpcMessage>(&line) {
            Ok(message) => message,
            Err(error) => {
                send_output(
                    &output_tx,
                    transport_error_value(
                        recover_request_id(&line),
                        &AppServerError::InvalidJson(error),
                    ),
                );
                continue;
            }
        };
        let request_id = message.id.clone();
        let is_turn_start = message.method.as_deref() == Some(Method::TurnStart.as_str());
        if is_turn_start && server.ready_for_turn_worker() {
            match server.turn_worker() {
                Ok(mut worker) => {
                    let output_tx = output_tx.clone();
                    turn_workers.push(thread::spawn(move || {
                        let result = worker.handle_turn_start_streaming(message, |message| {
                            send_output(&output_tx, message);
                        });
                        if let Err(error) = result {
                            send_output(&output_tx, transport_error_value(request_id, &error));
                        }
                    }));
                }
                Err(error) => send_output(&output_tx, transport_error_value(request_id, &error)),
            }
        } else {
            match server.handle(message) {
                Ok(messages) => {
                    for message in messages {
                        send_output(&output_tx, message);
                    }
                }
                Err(error) => send_output(&output_tx, transport_error_value(request_id, &error)),
            }
            if server.shutdown_requested() {
                break;
            }
        }
    }

    server
        .cancel_active_turns()
        .map_err(|error| format!("failed to cancel active turns during shutdown: {error}"))?;
    for worker in turn_workers {
        join_turn_worker(worker)?;
    }
    drop(output_tx);
    writer
        .join()
        .map_err(|_| "stdout writer panicked".to_string())??;
    Ok(())
}

fn reap_finished_workers(workers: &mut Vec<JoinHandle<()>>) -> Result<(), String> {
    let mut active = Vec::with_capacity(workers.len());
    for worker in workers.drain(..) {
        if worker.is_finished() {
            join_turn_worker(worker)?;
        } else {
            active.push(worker);
        }
    }
    *workers = active;
    Ok(())
}

fn join_turn_worker(worker: JoinHandle<()>) -> Result<(), String> {
    worker
        .join()
        .map_err(|_| "turn worker panicked".to_string())
}

fn send_output(sender: &Sender<Value>, message: Value) {
    let _ = sender.send(message);
}

fn write_json_line(stdout: &mut impl Write, value: &Value) -> io::Result<()> {
    serde_json::to_writer(&mut *stdout, value)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    stdout.write_all(b"\n")
}

fn transport_error_value(id: Option<Value>, error: &AppServerError) -> Value {
    JsonRpcMessage::error(
        id,
        ErrorCode::new(JSON_RPC_INTERNAL_ERROR, error.to_string()),
    )
    .to_wire_value()
}

fn recover_request_id(line: &str) -> Option<Value> {
    let value = serde_json::from_str::<Value>(line).ok()?;
    value.get("id").cloned()
}
