//! 进程内文件变更队列：按 canonical path 记录本轮已修改的文件。
//!
//! 工具执行在单个 turn 内是串行的，但同一文件可能被后续轮次、并行工具
//! 批次或会话重开路径再次触碰；队列以 canonical path 为键登记全部修改，
//! 为「写后读一致」与未来的并行执行提供单点事实。文件的落盘一律走
//! `singularity_core::atomic_replace_bytes`（临时文件 + 原子替换），
//! 崩溃时不出现半写撕裂。

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use super::path::normalize_lexically;

/// 进程内文件变更登记。`None` 表示可读工具不参与队列（只读工具无副作用）。
#[derive(Debug, Default)]
pub struct FileMutationQueue {
    changed: Mutex<BTreeSet<PathBuf>>,
}

impl FileMutationQueue {
    /// 创建空队列。
    pub fn new() -> Self {
        Self::default()
    }

    /// 把一次修改登记到队列；路径键为 canonical 形式，无法 canonicalize
    /// （文件尚不存在或文件系统不支持）时回退绝对路径，与 pi 的
    /// `file-mutation-queue.ts` 键规则一致。
    pub fn record_change(&self, path: &Path) {
        let key = canonical_key(path);
        self.changed
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(key);
    }

    /// 该路径在队列中是否有登记（本进程内已被修改过）。
    pub fn contains(&self, path: &Path) -> bool {
        let key = canonical_key(path);
        self.changed
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .contains(&key)
    }
}

/// canonical path 键：`fs::canonicalize` 成功取其绝对规范形；失败（如
/// NotFound、权限或平台不支持）回退为规范化绝对路径。
pub(crate) fn canonical_key(path: &Path) -> PathBuf {
    match std::fs::canonicalize(path) {
        Ok(canonical) => canonical,
        Err(_) => {
            normalize_lexically(&std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_and_contains_use_equivalent_keys() {
        let dir = tempfile::tempdir().expect("temp dir");
        let file = dir.path().join("sample.txt");
        std::fs::write(&file, b"x").expect("fixture");
        let queue = FileMutationQueue::new();
        assert!(!queue.contains(&file));
        queue.record_change(&file);
        assert!(queue.contains(&file));
        // 词法等价的另一路径形式命中同一键（canonicalize 后同形）。
        let copied = canonical_key(&file);
        assert!(queue.contains(&copied), "canonical key must match");
    }

    #[test]
    fn not_found_path_falls_back_to_canonical_absolute_key() {
        let dir = tempfile::tempdir().expect("temp dir");
        let missing = dir.path().join("does-not-exist.txt");
        let queue = FileMutationQueue::new();
        queue.record_change(&missing);
        assert!(queue.contains(&missing));
        assert_eq!(
            canonical_key(&missing),
            normalize_lexically(&missing),
            "non-canonicalizable path must use lexical absolute form"
        );
    }
}
