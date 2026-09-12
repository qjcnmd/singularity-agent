//! 用户主目录与数据根目录解析模块。

use std::path::PathBuf;

/// 未显式设置 SINGULARITY_HOME 时使用的用户级数据子目录名。
pub const SINGULARITY_DIR_NAME: &str = ".singularity";

/// 用户数据目录的原始环境变量链：SINGULARITY_HOME → USERPROFILE → HOME。
/// 返回（原始基路径，是否来自显式 SINGULARITY_HOME）；三者都未设置时为
/// None。路径的有效性校验由调用方按自身语义完成。
pub fn user_home_base_from_env() -> Option<(PathBuf, bool)> {
    match std::env::var_os("SINGULARITY_HOME") {
        Some(home) => Some((PathBuf::from(home), true)),
        None => std::env::var_os("USERPROFILE")
            .or_else(|| std::env::var_os("HOME"))
            .map(|home| (PathBuf::from(home), false)),
    }
}

/// 解析用户级 singularity 数据目录：显式 SINGULARITY_HOME，否则
/// $HOME/.singularity。该目录与启动目录无关：从任何位置启动都解析到同一份
/// 用户级配置与会话，与主流 harness 的用户级数据目录一致。
pub fn user_singularity_home() -> Option<PathBuf> {
    user_singularity_home_result().ok().flatten()
}

/// Resolve the data root, reporting invalid explicit paths instead of falling back.
pub fn user_singularity_home_result() -> Result<Option<PathBuf>, String> {
    let Some((home, explicit)) = user_home_base_from_env() else {
        return Ok(None);
    };
    if home.as_os_str().is_empty() || !home.is_absolute() {
        return Err("SINGULARITY_HOME must be a non-empty absolute path".into());
    }
    let mut normalized = PathBuf::new();
    for component in home.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            _ => normalized.push(component.as_os_str()),
        }
    }
    Ok(Some(if explicit {
        normalized
    } else {
        normalized.join(SINGULARITY_DIR_NAME)
    }))
}
