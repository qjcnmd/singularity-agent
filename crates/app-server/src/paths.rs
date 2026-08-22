use std::path::{Path, PathBuf};

use singularity_core::user_singularity_home;

use super::session_index::SessionRecord;
use crate::owner_only::ensure_owner_only_dir;

pub(super) fn canonical_thread_cwd(cwd: Option<&str>) -> Result<String, String> {
    let path = match cwd {
        Some(cwd) if !cwd.trim().is_empty() => Path::new(cwd).to_path_buf(),
        Some(_) => return Err("thread cwd must not be empty".to_string()),
        None => std::env::current_dir()
            .map_err(|error| format!("failed to read current directory: {error}"))?,
    };
    let canonical =
        std::fs::canonicalize(&path).map_err(|_| "failed to bind thread cwd".to_string())?;
    canonical
        .to_str()
        .map(str::to_string)
        .ok_or_else(|| "thread cwd is not valid UTF-8".to_string())
}

pub(super) fn workspace_path(thread: &singularity_protocol::Thread) -> Result<PathBuf, String> {
    let cwd = thread
        .cwd
        .as_deref()
        .filter(|cwd| !cwd.trim().is_empty())
        .ok_or_else(|| "thread does not have an absolute workspace".to_string())?;
    let path = Path::new(cwd);
    if !path.is_absolute() {
        return Err("thread does not have an absolute workspace".to_string());
    }
    Ok(path.to_path_buf())
}

/// 持久化状态的原始投影：仅供内部（打开会话、provider 配置）使用；
/// wire 可见的 thread 摘要必须经过 `AppServer::project_thread`。
pub fn thread_from_record(record: &SessionRecord) -> singularity_protocol::Thread {
    singularity_protocol::Thread {
        thread_id: record.session_id.clone(),
        model: record.model.clone(),
        cwd: Some(record.cwd.clone()),
        last_turn_status: record.status,
    }
}

pub const SESSIONS_DIR_NAME: &str = "sessions";
pub const BACKUPS_DIR_NAME: &str = "backups";

/// `~/.singularity` 下由本次架构固定下来的路径集合。
#[derive(Debug, Clone)]
pub struct AppPaths {
    pub home_dir: PathBuf,
    pub sessions_dir: PathBuf,
    pub backups_dir: PathBuf,
}

impl AppPaths {
    pub fn resolve() -> Result<Self, String> {
        let home = user_singularity_home()
            .ok_or_else(|| "cannot resolve SINGULARITY_HOME for session index".to_string())?;
        Ok(Self {
            sessions_dir: home.join(SESSIONS_DIR_NAME),
            backups_dir: home.join(BACKUPS_DIR_NAME),
            home_dir: home,
        })
    }

    /// 创建会话目录与备份目录（在 Unix 系统上收紧为 0700 权限）。
    pub fn prepare(&self) -> Result<(), String> {
        create_owner_only_dir(&self.home_dir)?;
        create_owner_only_dir(&self.sessions_dir)?;
        create_owner_only_dir(&self.backups_dir)?;
        Ok(())
    }
}

pub fn create_owner_only_dir(path: &Path) -> Result<(), String> {
    std::fs::create_dir_all(path)
        .map_err(|error| format!("failed to create {}: {error}", path.display()))?;
    ensure_owner_only_dir(path).map_err(|error| error.to_string())
}
