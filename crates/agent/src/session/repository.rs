//! Session discovery and bounded read projection.

use std::path::PathBuf;

use super::format::{Result, SessionEntry, SessionEntryType, SessionError};
use super::manager::SessionManager;
/// session/read 的条目类型过滤。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SessionEntryFilter {
    #[default]
    All,
    Messages,
    Compactions,
}

/// `SessionRepository::read` 的有界读取选项。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionReadOptions {
    /// 返回当前 leaf 路径上的最近 N 条（0 = 只读摘要）。
    pub recent_limit: usize,
    pub filter: SessionEntryFilter,
    /// 在过滤后的路径条目上应用的半开范围（`[start, end)`）。
    pub range: Option<(usize, usize)>,
}

impl Default for SessionReadOptions {
    fn default() -> Self {
        Self {
            recent_limit: 20,
            filter: SessionEntryFilter::All,
            range: None,
        }
    }
}

/// 一次有界会话读取的结果：摘要 + 最近片段，不返回全文。
#[derive(Debug, Clone, PartialEq)]
pub struct SessionRead {
    pub summary: Option<String>,
    pub entries: Vec<SessionEntry>,
    /// 会话文件中的条目总数（不含 header）。
    pub total_entries: usize,
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

    /// 有界读取会话：按 id 定位 rollout，校验 header id，返回摘要 + 最近片段。
    pub fn read(&self, session_id: &str, options: &SessionReadOptions) -> Result<SessionRead> {
        let path = self.sessions_dir.join(format!("{session_id}.jsonl"));
        let session = SessionManager::open_existing(&path)?;
        if session.session_id() != session_id {
            return Err(SessionError::InvalidSession(format!(
                "rollout header id {} does not match requested session id {session_id}",
                session.session_id()
            )));
        }
        Ok(session.read_entries(options))
    }
}

impl SessionManager {
    pub fn summary(&self) -> Option<String> {
        let path = self.session_path();
        path.iter()
            .rev()
            .find_map(|&index| match &self.entries[index].entry_type {
                SessionEntryType::Compaction(entry) => Some(entry.summary.clone()),
                _ => None,
            })
    }

    pub fn total_entries(&self) -> usize {
        self.entries.len()
    }

    pub fn recent_entries(&self, limit: usize) -> Vec<SessionEntry> {
        let path = self.session_path();
        let start = path.len().saturating_sub(limit);
        path[start..]
            .iter()
            .map(|&index| self.entries[index].clone())
            .collect()
    }

    pub fn read_entries(&self, options: &SessionReadOptions) -> SessionRead {
        let path = self.session_path();
        let filtered = path
            .iter()
            .filter(|&&index| match options.filter {
                SessionEntryFilter::All => true,
                SessionEntryFilter::Messages => {
                    matches!(self.entries[index].entry_type, SessionEntryType::Message(_))
                }
                SessionEntryFilter::Compactions => {
                    matches!(
                        self.entries[index].entry_type,
                        SessionEntryType::Compaction(_)
                    )
                }
            })
            .copied()
            .collect::<Vec<_>>();
        let (start, end) = options.range.unwrap_or((0, filtered.len()));
        let start = start.min(filtered.len());
        let end = end.min(filtered.len());
        let selected = if start >= end {
            &filtered[..0]
        } else {
            &filtered[start..end]
        };
        let start = selected.len().saturating_sub(options.recent_limit);
        SessionRead {
            summary: self.summary(),
            entries: selected[start..]
                .iter()
                .map(|&index| self.entries[index].clone())
                .collect(),
            total_entries: self.entries.len(),
        }
    }
}
