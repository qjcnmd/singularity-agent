use std::path::PathBuf;

use singularity_core::{create_owner_only_dir, user_singularity_home};

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
