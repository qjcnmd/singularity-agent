//! 本地 Workspace 路径身份。
//!
//! 文件系统可访问路径、用户可见字符串和等价比较键在这里一次生成。Workspace
//! 登记、Session cwd 与项目指令加载共用该身份；它不表达 Agent 权限边界。

use std::fmt::{Display, Formatter};
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub struct WorkspacePathError {
    message: String,
}

impl WorkspacePathError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl Display for WorkspacePathError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for WorkspacePathError {}

/// 已存在目录的规范身份。
#[derive(Debug, Clone)]
pub struct CanonicalWorkspacePath {
    native: PathBuf,
    display: String,
    comparison_key: String,
}

impl CanonicalWorkspacePath {
    /// 文件系统调用使用的规范原生路径。
    pub fn as_path(&self) -> &Path {
        &self.native
    }

    /// 跨协议、持久化与用户界面使用的稳定绝对路径。
    pub fn display(&self) -> &str {
        &self.display
    }

    /// 比较两个已经规范化的目录身份。
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

/// 把一个已存在目录收敛成唯一 Workspace 路径身份。
pub fn canonicalize_workspace(
    path: impl AsRef<Path>,
) -> Result<CanonicalWorkspacePath, WorkspacePathError> {
    let requested = path.as_ref();
    let native = std::fs::canonicalize(requested).map_err(|error| {
        WorkspacePathError::new(format!(
            "workspace directory is unavailable ({}): {error}",
            requested.display()
        ))
    })?;
    let metadata = std::fs::metadata(&native).map_err(|error| {
        WorkspacePathError::new(format!(
            "workspace directory cannot be inspected ({}): {error}",
            requested.display()
        ))
    })?;
    if !metadata.is_dir() {
        return Err(WorkspacePathError::new(format!(
            "workspace path is not a directory: {}",
            requested.display()
        )));
    }
    let display = display_path(&native);
    #[cfg(windows)]
    let comparison_key = display.to_lowercase();
    #[cfg(not(windows))]
    let comparison_key = display.clone();
    Ok(CanonicalWorkspacePath {
        native,
        display,
        comparison_key,
    })
}

fn display_path(path: &Path) -> String {
    let text = path.to_string_lossy();
    let native = if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = text.strip_prefix(r"\\?\") {
        rest.to_owned()
    } else {
        text.into_owned()
    };
    native.replace('\\', "/")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::*;

    #[test]
    fn equivalent_directory_spellings_share_one_identity() {
        let directory = tempfile::tempdir().expect("temp dir");
        let with_dot = directory.path().join(".");
        let first = canonicalize_workspace(directory.path()).expect("canonical path");
        let second = canonicalize_workspace(with_dot).expect("canonical dotted path");
        assert!(first.matches(&second));
        assert!(!first.display().contains('\\'));
    }

    #[test]
    fn files_are_not_workspace_directories() {
        let directory = tempfile::tempdir().expect("temp dir");
        let file = directory.path().join("file.txt");
        std::fs::write(&file, "x").expect("write fixture");
        assert!(canonicalize_workspace(&file).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn windows_case_and_verbatim_spelling_match() {
        let directory = tempfile::tempdir().expect("temp dir");
        let canonical = canonicalize_workspace(directory.path()).expect("canonical path");
        let uppercase = PathBuf::from(canonical.display().to_uppercase());
        let alternate = canonicalize_workspace(uppercase).expect("uppercase path");
        let verbatim = PathBuf::from(format!(r"\\?\{}", canonical.display().replace('/', r"\")));
        let verbatim = canonicalize_workspace(verbatim).expect("verbatim path");
        assert!(canonical.matches(&alternate));
        assert!(canonical.matches(&verbatim));
        assert!(!canonical.display().starts_with("//?/"));

        let root = canonicalize_workspace(r"C:\").expect("Windows root");
        assert_eq!(root.display(), "C:/");
    }
}
