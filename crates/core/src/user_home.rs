//! 用户数据根目录解析模块。
//!
//! 解析顺序与 DSH 一致：`SINGULARITY_HOME` 优先，未设置时使用系统用户主目录下的
//! `.singularity`。数据只落在一个根下，取值无效时如实报错，不会改写到别的位置；
//! 错误消息点名实际出问题的变量，默认来源的问题不归因给 `SINGULARITY_HOME`。

use std::{
    ffi::{OsStr, OsString},
    path::{Component, Path, PathBuf},
};

/// 显式数据根的环境变量名。
pub const SINGULARITY_HOME: &str = "SINGULARITY_HOME";

/// 默认数据根在系统用户主目录下的目录名。
pub const SINGULARITY_DIR_NAME: &str = ".singularity";

/// 数据根的来源。
///
/// 来源既决定错误归因，也决定技能范围：显式指定的数据目录自成一体，只有取自
/// 默认位置的数据根才与真实用户主目录下的 `.agents/skills` 共享技能。
#[derive(Debug, PartialEq, Eq)]
pub enum HomeOrigin {
    /// 由 `SINGULARITY_HOME` 显式指定。
    Explicit,
    /// 系统用户主目录下的默认位置；字段是那个主目录。
    Default(PathBuf),
}

/// 已解析的数据根及其来源。
#[derive(Debug, PartialEq, Eq)]
pub struct ResolvedHome {
    /// 配置、凭据、会话与用户级技能共用的数据根。
    pub path: PathBuf,
    pub origin: HomeOrigin,
}

/// 数据根的解析输入。
///
/// 取值与进程环境解耦：解析规则和错误归因可脱离全局环境单独验证，调用方也不必
/// 为了读取一次数据根而反复访问环境变量。
#[derive(Debug)]
pub struct HomeEnv {
    /// `SINGULARITY_HOME` 的取值；空串与纯空白等同于未设置。
    pub explicit: Option<OsString>,
    /// 系统用户主目录取值；`USERPROFILE` 优先，其次 `HOME`，空白取值视为未设置。
    pub os_home: Option<OsString>,
}

impl HomeEnv {
    /// 读取当前进程环境；两个来源的优先级在这里确定。
    pub fn from_process() -> Self {
        Self {
            explicit: std::env::var_os(SINGULARITY_HOME),
            os_home: ["USERPROFILE", "HOME"]
                .into_iter()
                .find_map(|name| std::env::var_os(name).filter(|value| !is_blank(value))),
        }
    }

    /// 解析数据根。
    ///
    /// 该目录与启动目录无关：从任何位置启动都解析到同一份用户级配置与会话，与
    /// 主流 harness 的用户级数据目录一致。显式取值优先；它为空、纯空白或未设置
    /// 时使用默认位置。取值不是绝对路径时如实报错，不静默改到别处。
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

/// 空串与纯空白取值按未设置处理：数据根不能因此落成相对路径。
fn is_blank(value: &OsStr) -> bool {
    value.to_string_lossy().trim().is_empty()
}

/// 校验取值是绝对路径，再做语义等价的归一化。
///
/// 归一化只清理 `.`、结尾斜杠，并把 `..` 退回上一段；无效时返回原因，由调用方
/// 按来源组装点明变量的消息。
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // 测试断言惯例
mod tests {
    use super::{HomeEnv, HomeOrigin, SINGULARITY_DIR_NAME, SINGULARITY_HOME};
    use std::ffi::OsString;
    use std::path::PathBuf;

    /// 解析输入可注入，诊断用例不修改进程环境。
    fn env(explicit: Option<&str>, os_home: Option<&str>) -> HomeEnv {
        HomeEnv {
            explicit: explicit.map(OsString::from),
            os_home: os_home.map(OsString::from),
        }
    }

    /// 显式取值按原样使用；归一化只做语义等价的清理。
    #[test]
    fn an_explicit_root_is_used_after_normalization() {
        let resolved = env(Some(r"C:\data\extra\..\final\."), Some(r"C:\Users\someone"))
            .resolve()
            .unwrap();
        assert_eq!(resolved.path, PathBuf::from(r"C:\data\final"));
        assert_eq!(resolved.origin, HomeOrigin::Explicit);
    }

    /// 显式取值未设置、为空或纯空白时落到用户主目录下的默认目录，与 DSH 的
    /// `~/.dsh` 同形。
    #[test]
    fn a_blank_explicit_root_falls_back_under_the_os_home() {
        for explicit in [None, Some(""), Some("   ")] {
            let resolved = env(explicit, Some(r"C:\Users\someone")).resolve().unwrap();
            assert_eq!(
                resolved.path,
                PathBuf::from(r"C:\Users\someone").join(SINGULARITY_DIR_NAME),
                "{explicit:?}"
            );
            assert_eq!(
                resolved.origin,
                HomeOrigin::Default(PathBuf::from(r"C:\Users\someone"))
            );
        }
    }

    /// 显式取值无效时错误点名 `SINGULARITY_HOME`，不提默认来源。
    #[test]
    fn an_invalid_explicit_root_is_attributed_to_its_own_variable() {
        let error = env(Some("relative/path"), Some(r"C:\Users\someone"))
            .resolve()
            .expect_err("a relative explicit root is invalid");
        assert!(error.contains(SINGULARITY_HOME), "{error}");
        assert!(!error.contains("USERPROFILE"), "{error}");
    }

    /// 默认来源不可用时错误点名 `USERPROFILE`/`HOME`，不把未设置的
    /// `SINGULARITY_HOME` 说成无效。
    #[test]
    fn an_unusable_default_source_is_attributed_to_the_os_home() {
        for os_home in [None, Some("relative/home"), Some("   ")] {
            let error = env(None, os_home)
                .resolve()
                .expect_err("an unusable default source must fail");
            assert!(
                error.contains("USERPROFILE") && error.contains("HOME"),
                "{os_home:?}: {error}"
            );
            assert!(!error.contains(SINGULARITY_HOME), "{os_home:?}: {error}");
        }
    }
}
