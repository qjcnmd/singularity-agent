use std::io::{self, BufRead, Write};

use singularity_app_server::AppServer;
use singularity_core::JSON_RPC_INTERNAL_ERROR;
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
                    writeln!(stdout, "{message}").expect("write response");
                }
                stdout.flush().expect("flush response");
            }
            Err(error) => {
                writeln!(
                    stdout,
                    "{{\"error\":{{\"code\":{JSON_RPC_INTERNAL_ERROR},\"message\":\"{error}\"}}}}"
                )
                .expect("write error");
                stdout.flush().expect("flush error");
            }
        }
    }
}
