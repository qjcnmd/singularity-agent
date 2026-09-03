//! 会话级"已观察文件"表：防误覆盖的正确性防护。
//!
//! 它只回答一个问题：模型要整份盖掉或就地改一个文件时，它**见过这个文件吗？
//! 见的还是现在这一版吗？**三态事实——没条目 = 未见过；条目 `Absent` = 读到过
//! "不存在"（确认缺失）；条目 `Present` = 见过某个版本。据此：
//!
//! - `read` 成功记下当前版本；读到不存在的文件记下确认缺失，之后 `write`
//!   才能安全重建而不撞掉并发创建者。
//! - `edit` 要求先见过且版本未变；`write` 覆盖已存在的文件同样要求，
//!   新建则不需要。
//! - 变更成功后补记新版本，刚改过的文件不必重读即可再改。
//!
//! 表随会话对象生灭、不落盘：重启后一切重新观察。键取 [`batch::path_key`]
//! 的词法绝对形，与批次内文件锁同一口径。

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use std::time::SystemTime;

use super::batch::lock_unpoisoned;

/// 文件版本事实：字节数 + 最后修改时间。任一变化即视为"自上次看到现在变了"。
/// 取元数据而非内容哈希：探测一次 `stat` 即可，不必为覆盖写把整份文件读进内存。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FileVersion {
    pub(crate) byte_len: u64,
    pub(crate) modified: SystemTime,
}

/// 一个目标在本会话里的观察状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Observed {
    /// 本会话从未读过、写过它。
    Unseen,
    /// 本会话读到过"不存在"，即确认缺失。
    Absent,
    /// 本会话见过这一版。
    Present(FileVersion),
}

/// 由一份文件元数据取版本事实。路径探测与已打开句柄两条来源共用这一处映射，
/// "同一个版本"在全仓只有一个算法。
pub(crate) fn version_of(metadata: &std::fs::Metadata) -> FileVersion {
    FileVersion {
        byte_len: metadata.len(),
        modified: metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
    }
}

/// 探测文件的当前版本；不是普通文件或不存在时返回 `None`（即确认缺失）。
pub(crate) fn current_version(path: &Path) -> Option<FileVersion> {
    let metadata = path.metadata().ok()?;
    metadata.is_file().then(|| version_of(&metadata))
}

/// 会话级观察表。对 runtime 只暴露"构造一个"这一件事：条目读写全在工具内部。
#[derive(Debug, Default)]
pub struct ObservedFiles {
    entries: Mutex<HashMap<String, Observed>>,
}

impl ObservedFiles {
    /// 记下某个目标的观察状态（读成功、读到缺失、写改成功都走这里）。
    pub(crate) fn record(&self, key: &str, observed: Observed) {
        lock_unpoisoned(&self.entries).insert(key.to_string(), observed);
    }

    /// 查询某个目标的观察状态；无条目即 [`Observed::Unseen`]。
    pub(crate) fn observed(&self, key: &str) -> Observed {
        lock_unpoisoned(&self.entries)
            .get(key)
            .copied()
            .unwrap_or(Observed::Unseen)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)] // 测试断言惯例
    use super::*;

    /// 版本探测的两个非显然点：长度变化即另一版（覆盖写的新鲜度判据），
    /// 路径不存在探测为 `None`（`read` 据此记确认缺失，之后 `write` 可安全重建）。
    #[test]
    fn version_probe_detects_growth_and_confirms_absence() {
        let dir = tempfile::tempdir().expect("workspace");
        let file = dir.path().join("a.txt");
        std::fs::write(&file, b"one").expect("write fixture");
        let version = current_version(&file).expect("fixture file has a version");
        std::fs::write(&file, b"one-one").expect("grow fixture");
        assert_ne!(
            current_version(&file),
            Some(version),
            "a different byte length must read as a different version"
        );
        assert!(
            current_version(&dir.path().join("missing.txt")).is_none(),
            "a missing path confirms absence"
        );
    }
}
