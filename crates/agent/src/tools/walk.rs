//! glob/grep 共享的只读目录遍历辅助：跳过 .git/target/node_modules
//! 子树与符号链接目录（防环），报告跳过的不可读路径，确定性排序。

use std::io;
use std::path::{Path, PathBuf};

use singularity_core::display_path;

pub(crate) fn search_root(cwd: &Path, path: &str) -> Result<PathBuf, String> {
    let root = cwd.join(path);
    if !root.is_dir() {
        return Err(format!("path is not a directory: {path}"));
    }
    Ok(root)
}

/// 遍历回调的控制信号：返回 WalkControl::Stop 时遍历器立即收尾。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WalkControl {
    Continue,
    Stop,
}

/// A bounded summary keeps partial search results useful without hiding I/O failures.
#[derive(Default)]
pub(crate) struct SearchWarnings {
    count: usize,
    first: Option<String>,
}

impl SearchWarnings {
    pub(crate) fn record(&mut self, path: &Path, error: &io::Error) {
        self.count += 1;
        if self.first.is_none() {
            self.first = Some(format!("{}: {error}", path.display()));
        }
    }

    pub(crate) fn merge(&mut self, other: Self) {
        self.count += other.count;
        if self.first.is_none() {
            self.first = other.first;
        }
    }

    pub(crate) fn append_to(&self, output: &mut String) {
        if let Some(first) = &self.first {
            output.push_str(&format!(
                "\n[search incomplete: {} unreadable path(s); first error: {first}]",
                self.count
            ));
        }
    }
}

/// 深度优先遍历 root 之下的普通文件；对每个文件以相对 root 的路径调用
/// on_file。子目录条目确定性排序后再进入，保证输出顺序稳定。回调返回
/// WalkControl::Stop 时立即停止整棵遍历。
pub(crate) fn walk_files(
    root: &Path,
    signal: &singularity_core::CancellationToken,
    on_file: &mut dyn FnMut(PathBuf) -> WalkControl,
) -> io::Result<SearchWarnings> {
    fn walk(
        dir: &Path,
        root: &Path,
        signal: &singularity_core::CancellationToken,
        on_file: &mut dyn FnMut(PathBuf) -> WalkControl,
        warnings: &mut SearchWarnings,
    ) -> io::Result<bool> {
        if signal.is_cancelled() {
            return Ok(false);
        }
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(error) if dir != root && error.kind() == io::ErrorKind::PermissionDenied => {
                warnings.record(dir, &error);
                return Ok(true);
            }
            Err(error) => return Err(error),
        };
        let mut paths = Vec::new();
        for entry in entries {
            if signal.is_cancelled() {
                return Ok(false);
            }
            match entry {
                Ok(entry) => paths.push(entry.path()),
                Err(error) => warnings.record(dir, &error),
            }
        }
        paths.sort();
        for path in paths {
            if signal.is_cancelled() {
                return Ok(false);
            }
            let metadata = match std::fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) => {
                    warnings.record(&path, &error);
                    continue;
                }
            };
            if metadata.is_dir() {
                if path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(singularity_core::workspace::is_ignored_directory)
                {
                    continue;
                }
                if !walk(&path, root, signal, on_file, warnings)? {
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
    let mut warnings = SearchWarnings::default();
    walk(root, root, signal, on_file, &mut warnings)?;
    Ok(warnings)
}

/// 把相对 root 的路径投影为相对 cwd 的路径字符串；root 不在 cwd
/// 之下时回退为绝对路径。
pub(crate) fn to_cwd_relative(cwd: &Path, root: &Path, relative: &Path) -> String {
    if root == cwd {
        return display_path(relative);
    }
    match root.strip_prefix(cwd) {
        Ok(prefix) => display_path(&prefix.join(relative)),
        Err(_) => display_path(&root.join(relative)),
    }
}
