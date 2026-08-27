//! 会话发现与有界读投影。

use std::path::PathBuf;

use super::format::{Result, SessionEntry, SessionError};
use super::manager::SessionManager;

/// 一次会话读取的结果：最近一次 compaction 摘要 + 完整 leaf 条目序列。
///
/// 打开文件时的行数/单行大小上限仍然生效；条目序列是 turn 分页投影的
/// 唯一输入，任何窗口裁剪都发生在其上的过滤与分页层，不做物理切片。
#[derive(Debug, Clone, PartialEq)]
pub struct ThreadRead {
    pub summary: Option<String>,
    pub entries: Vec<SessionEntry>,
}

/// 按 `~/.singularity/sessions/<session_id>.jsonl` 布局读取会话的仓储入口。
#[derive(Debug, Clone)]
pub struct SessionRepository {
    sessions_dir: PathBuf,
}

impl SessionRepository {
    pub fn new(sessions_dir: impl Into<PathBuf>) -> Self {
        Self {
            sessions_dir: sessions_dir.into(),
        }
    }

    /// 读取会话：按 id 定位 rollout，校验 header id，返回摘要与完整 leaf 序列。
    pub fn read(&self, session_id: &str) -> Result<ThreadRead> {
        let path = self.sessions_dir.join(format!("{session_id}.jsonl"));
        let session = SessionManager::open_existing_read_only(&path)?;
        if session.session_id() != session_id {
            return Err(SessionError::InvalidSession(format!(
                "rollout header id {} does not match requested session id {session_id}",
                session.session_id()
            )));
        }
        Ok(ThreadRead {
            summary: session.summary(),
            entries: session.entries().to_vec(),
        })
    }
}

impl SessionManager {
    pub fn summary(&self) -> Option<String> {
        self.entries.iter().rev().find_map(|entry| match entry {
            SessionEntry::Compaction { compaction, .. } => Some(compaction.summary.clone()),
            _ => None,
        })
    }
}
