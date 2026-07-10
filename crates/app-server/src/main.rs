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
    let db_path = std::env::var("SINGULARITY_APP_SERVER_DB")
        .unwrap_or_else(|_| ".singularity/rust-app-server.sqlite3".to_string());
    if let Some(parent) = std::path::Path::new(&db_path).parent() {
        std::fs::create_dir_all(parent).expect("create app-server state directory");
    }
    let store = SessionStore::open(&db_path).expect("open app-server store");
    let provider_snapshot = ProviderConfigSnapshot::capture(|name| std::env::var(name).ok());
    let mut server = AppServer::new(store, provider_snapshot);
    let (output_tx, output_rx) = mpsc::channel::<Value>();
    let writer = thread::spawn(move || {
        let mut stdout = io::stdout().lock();
        for message in output_rx {
            write_json_line(&mut stdout, &message).expect("write response");
            stdout.flush().expect("flush response");
        }
    });
    let mut turn_workers = Vec::new();
    let stdin = io::stdin();

    for line in stdin.lock().lines() {
        reap_finished_workers(&mut turn_workers);
        let line = line.expect("read stdin line");
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
        .expect("cancel active turns during shutdown");
    for worker in turn_workers {
        worker.join().expect("turn worker joins");
    }
    drop(output_tx);
    writer.join().expect("stdout writer joins");
}

fn reap_finished_workers(workers: &mut Vec<JoinHandle<()>>) {
    let mut active = Vec::with_capacity(workers.len());
    for worker in workers.drain(..) {
        if worker.is_finished() {
            worker.join().expect("turn worker joins");
        } else {
            active.push(worker);
        }
    }
    *workers = active;
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
