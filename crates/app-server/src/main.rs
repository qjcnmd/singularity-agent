use std::io::{self, BufRead, Write};

use serde_json::Value;
use singularity_app_server::AppServer;
use singularity_core::{ErrorCode, JSON_RPC_INTERNAL_ERROR};
use singularity_protocol::JsonRpcMessage;
use singularity_store::SessionStore;

fn main() {
    let db_path = std::env::var("SINGULARITY_APP_SERVER_DB")
        .unwrap_or_else(|_| ".singularity/rust-app-server.sqlite3".to_string());
    if let Some(parent) = std::path::Path::new(&db_path).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let store = SessionStore::open(&db_path).expect("open app-server store");
    let mut server = AppServer::new(store);
    let stdin = io::stdin();
    let mut stdout = io::stdout();

    for line in stdin.lock().lines() {
        let line = line.expect("read stdin line");
        if line.trim().is_empty() {
            continue;
        }
        match server.handle_json(&line) {
            Ok(messages) => {
                for message in messages {
                    write_json_line(&mut stdout, &message).expect("write response");
                }
                stdout.flush().expect("flush response");
                if server.shutdown_requested() {
                    break;
                }
            }
            Err(error) => {
                write_transport_error(&mut stdout, recover_request_id(&line), &error)
                    .expect("write error");
                stdout.flush().expect("flush error");
            }
        }
    }
}

fn write_json_line(stdout: &mut impl Write, value: &Value) -> io::Result<()> {
    serde_json::to_writer(&mut *stdout, value)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    stdout.write_all(b"\n")
}

fn write_transport_error(
    stdout: &mut impl Write,
    id: Option<Value>,
    error: &singularity_app_server::AppServerError,
) -> io::Result<()> {
    let message = JsonRpcMessage::error(
        id,
        ErrorCode::new(JSON_RPC_INTERNAL_ERROR, error.to_string()),
    );
    write_json_line(stdout, &message.to_wire_value())
}

fn recover_request_id(line: &str) -> Option<Value> {
    let value = serde_json::from_str::<Value>(line).ok()?;
    value.get("id").cloned()
}
