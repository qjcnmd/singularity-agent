//! 工具路径处理：基目录绑定。

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
