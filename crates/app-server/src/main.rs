use std::io::{self, BufRead, Write};

use singularity_agent::PythonSidecarConfig;
use singularity_app_server::AppServer;
use singularity_core::JSON_RPC_INTERNAL_ERROR;
use singularity_store::SessionStore;

const PYTHON_SIDECAR_ENV: &str = "SINGULARITY_PYTHON_SIDECAR";
const PYTHON_SIDECAR_BIN_ENV: &str = "SINGULARITY_PYTHON_SIDECAR_BIN";
const PYTHON_SIDECAR_MODULE_ENV: &str = "SINGULARITY_PYTHON_SIDECAR_MODULE";
const PYTHON_SIDECAR_PROJECT_ROOT_ENV: &str = "SINGULARITY_SIDECAR_PROJECT_ROOT";
const PYTHONPATH_ENV: &str = "PYTHONPATH";

fn main() {
    let db_path = std::env::var("SINGULARITY_APP_SERVER_DB")
        .unwrap_or_else(|_| ".singularity/rust-app-server.sqlite3".to_string());
    if let Some(parent) = std::path::Path::new(&db_path).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let store = SessionStore::open(&db_path).expect("open app-server store");
    let mut server = AppServer::new(store);
    if let Some(config) = python_sidecar_config() {
        server = server.with_python_sidecar(config);
    }
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

fn python_sidecar_config() -> Option<PythonSidecarConfig> {
    if std::env::var(PYTHON_SIDECAR_ENV).ok().as_deref() != Some("1") {
        return None;
    }
    let project_root = std::env::var(PYTHON_SIDECAR_PROJECT_ROOT_ENV)
        .map(std::path::PathBuf::from)
        .or_else(|_| std::env::current_dir())
        .unwrap_or_else(|_| std::path::PathBuf::from("."));
    let mut config = PythonSidecarConfig::new(project_root);
    if let Ok(python_bin) = std::env::var(PYTHON_SIDECAR_BIN_ENV) {
        config.python_bin = python_bin;
    }
    if let Ok(module) = std::env::var(PYTHON_SIDECAR_MODULE_ENV) {
        config.module = module;
    }
    if let Ok(python_path) = std::env::var(PYTHONPATH_ENV) {
        config.python_path = Some(std::path::PathBuf::from(python_path));
    }
    Some(config)
}
