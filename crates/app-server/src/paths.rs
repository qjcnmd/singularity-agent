use std::path::PathBuf;

use singularity_core::{create_owner_only_dir, user_singularity_home};

/// 把 runtime 的 JSONL 只读摘要投影为协议 Thread。
pub fn thread_from_summary(
    record: &singularity_runtime::ThreadSummary,
) -> singularity_protocol::Thread {
    singularity_protocol::Thread {
        thread_id: record.thread_id.clone(),
        model: record.model.clone(),
        cwd: Some(record.cwd.clone()),
        last_turn_status: record.status.map(|status| match status {
            singularity_runtime::ThreadStatus::Active => singularity_protocol::ThreadStatus::Active,
            singularity_runtime::ThreadStatus::Completed => {
                singularity_protocol::ThreadStatus::Completed
            }
            singularity_runtime::ThreadStatus::Failed => singularity_protocol::ThreadStatus::Failed,
            singularity_runtime::ThreadStatus::Interrupted => {
                singularity_protocol::ThreadStatus::Interrupted
            }
        }),
    }
}

/// `~/.singularity` 下的固定路径集合（会话目录名复用 runtime 单一事实源）。
#[derive(Debug, Clone)]
pub struct AppPaths {
    pub home_dir: PathBuf,
    pub sessions_dir: PathBuf,
}

impl AppPaths {
    pub fn resolve() -> Result<Self, String> {
        let home = user_singularity_home()
            .ok_or_else(|| "cannot resolve SINGULARITY_HOME for sessions".to_string())?;
        Ok(Self {
            sessions_dir: home.join(singularity_runtime::store::SESSIONS_DIR_NAME),
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
