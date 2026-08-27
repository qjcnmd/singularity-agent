//! 线性 JSONL Session 子系统的稳定 façade。
//!
//! `SessionManager` 仍是唯一的可变生命周期 owner；其公开合同由本模块
//! 重新导出，而 format/file/context/repair/repository 子模块承载各自的
//! schema、I/O、上下文、恢复和仓储接缝。客户端只依赖这里的 façade。

mod manager;
pub mod writer_lock;

pub mod context;
pub mod file;
pub mod format;
pub mod repair;
pub mod repository;

pub use file::now_iso;
pub use format::{
    CURRENT_SESSION_VERSION, CompactionEntry, Result, SessionEntry, SessionError,
    SessionMetadata, SessionMetadataKind,
};
pub use manager::SessionManager;
pub use repository::{SessionRepository, ThreadRead};

/// runtime 与 app-server 投影共享的 JSONL 派生事实。
#[derive(Debug, Clone, PartialEq)]
pub struct SessionProjection {
    pub session_id: String,
    pub cwd: String,
    pub created_at: String,
    pub updated_at: String,
    pub title: Option<String>,
    pub model: Option<String>,
    pub status: Option<SessionProjectionStatus>,
    pub latest_usage: Option<serde_json::Value>,
    pub turn_count: usize,
    pub total_tokens: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionProjectionStatus {
    Active,
    Completed,
    Failed,
    Interrupted,
}

pub const MAX_SESSION_TITLE_CHARS: usize = 120;

/// 投影有界、只读的 JSONL 事实，不修复或修改会话。
pub fn project_session(session: &SessionManager) -> SessionProjection {
    use crate::message::AgentMessageRole;

    let metadata = session.metadata_entries();
    let model = metadata.iter().rev().find_map(|entry| match entry {
        SessionMetadata::ThreadSettings {
            provider,
            model,
            reasoning,
        } => Some(
            match provider.as_deref().filter(|value| !value.is_empty()) {
                Some(provider) => singularity_model::compose_model_selector(
                    provider,
                    model,
                    reasoning.as_deref().filter(|value| !value.is_empty()),
                ),
                None => match reasoning.as_deref().filter(|value| !value.is_empty()) {
                    Some(reasoning) => format!("{model}#{reasoning}"),
                    None => model.clone(),
                },
            },
        ),
        _ => None,
    });
    let status = metadata.iter().rev().find_map(|entry| match entry.kind() {
        SessionMetadataKind::TurnStarted => Some(SessionProjectionStatus::Active),
        SessionMetadataKind::TurnCompleted => Some(SessionProjectionStatus::Completed),
        SessionMetadataKind::TurnFailed => Some(SessionProjectionStatus::Failed),
        SessionMetadataKind::TurnInterrupted => Some(SessionProjectionStatus::Interrupted),
        _ => None,
    });
    let latest_usage = metadata.iter().rev().find_map(|entry| match entry {
        SessionMetadata::Usage { usage, .. } => Some(usage.clone()),
        _ => None,
    });
    let title = metadata
        .iter()
        .rev()
        .find_map(|entry| match entry {
            SessionMetadata::ThreadName { name } => Some(name.clone()),
            _ => None,
        })
        .or_else(|| {
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
    let total_tokens = metadata
        .iter()
        .filter_map(|entry| match entry {
            SessionMetadata::Usage { usage, .. } => {
                usage.get("totalTokens").and_then(serde_json::Value::as_u64)
            }
            _ => None,
        })
        .sum();
    let turn_count = metadata
        .iter()
        .filter(|entry| entry.kind() == SessionMetadataKind::TurnStarted)
        .count();
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
    SessionProjection {
        session_id: session.session_id().to_string(),
        cwd: session.cwd().to_string_lossy().to_string(),
        created_at,
        updated_at,
        title,
        model,
        status,
        latest_usage,
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

#[cfg(test)]
#[path = "../wire_fixture_tests.rs"]
mod wire_fixture_tests;
