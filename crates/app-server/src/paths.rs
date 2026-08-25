use std::path::PathBuf;

use singularity_core::{create_owner_only_dir, user_singularity_home};

use super::session_index::SessionRecord;

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

/// `~/.singularity` 下的固定路径集合。
#[derive(Debug, Clone)]
pub struct AppPaths {
    pub home_dir: PathBuf,
    pub sessions_dir: PathBuf,
}

impl AppPaths {
    pub fn resolve() -> Result<Self, String> {
        let home = user_singularity_home()
            .ok_or_else(|| "cannot resolve SINGULARITY_HOME for session index".to_string())?;
        Ok(Self {
            sessions_dir: home.join(SESSIONS_DIR_NAME),
            home_dir: home,
        })
    }

    /// 创建 home 与会话目录（在 Unix 系统上收紧为 0700 权限）。
    pub fn prepare(&self) -> Result<(), String> {
        create_owner_only_dir(&self.home_dir)?;
        create_owner_only_dir(&self.sessions_dir)?;
        Ok(())
    }
}
