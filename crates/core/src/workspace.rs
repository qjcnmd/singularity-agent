//! 本地 Workspace 的路径身份：供文件系统调用的原生路径、给用户看的字符串和判等用的
//! 比较键，都在这里一次性生成。Workspace 登记、Session 的 cwd 与项目指令加载共用这份
//! 身份；它只表示路径，不代表 Agent 的权限边界。

use std::path::{Path, PathBuf};

use crate::display_path;

/// 规范化的工作区路径结构，封装原生路径、展示路径与不区分大小写的比较键。
#[derive(Debug, Clone)]
pub struct CanonicalWorkspacePath {
    native: PathBuf,
    display: String,
    comparison_key: String,
}

impl CanonicalWorkspacePath {
    /// 从已保存的路径字符串恢复规范路径对象。
    pub fn from_saved(path: impl AsRef<Path>) -> Result<Self, String> {
        let path = path.as_ref();
        if !path.is_absolute() {
            return Err("saved workspace path must be absolute".to_string());
        }
        Ok(Self::from_native(path.components().collect()))
    }

    fn from_native(native: PathBuf) -> Self {
        let display = display_path(&native);
        let comparison_key = display.to_lowercase();
        Self { native, display, comparison_key }
    }

    /// 获取供底层文件系统 API 使用的原生路径。
    pub fn as_path(&self) -> &Path {
        &self.native
    }

    /// 获取供协议和界面展示的统一路径字符串。
    pub fn display(&self) -> &str {
        &self.display
    }
}

impl PartialEq for CanonicalWorkspacePath {
    fn eq(&self, other: &Self) -> bool {
        self.comparison_key == other.comparison_key
    }
}

impl Eq for CanonicalWorkspacePath {}

/// 比较两个已保存的路径是否指向相同的工作区目录。
pub fn saved_directory_matches(left: &str, right: &str) -> Result<bool, String> {
    let left = CanonicalWorkspacePath::from_saved(left)?;
    let right = CanonicalWorkspacePath::from_saved(right)?;
    Ok(left == right)
}

/// 检查并规范化已存在的本地目录为工作区路径。
pub fn canonicalize_workspace(path: impl AsRef<Path>) -> Result<CanonicalWorkspacePath, String> {
    let requested = path.as_ref();
    let native = std::fs::canonicalize(requested)
        .map_err(|error| format!("workspace directory is unavailable ({}): {error}", requested.display()))?;
    let metadata = std::fs::metadata(&native).map_err(|error| {
        format!("workspace directory cannot be inspected ({}): {error}", requested.display())
    })?;
    if !metadata.is_dir() {
        return Err(format!("workspace path is not a directory: {}", requested.display()));
    }
    Ok(CanonicalWorkspacePath::from_native(native))
}

/// 沿当前目录逐级向上查找项目根目录（带有 `.git` 标记），未找到时返回当前工作目录。
pub(crate) fn project_root(cwd: &Path) -> Result<PathBuf, String> {
    for ancestor in cwd.ancestors() {
        let marker = ancestor.join(crate::PROJECT_ROOT_MARKER);
        match std::fs::symlink_metadata(&marker) {
            Ok(_) => return Ok(ancestor.to_path_buf()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!("project_root_marker_read_failed:{}:{error}", marker.display()));
            }
        }
    }
    Ok(cwd.to_path_buf())
}
