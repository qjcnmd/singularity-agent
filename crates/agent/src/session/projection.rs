//! 从会话持久事实派生列表摘要，不执行修复或写入。

use super::{
    ControlChannel, LedgerRecord, OperationKind, SessionData, SessionEntry, SessionMetadata,
};
use singularity_protocol::{ThreadSummary, TurnStatus};
use std::collections::{BTreeMap, HashMap};

const MAX_SESSION_TITLE_CHARS: usize = 8;

/// 投影有界、只读的 JSONL 事实，不修复或修改会话。
pub fn project_session(session: &SessionData, live_run: bool) -> ThreadSummary {
    use crate::message::AgentMessageRole;

    let mut model = None;
    let mut status = None;
    let mut title = None;
    let total_tokens = total_tokens(session.entries());
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
        && latest_turn.is_some_and(|latest| {
            session.entries().iter().any(|entry| {
                matches!(entry,
            SessionEntry::Record { record: LedgerRecord::ControlAccepted {
                turn_id, channel: ControlChannel::Cancel, ..
            }, .. } if turn_id == latest)
            })
        });
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
        total_tokens,
    }
}

fn total_tokens(entries: &[SessionEntry]) -> u64 {
    let mut total = 0u64;
    let mut positions = HashMap::new();
    let mut pending = BTreeMap::<usize, u64>::new();
    for (position, entry) in entries.iter().enumerate() {
        match entry {
            SessionEntry::Record {
                record: LedgerRecord::OperationStarted { operation_id, .. },
                ..
            } => {
                if let std::collections::hash_map::Entry::Vacant(slot) =
                    positions.entry(operation_id)
                {
                    slot.insert(position);
                    pending.insert(position, 0);
                }
            }
            SessionEntry::Compaction { compaction, .. } => {
                if let Some(usage) = &compaction.usage {
                    if let Some(mut tokens) = pending.last_entry() {
                        *tokens.get_mut() = tokens.get().saturating_add(usage.total_tokens);
                    } else {
                        total = total.saturating_add(usage.total_tokens);
                    }
                }
            }
            SessionEntry::Record {
                record:
                    LedgerRecord::OperationFinished {
                        operation_id,
                        usage,
                        ..
                    },
                ..
            } => {
                let summaries = positions
                    .get(operation_id)
                    .and_then(|position| pending.remove(position))
                    .unwrap_or(0);
                // v5/v6 terminal accounting includes summary calls. Interrupted
                // repair records have no observed usage; retain their summaries,
                // as well as summaries written outside a completed operation.
                let tokens = match usage {
                    Some(usage) if usage.usage_present => usage.total_tokens,
                    usage => summaries
                        .saturating_add(usage.as_ref().map_or(0, |usage| usage.total_tokens)),
                };
                total = total.saturating_add(tokens);
            }
            _ => {}
        }
    }
    pending
        .values()
        .fold(total, |total, tokens| total.saturating_add(*tokens))
}
