//! Electron 的 stdio AppServer 入口。

use std::path::{Path, PathBuf};
use std::sync::Arc;

mod desktop;
mod logging;
mod runtime;

use crate::desktop::AppServer;
use crate::desktop::workspace_store::WorkspaceStore;
use crate::runtime::{SESSIONS_DIR_NAME, ThreadCatalog};
use model::ModelConfigManager;

pub(crate) const PROGRAM_NAME: &str = env!("CARGO_BIN_NAME");
/// 后端事件流总容量；最后一段预留给控制与终态事件，高频进度在接近满载时让路。
pub(crate) const STREAM_CHANNEL_CAPACITY: usize = 4096;

fn main() {
    if let Err(message) = run() {
        tracing::error!(error = %message, "backend exited with error");
        eprintln!("{PROGRAM_NAME}: {message}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let (home, _data_lock) = lock_data_directory()?;
    logging::init(&home);
    tracing::info!(home = %home.display(), app = app_core::identity::APP_NAME, "backend starting");
    let runtime = tokio::runtime::Runtime::new().map_err(|error| error.to_string())?;
    let (app, events) = prepare_desktop(&home)?;
    let result = runtime.block_on(desktop::run(app, events));
    runtime.shutdown_background();
    tracing::info!(ok = result.is_ok(), app = app_core::identity::APP_NAME, "backend stopped");
    result
}

/// 在整个进程生命周期内持有一把 OS 锁；锁文件还留在磁盘上，不代表有程序正持有它。
fn lock_data_directory() -> Result<(PathBuf, std::fs::File), String> {
    let home = app_core::resolve_home()?;
    std::fs::create_dir_all(&home)
        .map_err(|error| format!("failed to create {}: {error}", home.display()))?;
    let lock_path = home.join("instance.lock");
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|e| format!("failed to open {}: {e}", lock_path.display()))?;
    file.try_lock().map_err(|e| match e {
        std::fs::TryLockError::WouldBlock => {
            format!("数据目录已被另一个 {} 程序使用：{}", app_core::identity::APP_NAME, home.display())
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
) -> Result<(Arc<AppServer>, tokio::sync::mpsc::Receiver<protocol::StreamEnvelope>), String> {
    let sessions_dir = home.join(SESSIONS_DIR_NAME);
    let models = Arc::new(ModelConfigManager::open(home.to_path_buf()));
    let mcp = Arc::new(mcp::McpManager::open(home.to_path_buf()));
    let catalog = ThreadCatalog::new(sessions_dir);
    let workspaces = WorkspaceStore::open(home).map_err(|error| error.to_string())?;
    let (stream, events) = tokio::sync::mpsc::channel(STREAM_CHANNEL_CAPACITY);
    Ok((AppServer::new(catalog, workspaces, models, mcp, home.to_path_buf(), stream), events))
}
