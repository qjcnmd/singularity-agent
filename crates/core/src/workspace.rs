//! 本地 Workspace 的路径身份。
//!
//! 供文件系统调用的原生路径、给用户看的字符串、以及用于判等的比较键，都在这里一次性生成。
//! Workspace 登记、Session 的 cwd 与项目指令加载共用这一份身份；它只表示路径，不代表 Agent
//! 的权限边界。

use std::path::{Path, PathBuf};

use crate::display_path;

/// 本地文件发现时要跳过的生成目录和仓库内部目录。
pub fn is_ignored_directory(name: &str) -> bool {
    matches!(name, ".git" | "node_modules" | "target")
}

/// Workspace 的规范路径身份；从持久数据恢复身份时不要求原目录仍然存在。
#[derive(Debug, Clone)]
pub struct CanonicalWorkspacePath {
    native: PathBuf,
    display: String,
    comparison_key: String,
}

impl CanonicalWorkspacePath {
    /// 从已保存的路径恢复绝对目录身份，不重新解析当前文件系统或符号链接。
    pub fn from_saved(path: impl AsRef<Path>) -> Result<Self, String> {
        let normalized = PathBuf::from(display_path(path.as_ref()));
        let path = normalized.as_path();
        if !path.is_absolute() {
            return Err("saved workspace path must be absolute".to_string());
        }
        Ok(Self::from_native(path.components().collect()))
    }

    fn from_native(native: PathBuf) -> Self {
        let display = display_path(&native);
        let comparison_key = display.to_lowercase();
        Self {
            native,
            display,
            comparison_key,
        }
    }

    /// 调用文件系统 API 时使用的规范原生路径。
    pub fn as_path(&self) -> &Path {
        &self.native
    }

    /// 跨协议、持久化和界面统一使用的稳定绝对路径。
    pub fn display(&self) -> &str {
        &self.display
    }

    /// 比较两个已经规范化的目录身份是否指向同一目录。
    pub fn matches(&self, other: &Self) -> bool {
        self.comparison_key == other.comparison_key
    }
}

impl PartialEq for CanonicalWorkspacePath {
    fn eq(&self, other: &Self) -> bool {
        self.matches(other)
    }
}

impl Eq for CanonicalWorkspacePath {}

/// 两个已保存的绝对目录字符串是否指向同一目录。工作台的 scope 校验与会话打开时的期望目录校验
/// 共用这条比较规则，因此同一对路径在两处一定得到同一结论。
pub fn saved_directory_matches(left: &str, right: &str) -> Result<bool, String> {
    let left = CanonicalWorkspacePath::from_saved(left)?;
    let right = CanonicalWorkspacePath::from_saved(right)?;
    Ok(left.matches(&right))
}

/// 把一个已存在的目录收敛成唯一的 Workspace 路径身份。
pub fn canonicalize_workspace(path: impl AsRef<Path>) -> Result<CanonicalWorkspacePath, String> {
    let requested = path.as_ref();
    let native = std::fs::canonicalize(requested).map_err(|error| {
        format!(
            "workspace directory is unavailable ({}): {error}",
            requested.display()
        )
    })?;
    let metadata = std::fs::metadata(&native).map_err(|error| {
        format!(
            "workspace directory cannot be inspected ({}): {error}",
            requested.display()
        )
    })?;
    if !metadata.is_dir() {
        return Err(format!(
            "workspace path is not a directory: {}",
            requested.display()
        ));
    }
    Ok(CanonicalWorkspacePath::from_native(native))
}

/// 从 cwd 逐级向上查找最近一个带项目根标记的目录，找不到就以 cwd 为边界。
/// 标记存在却读不出来时报错，由调用方决定上报还是降级；不能当作标记不存在
/// 继续向上找，否则同一个 cwd 会因为一次读取失败而得到不同的根目录。
pub(crate) fn project_root(cwd: &Path) -> Result<PathBuf, String> {
    for ancestor in cwd.ancestors() {
        let marker = ancestor.join(crate::PROJECT_ROOT_MARKER);
        match std::fs::symlink_metadata(&marker) {
            Ok(_) => return Ok(ancestor.to_path_buf()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!(
                    "project_root_marker_read_failed:{}:{error}",
                    marker.display()
                ));
            }
        }
    }
    Ok(cwd.to_path_buf())
}
