//! glob/grep 共享的只读目录遍历辅助：跳过 `.git`/`target`/`node_modules`
//! 子树与符号链接目录（防环），权限拒绝的目录静默跳过，确定性排序。

use std::io;
use std::path::{Path, PathBuf};

/// 遍历回调的控制信号：返回 [`WalkControl::Stop`] 时遍历器立即收尾。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WalkControl {
    Continue,
    Stop,
}

/// 深度优先遍历 `root` 之下的普通文件；对每个文件以相对 `root` 的路径调用
/// `on_file`。子目录条目确定性排序后再进入，保证输出顺序稳定。回调返回
/// [`WalkControl::Stop`] 时立即停止整棵遍历。
pub(crate) fn walk_files(
    root: &Path,
    signal: &singularity_core::CancellationToken,
    on_file: &mut dyn FnMut(PathBuf) -> WalkControl,
) -> io::Result<()> {
    fn walk(
        dir: &Path,
        root: &Path,
        signal: &singularity_core::CancellationToken,
        on_file: &mut dyn FnMut(PathBuf) -> WalkControl,
    ) -> io::Result<bool> {
        if signal.is_cancelled() {
            return Ok(false);
        }
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => return Ok(true),
            Err(error) => return Err(error),
        };
        let mut paths = Vec::new();
        for entry in entries {
            if signal.is_cancelled() {
                return Ok(false);
            }
            paths.push(entry?.path());
        }
        paths.sort();
        for path in paths {
            if signal.is_cancelled() {
                return Ok(false);
            }
            let metadata = match std::fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(_) => continue,
            };
            if metadata.is_dir() {
                if path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| matches!(name, ".git" | "target" | "node_modules"))
                {
                    continue;
                }
                // 符号链接目录跳过，防止环与越出搜索根。
                if metadata.file_type().is_symlink() {
                    continue;
                }
                if !walk(&path, root, signal, on_file)? {
                    return Ok(false);
                }
            } else if metadata.is_file() {
                let relative = path.strip_prefix(root).unwrap_or(&path).to_path_buf();
                if on_file(relative) == WalkControl::Stop {
                    return Ok(false);
                }
            }
        }
        Ok(true)
    }
    let _ = walk(root, root, signal, on_file)?;
    Ok(())
}

/// 把相对路径渲染成 `/` 分隔的字符串（跨平台输出稳定）。
pub(crate) fn display_path(relative: &Path) -> String {
    relative
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

/// 把相对 `root` 的路径投影为相对 `cwd` 的路径字符串；`root` 不在 `cwd`
/// 之下时回退为绝对路径。
pub(crate) fn to_cwd_relative(cwd: &Path, root: &Path, relative: &Path) -> String {
    if root == cwd {
        return display_path(relative);
    }
    match root.strip_prefix(cwd) {
        Ok(prefix) => display_path(&prefix.join(relative)),
        Err(_) => root.join(relative).to_string_lossy().into_owned(),
    }
}
