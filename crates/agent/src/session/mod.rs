//! 线性 JSONL Session 子系统的稳定 façade。
//!
//! `SessionManager` 仍是唯一的可变生命周期 owner；其公开合同由本模块
//! 重新导出，而 format/file/context/repair 子模块承载各自的 schema、I/O、
//! 上下文和恢复接缝。客户端只依赖这里的 façade。

mod manager;
pub mod writer_lock;

pub mod context;
pub mod file;
pub mod format;
pub mod repair;

pub use file::now_iso;
pub use format::{
    CURRENT_SESSION_VERSION, CompactionEntry, Result, SessionEntry, SessionError, SessionMetadata,
    SessionMetadataKind, TurnTerminalStatus, turn_usage_from_model_usage,
};
pub use manager::{SessionAccess, SessionManager};
pub use singularity_protocol::{TurnModelUsage, TurnStatus};
pub use writer_lock::{WriterLockCoordinator, WriterLockGuard};

/// 会话列表所需的头部事实（对齐 pi `jsonl/repo.ts:65-87`：列表只读文件
/// 首行，不解析条目）。
#[derive(Debug, Clone, PartialEq)]
pub struct SessionHeaderInfo {
    pub session_id: String,
    pub cwd: String,
    pub created_at: String,
}

/// 只读 JSONL 首行并严格校验 header。损坏文件、非当前版本与非法 header
/// 一律 `Err`——列表路径逐项跳过，单个坏文件不阻断其余会话。
pub fn read_session_header(path: &std::path::Path) -> Result<SessionHeaderInfo> {
    use std::io::BufRead;
    let file = std::fs::File::open(path)?;
    let mut reader = std::io::BufReader::new(file);
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Err(SessionError::InvalidHeader(
            "session file is empty".to_string(),
        ));
    }
    let value: serde_json::Value = serde_json::from_str(line.trim_end())?;
    let (session_id, _version, cwd, timestamp) = format::validate_header(&value)?;
    Ok(SessionHeaderInfo {
        session_id,
        cwd,
        created_at: timestamp,
    })
}

/// JSONL 派生的 Thread 摘要：会话层唯一投影产物，runtime、app-server 与
/// TUI 共用同一结构，不存在第二份同形镜像。
#[derive(Debug, Clone, PartialEq)]
pub struct ThreadSummary {
    pub thread_id: String,
    pub cwd: String,
    pub created_at: String,
    pub updated_at: String,
    pub title: Option<String>,
    pub model: Option<String>,
    pub status: Option<TurnStatus>,
    pub turn_count: usize,
    pub total_tokens: u64,
}

pub const MAX_SESSION_TITLE_CHARS: usize = 120;

/// 投影有界、只读的 JSONL 事实，不修复或修改会话。
pub fn project_session(session: &SessionManager) -> ThreadSummary {
    use crate::message::AgentMessageRole;

    let mut model = None;
    let mut status = None;
    let mut title = None;
    let mut total_tokens = 0u64;
    let mut turn_count = 0usize;
    // 单趟反向遍历全部条目完成五个投影：各"最近一个"字段取首个命中，
    // 聚合字段累加。compaction 的 usage 计入累计（摘要请求同样是会话成本）。
    for entry in session.entries().iter().rev() {
        let metadata = match entry {
            SessionEntry::Metadata { metadata, .. } => metadata,
            SessionEntry::Compaction { compaction, .. } => {
                if let Some(usage) = &compaction.usage {
                    total_tokens += usage.total_tokens;
                }
                continue;
            }
            SessionEntry::Message { .. } => continue,
        };
        if model.is_none()
            && let SessionMetadata::ThreadSettings {
                provider,
                model: model_name,
                reasoning,
            } = metadata
        {
            model = Some(singularity_model::compose_model_selector(
                provider,
                model_name,
                reasoning.as_deref().filter(|value| !value.is_empty()),
            ));
        }
        if status.is_none() {
            status = match metadata.kind() {
                SessionMetadataKind::TurnStarted => Some(TurnStatus::Running),
                SessionMetadataKind::TurnTerminal => metadata
                    .terminal_status()
                    .map(TurnTerminalStatus::turn_status),
                _ => None,
            };
        }
        if title.is_none()
            && let SessionMetadata::ThreadName { name } = metadata
        {
            title = Some(name.clone());
        }
        if let SessionMetadata::TurnTerminal { usage, .. } = metadata {
            total_tokens += usage.total_tokens;
        }
        if metadata.kind() == SessionMetadataKind::TurnStarted {
            turn_count += 1;
        }
    }
    let title = title.or_else(|| {
        session.entries().iter().find_map(|entry| {
            let SessionEntry::Message { message, .. } = entry else {
                return None;
            };
            if message.role() != AgentMessageRole::User {
                return None;
            }
            let title = message
                .content_text()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .chars()
                .take(MAX_SESSION_TITLE_CHARS)
                .collect::<String>();
            (!title.is_empty()).then_some(title)
        })
    });
    let created_at = session.created_at().to_string();
    let updated_at = session
        .entries()
        .last()
        .and_then(|entry| match entry {
            SessionEntry::Message { timestamp, .. }
            | SessionEntry::Compaction { timestamp, .. }
            | SessionEntry::Metadata { timestamp, .. } => timestamp.clone(),
        })
        .unwrap_or_else(|| created_at.clone());
    ThreadSummary {
        thread_id: session.session_id().to_string(),
        cwd: session.cwd().to_string_lossy().to_string(),
        created_at,
        updated_at,
        title,
        model,
        status,
        turn_count,
        total_tokens,
    }
}

#[cfg(test)]
pub(crate) use file::{AppendLimits, normalize_cwd_string};

#[cfg(test)]
use crate::message::{AgentMessage, AgentMessageRole, ContentBlock};

#[cfg(test)]
use serde_json::{Value, json};

#[cfg(test)]
#[path = "../session_tests.rs"]
mod tests;
