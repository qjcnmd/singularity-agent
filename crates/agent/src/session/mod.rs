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

/// runtime 与 app-server 投影共享的 JSONL 派生事实。
#[derive(Debug, Clone, PartialEq)]
pub struct SessionProjection {
    pub session_id: String,
    pub cwd: String,
    pub created_at: String,
    pub updated_at: String,
    pub title: Option<String>,
    pub model: Option<String>,
    pub status: Option<TurnStatus>,
    pub latest_usage: Option<TurnModelUsage>,
    pub turn_count: usize,
    pub total_tokens: u64,
}

pub const MAX_SESSION_TITLE_CHARS: usize = 120;

/// 投影有界、只读的 JSONL 事实，不修复或修改会话。
pub fn project_session(session: &SessionManager) -> SessionProjection {
    use crate::message::AgentMessageRole;

    let mut model = None;
    let mut status = None;
    let mut latest_usage = None;
    let mut title = None;
    let mut total_tokens = 0u64;
    let mut turn_count = 0usize;
    // 单趟反向遍历完成全部六个投影：各"最近一个"字段取首个命中，
    // 聚合字段累加。
    for entry in session.metadata_entries().iter().rev() {
        if model.is_none()
            && let SessionMetadata::ThreadSettings {
                provider,
                model: model_name,
                reasoning,
            } = entry
        {
            model = Some(
                match provider.as_deref().filter(|value| !value.is_empty()) {
                    Some(provider) => singularity_model::compose_model_selector(
                        provider,
                        model_name,
                        reasoning.as_deref().filter(|value| !value.is_empty()),
                    ),
                    None => match reasoning.as_deref().filter(|value| !value.is_empty()) {
                        Some(reasoning) => format!("{model_name}#{reasoning}"),
                        None => model_name.clone(),
                    },
                },
            );
        }
        if status.is_none() {
            status = match entry.kind() {
                SessionMetadataKind::TurnStarted => Some(TurnStatus::Running),
                SessionMetadataKind::TurnTerminal => {
                    entry.terminal_status().map(TurnTerminalStatus::turn_status)
                }
                _ => None,
            };
        }
        if latest_usage.is_none()
            && let SessionMetadata::TurnTerminal { usage, .. } = entry
        {
            latest_usage = Some(usage.clone());
        }
        if title.is_none()
            && let SessionMetadata::ThreadName { name } = entry
        {
            title = Some(name.clone());
        }
        if let SessionMetadata::TurnTerminal { usage, .. } = entry {
            total_tokens += usage.total_tokens;
        }
        if entry.kind() == SessionMetadataKind::TurnStarted {
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
