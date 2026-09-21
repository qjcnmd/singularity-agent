//! 用户数据根目录的解析。
//!
//! 解析顺序：优先用 `SINGULARITY_HOME`，没设置就用系统用户主目录
//! 下的 `.singularity`。数据只落在一个根目录里；取值无效时如实报错，不会偷偷
//! 改写到别处。错误消息点名真正出问题的那个变量——默认来源出问题时不把责任
//! 算到 `SINGULARITY_HOME` 头上。

use std::{
    ffi::{OsStr, OsString},
    path::{Component, Path, PathBuf},
};

pub const SINGULARITY_HOME: &str = "SINGULARITY_HOME";

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

/// 解析数据根所需的输入。把取值与进程环境分开：解析规则和错误归因可以脱离全局环境单独验证，
/// 调用方也不必为了读一次数据根而反复访问环境变量。
#[derive(Debug)]
pub struct HomeEnv {
    /// `SINGULARITY_HOME` 的取值；空串和纯空白都等同于没设置。
    pub explicit: Option<OsString>,
    /// 系统用户主目录的取值；优先 `USERPROFILE`，其次 `HOME`，空白取值视为没设置。
    pub os_home: Option<OsString>,
}

impl HomeEnv {
    /// 读取当前进程的环境变量；两个来源的优先级在这里定下来。
    pub fn from_process() -> Self {
        Self {
            explicit: std::env::var_os(SINGULARITY_HOME),
            os_home: ["USERPROFILE", "HOME"]
                .into_iter()
                .find_map(|name| std::env::var_os(name).filter(|value| !is_blank(value))),
        }
    }

    /// 解析数据根。这个目录与启动目录无关：从哪里启动都解析到同一份用户级配置和会话。
    /// 显式取值优先，它为空、纯空白或没设置时用默认位置；取值不是绝对路径时如实报错，
    /// 不会静默改到别处。
    pub fn resolve(&self) -> Result<ResolvedHome, String> {
        if let Some(value) = self.explicit.as_deref().filter(|value| !is_blank(value)) {
            let base = Path::new(value);
            let path = validated_root(base)
                .map_err(|reason| format!("{SINGULARITY_HOME} {reason}: {}", base.display()))?;
            return Ok(ResolvedHome {
                path,
                origin: HomeOrigin::Explicit,
            });
        }
        let Some(value) = self.os_home.as_deref().filter(|value| !is_blank(value)) else {
            return Err(
                "cannot resolve the default data directory: USERPROFILE and HOME are both unset"
                    .to_string(),
            );
        };
        let base = Path::new(value);
        let base = validated_root(base).map_err(|reason| {
            format!(
                "cannot resolve the default data directory: USERPROFILE/HOME {reason}: {}",
                base.display()
            )
        })?;
        Ok(ResolvedHome {
            path: base.join(SINGULARITY_DIR_NAME),
            origin: HomeOrigin::Default(base),
        })
    }
}

/// 空串和纯空白都按没设置处理：否则数据根会变成一个相对路径。
fn is_blank(value: &OsStr) -> bool {
    value.to_string_lossy().trim().is_empty()
}

/// 先校验取值是绝对路径，再做词法归一化。归一化只处理路径组件：去掉 `.` 和结尾的斜杠，把 `..`
/// 退回上一段；它不解析符号链接和 junction，所以取值里某一段是链接时，结果可能和系统按链接
/// 目标解析出来的不一样。取值无效时返回原因，由调用方按来源拼出点名变量的消息。
fn validated_root(base: &Path) -> Result<PathBuf, &'static str> {
    if !base.is_absolute() {
        return Err("must be an absolute path");
    }
    let mut normalized = PathBuf::new();
    for component in base.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            _ => normalized.push(component.as_os_str()),
        }
    }
    Ok(normalized)
}
