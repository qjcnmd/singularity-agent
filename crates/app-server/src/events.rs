//! Lifecycle event projection.
//!
//! stdio 单连接传输下事件随请求响应全量发送；协议中不存在 `event/subscribe`
//! 或订阅状态，客户端把 matching response 之前的 notification 关联到本次请求。
//! 执行期事件的逐条映射在 [`crate::lifecycle`] 投影适配器中完成，本模块只
//! 保留通知包装与历史投影。

use super::*;
use singularity_agent::{
    message::{AgentMessageRole, ContentBlock},
    session::{SessionEntry, SessionEntryType, SessionMetadata, SessionMetadataKind},
};

/// 将内部 SessionEntry 转成稳定的公开 history item。该边界只复制用户可见的
/// message/thinking/tool/turn/settings/usage/compaction 字段，绝不序列化原始 entry
/// 或其 `provider_reasoning_replay`、parent/tree、迁移字段。
pub(crate) fn project_public_history(entry: &SessionEntry) -> Vec<HistoryItem> {
    match &entry.entry_type {
        SessionEntryType::Message(message) => match message.role {
            AgentMessageRole::User | AgentMessageRole::Assistant => {
                let role = if matches!(message.role, AgentMessageRole::User) {
                    "user"
                } else {
                    "assistant"
                };
                let mut items = Vec::new();
                let mut text_index = 0usize;
                let mut thinking_index = 0usize;
                for block in &message.content {
                    match block {
                        ContentBlock::Text { text } if !text.is_empty() => {
                            items.push(HistoryItem::Message {
                                id: format!("{}:text:{text_index}", entry.id),
                                role: role.to_string(),
                                text: text.clone(),
                            });
                            text_index += 1;
                        }
                        ContentBlock::Thinking { thinking, .. } if !thinking.is_empty() => {
                            items.push(HistoryItem::Thinking {
                                id: format!("{}:thinking:{thinking_index}", entry.id),
                                text: thinking.clone(),
                            });
                            thinking_index += 1;
                        }
                        ContentBlock::ToolCall { id, name, args } => {
                            items.push(HistoryItem::ToolCall {
                                id: id.clone(),
                                name: name.clone(),
                                args: args.clone(),
                            });
                        }
                        _ => {}
                    }
                }
                items
            }
            AgentMessageRole::ToolResult => vec![HistoryItem::ToolResult {
                id: message
                    .tool_call_id
                    .clone()
                    .unwrap_or_else(|| entry.id.clone()),
                output: message.content_text(),
                is_error: message.is_error.unwrap_or(false),
            }],
        },
        SessionEntryType::Compaction(compaction) => vec![HistoryItem::Compaction {
            id: entry.id.clone(),
            summary: compaction.summary.clone(),
        }],
        SessionEntryType::Metadata(metadata) => match metadata {
            SessionMetadata::TurnStarted { turn_id } => vec![HistoryItem::Turn {
                id: turn_id.clone(),
                status: TurnStatus::Running,
            }],
            SessionMetadata::TurnCompleted { turn_id }
            | SessionMetadata::TurnFailed { turn_id, .. }
            | SessionMetadata::TurnInterrupted { turn_id, .. } => vec![HistoryItem::Turn {
                id: turn_id.clone(),
                status: terminal_turn_status(metadata.kind()),
            }],
            SessionMetadata::ThreadSettings {
                provider,
                model,
                reasoning,
            } => vec![HistoryItem::Settings {
                id: entry.id.clone(),
                provider: provider.clone(),
                model: Some(model.clone()),
                reasoning: reasoning.clone(),
            }],
            SessionMetadata::ThreadName { .. } => Vec::new(),
            SessionMetadata::Usage { usage, .. } => vec![HistoryItem::Usage {
                id: entry.id.clone(),
                usage: usage.clone(),
            }],
        },
    }
}

fn terminal_turn_status(kind: SessionMetadataKind) -> TurnStatus {
    match kind {
        SessionMetadataKind::TurnCompleted => TurnStatus::Completed,
        SessionMetadataKind::TurnFailed => TurnStatus::Failed,
        _ => TurnStatus::Interrupted,
    }
}

/// thread/read 的按轮分组投影。
///
/// turn 开始 metadata 划定轮次边界；同 id 的终态 metadata 写入轮次身份而
/// 不是条目，message/compaction/settings/usage 全部投影为轮内条目。首个
/// 开始标记之前存在落盘条目时，它们构成一个无归属 turn 的前导组
/// （turnId/status 为 null）；没有任何条目时不产生空组。
///
/// 崩溃遗留的未终止轮：非末组直接按 interrupted 投影（与 reopen repair 的
/// 落盘结果一致）；末组保持 running，由调用方依据整体状态投影修正——只有
/// 本进程存在该会话的存活 turn 时 running 才成立。
pub(crate) fn project_turn_history(entries: &[SessionEntry]) -> Vec<ThreadTurn> {
    // 前导组按需创建：一旦出现过 turn 开始标记，后续条目都归属当前组。
    fn leading_or_last(turns: &mut Vec<ThreadTurn>) -> &mut ThreadTurn {
        if turns.is_empty() {
            turns.push(ThreadTurn {
                turn_id: None,
                status: None,
                items: Vec::new(),
            });
        }
        turns.last_mut().expect("group just ensured")
    }

    let mut turns: Vec<ThreadTurn> = Vec::new();
    for entry in entries {
        match &entry.entry_type {
            SessionEntryType::Metadata(metadata) => match metadata.kind() {
                SessionMetadataKind::TurnStarted => turns.push(ThreadTurn {
                    turn_id: metadata.turn_id().map(str::to_string),
                    status: None,
                    items: Vec::new(),
                }),
                kind if kind.matches_turn_terminal() => {
                    let last = leading_or_last(&mut turns);
                    let matched = last.status.is_none()
                        && last.turn_id.is_some()
                        && last.turn_id.as_deref() == metadata.turn_id();
                    if matched {
                        last.status = Some(terminal_turn_status(kind));
                    } else {
                        // 异常布局（缺开始标记或错位 id）的终态标记保真为条目。
                        let items = project_public_history(entry);
                        last.items.extend(items);
                    }
                }
                _ => leading_or_last(&mut turns)
                    .items
                    .extend(project_public_history(entry)),
            },
            _ => leading_or_last(&mut turns)
                .items
                .extend(project_public_history(entry)),
        }
    }
    // 末组未终止轮保持 running 投影（本进程存活 turn 的真实状态）；
    // 调用方依据整体状态投影把崩溃遗留修正为 interrupted。
    if let Some(last) = turns.last_mut()
        && last.turn_id.is_some()
        && last.status.is_none()
    {
        last.status = Some(TurnStatus::Running);
    }
    // 非末组的未终止轮只能是崩溃或损坏遗留，不伪装成运行中。
    let trailing = turns.len().saturating_sub(1);
    for turn in &mut turns[..trailing] {
        if turn.turn_id.is_some() && turn.status.is_none() {
            turn.status = Some(TurnStatus::Interrupted);
        }
    }
    turns
}

impl AppServer {
    /// 将应用事件包装为 JSON-RPC notification。
    pub(super) fn event_notification(&self, event: AppEvent) -> AppServerResult<Value> {
        Ok(event.to_notification().to_wire_value())
    }
}
