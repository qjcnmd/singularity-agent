//! glob 与 grep 共用的只读目录遍历辅助：跳过 .git/target/node_modules 子树和
//! 符号链接目录（防止绕成环），报告跳过的不可读路径，并保证顺序确定。

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

/// 遍历回调给遍历器的控制信号：返回 WalkControl::Stop 时立刻收尾。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WalkControl {
    Continue,
    Stop,
}

/// 有界汇总：让部分搜索结果仍然可用，同时不掩盖 I/O 失败。
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

/// 深度优先遍历 root 下的普通文件，对每个文件用相对 root 的路径调用 on_file。进入
/// 子目录前先对条目做确定性排序以保证输出顺序稳定；回调返回 Stop 时立刻停止整棵遍历。
pub(crate) fn walk_files(
    root: &Path,
    signal: &tokio_util::sync::CancellationToken,
    on_file: &mut dyn FnMut(PathBuf) -> WalkControl,
) -> io::Result<SearchWarnings> {
    /// 递归遍历一层目录。返回 [`WalkControl::Stop`] 表示整棵遍历必须停止（取消令牌已
    /// 置位、回调要求停止或子树已停止）；I/O 失败仍按 `Err` 上报，不混进停止信号。
    fn walk(
        dir: &Path,
        root: &Path,
        signal: &tokio_util::sync::CancellationToken,
        on_file: &mut dyn FnMut(PathBuf) -> WalkControl,
        warnings: &mut SearchWarnings,
    ) -> io::Result<WalkControl> {
        if signal.is_cancelled() {
            return Ok(WalkControl::Stop);
        }
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            // 子目录不可读只记警告；根目录读不了仍按失败上报。
            Err(error) if dir != root && error.kind() == io::ErrorKind::PermissionDenied => {
                warnings.record(dir, &error);
                return Ok(WalkControl::Continue);
            }
            Err(error) => return Err(error),
        };
        let mut children = Vec::new();
        for entry in entries {
            if signal.is_cancelled() {
                return Ok(WalkControl::Stop);
            }
            match entry {
                Ok(entry) => children.push(entry),
                Err(error) => warnings.record(dir, &error),
            }
        }
        children.sort_by_cached_key(std::fs::DirEntry::file_name);
        for entry in children {
            if signal.is_cancelled() {
                return Ok(WalkControl::Stop);
            }
            let path = entry.path();
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(error) => {
                    warnings.record(&path, &error);
                    continue;
                }
            };
            if file_type.is_dir() {
                if path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(singularity_core::workspace::is_ignored_directory)
                {
                    continue;
                }
                if walk(&path, root, signal, on_file, warnings)? == WalkControl::Stop {
                    return Ok(WalkControl::Stop);
                }
            } else if file_type.is_file() {
                let relative = path.strip_prefix(root).unwrap_or(&path).to_path_buf();
                if on_file(relative) == WalkControl::Stop {
                    return Ok(WalkControl::Stop);
                }
            }
        }
        Ok(WalkControl::Continue)
    }
    let mut warnings = SearchWarnings::default();
    walk(root, root, signal, on_file, &mut warnings)?;
    Ok(warnings)
}

/// 把相对 root 的路径换算成相对 cwd 的路径字符串；root 不在 cwd 之下时，
/// 退回使用绝对路径。
pub(crate) fn to_cwd_relative(cwd: &Path, root: &Path, relative: &Path) -> String {
    match root.strip_prefix(cwd) {
        Ok(prefix) => display_path(&prefix.join(relative)),
        Err(_) => display_path(&root.join(relative)),
    }
}
