//! 桌面工作台的运行环境与数据目录锁。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use singularity_model::ModelConfigManager;
use singularity_runtime::{SESSIONS_DIR_NAME, ThreadCatalog, TurnRunner};

use crate::desktop::AppServer;
use crate::desktop::workspace_store::WorkspaceStore;

/// 在整个进程生命周期内持有一把 OS 锁；锁文件还留在磁盘上，不代表有程序正持有它。
pub fn lock_data_directory() -> Result<(PathBuf, std::fs::File), String> {
    let home = singularity_core::resolve_home()?;
    singularity_core::create_data_dir(&home)?;
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
pub fn prepare_desktop(home: &Path) -> Result<Arc<AppServer>, String> {
    let sessions_dir = home.join(SESSIONS_DIR_NAME);
    singularity_core::create_data_dir(&sessions_dir)?;
    let models = Arc::new(ModelConfigManager::open(home.to_path_buf()));
    let mcp = Arc::new(singularity_mcp::McpManager::open(home.to_path_buf()));
    let runner = Arc::new(TurnRunner::new(sessions_dir.clone(), Arc::clone(&models), Arc::clone(&mcp)));
    let catalog = ThreadCatalog::new(sessions_dir);
    let workspaces = WorkspaceStore::open(home)?;
    Ok(AppServer::new(runner, catalog, workspaces, models, mcp, home.to_path_buf()))
}
