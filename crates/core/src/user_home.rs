//! 用户数据根目录的解析。
//!
//! 解析顺序：优先用 `SINGULARITY_HOME`，没设置就用系统用户主目录
//! 下的 `.singularity`。数据只落在一个根目录里；取值无效时如实报错，不会偷偷
//! 改写到别处。错误消息点名真正出问题的那个变量——默认来源出问题时不把责任
//! 算到 `SINGULARITY_HOME` 头上。

use std::{
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
};

const SINGULARITY_HOME: &str = "SINGULARITY_HOME";

pub const SINGULARITY_DIR_NAME: &str = ".singularity";

/// 数据根是从哪里来的。来源既决定错误归因，也决定技能范围：显式指定的数据目录自成一体；
/// 只有取自默认位置的数据根，才会和真实用户主目录下的 `.agents/skills` 共享技能。
#[derive(Debug, PartialEq, Eq)]
pub enum HomeOrigin {
    Explicit,
    /// 系统用户主目录下的默认位置；字段里装的就是那个主目录。
    Default(PathBuf),
}

/// 已解析的数据根及其来源。
#[derive(Debug, PartialEq, Eq)]
pub struct ResolvedHome {
    /// 配置、凭据、会话与用户级技能共用的数据根目录。
    pub path: PathBuf,
    pub origin: HomeOrigin,
}

/// 系统用户主目录，由标准库按 Windows 用户配置解析。
pub fn os_home() -> Option<OsString> {
    std::env::home_dir().map(PathBuf::into_os_string)
}

/// 从当前进程环境解析应用数据根及来源；显式目录优先，否则使用系统主目录。
/// 无效路径直接报错，不改变保存位置。
pub fn resolve_home() -> Result<ResolvedHome, String> {
    if let Some(value) = std::env::var_os(SINGULARITY_HOME).filter(|value| !is_blank(value)) {
        let base = Path::new(&value);
        let path = validated_root(base)
            .map_err(|reason| format!("{SINGULARITY_HOME} {reason}: {}", base.display()))?;
        return Ok(ResolvedHome {
            path,
            origin: HomeOrigin::Explicit,
        });
    }
    let value = os_home().ok_or_else(|| {
        "cannot resolve the default data directory: system user profile is unavailable".to_string()
    })?;
    let base = Path::new(&value);
    let base = validated_root(base).map_err(|reason| {
        format!(
            "cannot resolve the default data directory: system user profile {reason}: {}",
            base.display()
        )
    })?;
    Ok(ResolvedHome {
        path: base.join(SINGULARITY_DIR_NAME),
        origin: HomeOrigin::Default(base),
    })
}

/// 空串和纯空白都按没设置处理：否则数据根会变成一个相对路径。
fn is_blank(value: &OsStr) -> bool {
    value.to_string_lossy().trim().is_empty()
}

/// 数据根要求绝对路径；路径组件交给系统解析，不自行折叠父目录。
fn validated_root(base: &Path) -> Result<PathBuf, String> {
    if !base.is_absolute() {
        return Err("must be an absolute path".into());
    }
    std::path::absolute(base).map_err(|error| format!("cannot resolve path: {error}"))
}
