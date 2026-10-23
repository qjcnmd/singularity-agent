//! 项目目录的路径处理：文件系统调用用的原生路径、显示用的字符串和比较用的键在这里一起生成。
//! 项目登记、会话的工作目录和项目指令加载都用它；它只表示路径，不限制 Agent 的访问范围。

use std::path::{Path, PathBuf};

use crate::display_path;

/// 已规范化的项目目录路径；相等比较忽略大小写和斜杠方向。
#[derive(Debug, Clone)]
pub struct CanonicalWorkspacePath {
    native: PathBuf,
    display: String,
    comparison_key: String,
}

impl CanonicalWorkspacePath {
    /// 从已保存的绝对路径构造，只整理路径组件，不访问文件系统。
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

    /// 文件系统调用使用的原生路径。
    pub fn as_path(&self) -> &Path {
        &self.native
    }

    /// 忽略大小写的目录键，用于项目分组。
    pub fn key(&self) -> &str {
        &self.comparison_key
    }

    /// 界面显示和保存使用的正斜杠路径（去掉 Windows `\\?\` 前缀）。
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

/// 按忽略大小写的规范形式比较两个已保存的绝对路径。
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
