//! 用户级 singularity 数据目录解析。
//!
//! 项目信任链（trust.json / TrustDecisions / resolve_project_trusted）已随
//! H2 裁决删除：AGENTS.md 与 Pi 一致无条件逐层加载，不再做 trust 门控。

use std::path::PathBuf;

/// 未显式设置 `SINGULARITY_HOME` 时使用的用户级数据子目录名。
const USER_SINGULARITY_DIR_NAME: &str = ".singularity";

/// 解析用户级 singularity 数据目录：显式 `SINGULARITY_HOME`，否则 `$HOME/.singularity`
/// （与 model crate 的用户配置目录语义一致）。
pub fn user_singularity_home() -> Option<PathBuf> {
    let explicit_home = std::env::var_os("SINGULARITY_HOME");
    let home = explicit_home
        .clone()
        .or_else(|| std::env::var_os("USERPROFILE"))
        .or_else(|| std::env::var_os("HOME"))?;
    let home = PathBuf::from(home);
    if home.as_os_str().is_empty() || !home.is_absolute() {
        return None;
    }
    if explicit_home.is_some() {
        Some(home)
    } else {
        Some(home.join(USER_SINGULARITY_DIR_NAME))
    }
}
