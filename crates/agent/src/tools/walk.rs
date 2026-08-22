//! glob/grep 共享的只读目录遍历辅助：跳过 `skipped_dir` 子树与符号链接目录
//! （防环），权限拒绝的目录静默跳过，确定性排序。

use std::io;
use std::path::{Path, PathBuf};

/// 遍历中跳过（含子树）的目录名。
pub(crate) fn skipped_dir(name: &str) -> bool {
    matches!(name, ".git" | "target" | "node_modules")
}

/// 深度优先遍历 `root` 之下的普通文件；对每个文件以相对 `root` 的路径调用
/// `on_file`。子目录条目确定性排序后再进入，保证输出顺序稳定。
pub(crate) fn walk_files(root: &Path, on_file: &mut dyn FnMut(PathBuf)) -> io::Result<()> {
    fn walk(dir: &Path, root: &Path, on_file: &mut dyn FnMut(PathBuf)) -> io::Result<()> {
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => return Ok(()),
            Err(error) => return Err(error),
        };
        let mut paths = Vec::new();
        for entry in entries {
            paths.push(entry?.path());
        }
        paths.sort();
        for path in paths {
            let metadata = match std::fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(_) => continue,
            };
            if metadata.is_dir() {
                if path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(skipped_dir)
                {
                    continue;
                }
                // 符号链接目录跳过，防止环与越出搜索根。
                if metadata.file_type().is_symlink() {
                    continue;
                }
                walk(&path, root, on_file)?;
            } else if metadata.is_file() {
                let relative = path.strip_prefix(root).unwrap_or(&path).to_path_buf();
                on_file(relative);
            }
        }
        Ok(())
    }
    walk(root, root, on_file)
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
        Ok(prefix) => prefix
            .components()
            .map(|component| component.as_os_str().to_string_lossy())
            .chain(
                relative
                    .components()
                    .map(|component| component.as_os_str().to_string_lossy()),
            )
            .collect::<Vec<_>>()
            .join("/"),
        Err(_) => root.join(relative).to_string_lossy().into_owned(),
    }
}
