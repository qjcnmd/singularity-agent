//! 工具路径处理：基目录绑定与词法规范化。

use std::path::{Path, PathBuf};

/// 相对路径绑定到基目录，绝对路径保持原样。
///
/// 用于工具（read/write/edit/grep/glob）解析用户提供的路径参数。
/// 不做词法规范化（`..` 保留给文件系统解析），避免与符号链接交互
/// 时产生非等价词法变换。
pub(crate) fn resolve_path(base: &Path, path: &str) -> PathBuf {
    let candidate = Path::new(path);
    if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        base.join(candidate)
    }
}

/// 词法规范化绝对路径（解析 `.`/`..`），不触碰文件系统。
///
/// 用于 [`canonical_key`] 在 `fs::canonicalize` 失败时的回退；
/// 假定输入已为绝对路径。
pub(crate) fn normalize_lexically(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                result.pop();
            }
            other => result.push(other.as_os_str()),
        }
    }
    result
}