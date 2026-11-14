//! Electron 的 stdio AppServer 入口。

use std::path::{Path, PathBuf};
use std::sync::Arc;

mod desktop;
mod runtime;

use crate::desktop::AppServer;
use crate::desktop::workspace_store::WorkspaceStore;
use crate::runtime::{SESSIONS_DIR_NAME, ThreadCatalog};
use singularity_model::ModelConfigManager;

pub(crate) const PROGRAM_NAME: &str = env!("CARGO_BIN_NAME");

fn main() {
    if let Err(message) = run() {
        eprintln!("{PROGRAM_NAME}: {message}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let (home, _data_lock) = lock_data_directory()?;
    let runtime = tokio::runtime::Runtime::new().map_err(|error| error.to_string())?;
    let (app, events) = prepare_desktop(&home)?;
    runtime.block_on(desktop::run(app, events))
}

/// 在整个进程生命周期内持有一把 OS 锁；锁文件还留在磁盘上，不代表有程序正持有它。
fn lock_data_directory() -> Result<(PathBuf, std::fs::File), String> {
    let home = singularity_core::resolve_home()?;
    std::fs::create_dir_all(&home)
        .map_err(|error| format!("failed to create {}: {error}", home.display()))?;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(home.join("instance.lock"))
        .map_err(|e| e.to_string())?;
    file.try_lock().map_err(|e| match e {
        std::fs::TryLockError::WouldBlock => {
            format!("数据目录已被另一个 Singularity 程序使用：{}", home.display())
        }
        std::fs::TryLockError::Error(e) => {
            format!("cannot lock data directory {}: {e}", home.display())
        }
    })?;
    Ok((home, file))
}

/// 装配工作台共享组件，直接交给应用服务持有。
fn prepare_desktop(
    home: &Path,
) -> Result<
    (Arc<AppServer>, tokio::sync::mpsc::UnboundedReceiver<singularity_protocol::StreamEnvelope>),
    String,
> {
    let sessions_dir = home.join(SESSIONS_DIR_NAME);
    let models = Arc::new(ModelConfigManager::open(home.to_path_buf()));
    let mcp = Arc::new(singularity_mcp::McpManager::open(home.to_path_buf()));
    let catalog = ThreadCatalog::new(sessions_dir);
    let workspaces = WorkspaceStore::open(home)?;
    let (stream, events) = tokio::sync::mpsc::unbounded_channel();
    Ok((AppServer::new(catalog, workspaces, models, mcp, home.to_path_buf(), stream), events))
}
