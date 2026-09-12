//! 从会话持久事实派生列表摘要，不执行修复或写入。

use super::{LedgerRecord, OperationKind, SessionData, SessionEntry, SessionMetadata};
use singularity_protocol::{ThreadSummary, TurnStatus};

const MAX_SESSION_TITLE_CHARS: usize = 8;

/// 投影有界、只读的 JSONL 事实，不修复或修改会话。
pub fn project_session(session: &SessionData, live_run: bool) -> ThreadSummary {
    use crate::message::AgentMessage;

    let mut model = None;
    let mut status = None;
    let mut title = None;
    let mut turn_count = 0usize;
    let mut open_run = false;
    // 反向遍历取最近的设置与终态，同时累计轮数。
    for entry in session.entries().iter().rev() {
        match entry {
            SessionEntry::Compaction { .. } => continue,
            SessionEntry::Message { .. } => continue,
            SessionEntry::Metadata { metadata, .. } => {
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
                if title.is_none()
                    && let SessionMetadata::ThreadName { name } = metadata
                {
                    title = Some(name.clone());
                }
            }
            SessionEntry::Record { record, .. } => match record {
                LedgerRecord::OperationStarted {
                    kind: OperationKind::Run,
                    ..
                } => {
                    turn_count += 1;
                    if status.is_none() {
                        // 反向遍历中先遇到 started：该 run 的终态在其后（更旧侧
                        // 不可能），说明它是最新轮且尚未终结。
                        open_run = true;
                    }
                }
                LedgerRecord::OperationFinished {
                    turn_id, outcome, ..
                } if status.is_none() && turn_id.is_some() => {
                    status = Some(*outcome);
                }
                _ => {}
            },
        }
    }
    if open_run {
        status = Some(if live_run {
            TurnStatus::Running
        } else {
            TurnStatus::Interrupted
        });
    }
    let title = title.or_else(|| {
        session.entries().iter().find_map(|entry| {
            let SessionEntry::Message { message, .. } = entry else {
                return None;
            };
            if !matches!(message, AgentMessage::User { .. }) {
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
    let latest_turn = session
        .entries()
        .iter()
        .rev()
        .find_map(|entry| match entry {
            SessionEntry::Record {
                record:
                    LedgerRecord::OperationStarted {
                        kind: OperationKind::Run,
                        turn_id,
                        ..
                    },
                ..
            } => turn_id.as_deref(),
            _ => None,
        });
    let manually_stopped = status == Some(TurnStatus::Interrupted)
        && session
            .entries()
            .iter()
            .rev()
            .find_map(|entry| match entry {
                SessionEntry::Record {
                    record:
                        LedgerRecord::OperationFinished {
                            turn_id: Some(id),
                            user_stopped,
                            ..
                        },
                    ..
                } if Some(id.as_str()) == latest_turn => Some(*user_stopped),
                _ => None,
            })
            .unwrap_or(false);

    let created_at = session.created_at().to_string();
    let updated_at = session
        .entries()
        .last()
        .map(|entry| match entry {
            SessionEntry::Message { timestamp, .. }
            | SessionEntry::Compaction { timestamp, .. }
            | SessionEntry::Metadata { timestamp, .. }
            | SessionEntry::Record { timestamp, .. } => timestamp.clone(),
        })
        .unwrap_or_else(|| created_at.clone());
    ThreadSummary {
        thread_id: session.session_id().to_string(),
        cwd: session.cwd_string(),
        created_at,
        updated_at,
        title,
        model,
        status,
        manually_stopped,
        turn_count,
    }
}
