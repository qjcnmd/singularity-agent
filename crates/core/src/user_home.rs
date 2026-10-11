//! 用户数据根目录的解析：优先用 `SINGULARITY_HOME`，没设置就用系统用户主目录下的
//! `.singularity`。取值无效时如实报错，不改写到别处；错误消息点名真正出问题的变量，
//! 默认来源出问题时不归到 `SINGULARITY_HOME` 头上。

use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
};

const SINGULARITY_HOME: &str = "SINGULARITY_HOME";

pub const SINGULARITY_DIR_NAME: &str = ".singularity";

/// 数据根目录来源类型。
#[derive(Debug, PartialEq, Eq)]
pub enum HomeOrigin {
    /// 通过 `SINGULARITY_HOME` 环境变量显式指定
    Explicit,
    /// 系统用户主目录下的默认位置（包含真实用户主目录路径）
    Default(PathBuf),
}

/// 解析完成的数据根路径与来源信息。
#[derive(Debug, PartialEq, Eq)]
pub struct ResolvedHome {
    /// 数据根目录绝对路径（用于存储配置、凭据与会话）
    pub path: PathBuf,
    pub origin: HomeOrigin,
}

/// 解析应用数据根目录（优先读取 `SINGULARITY_HOME` 环境变量，默认使用用户目录下的 `.singularity`）。
pub fn resolve_home() -> Result<ResolvedHome, String> {
    if let Some(value) = std::env::var_os(SINGULARITY_HOME).filter(|value| !is_blank(value)) {
        let base = Path::new(&value);
        let path = validated_root(base)
            .map_err(|reason| format!("{SINGULARITY_HOME} {reason}: {}", base.display()))?;
        return Ok(ResolvedHome { path, origin: HomeOrigin::Explicit });
    }
    let base = std::env::home_dir().ok_or_else(|| {
        "cannot resolve the default data directory: system user profile is unavailable".to_string()
    })?;
    let base = validated_root(&base).map_err(|reason| {
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
