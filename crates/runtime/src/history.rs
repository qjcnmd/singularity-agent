//! JSONL 会话条目 → 公开历史投影。
//!
//! `project_public_history` 只复制用户可见的 message/thinking/tool/settings/
//! compaction 字段，绝不序列化原始 entry 或其
//! `provider_reasoning_replay`。`project_turn_history`
//! 按 run operation 的 `operation_started` 划定轮次边界，产出协议层的公开
//! 历史类型（`ThreadTurn`/`HistoryItem`）；store 的 `paged_read` 在此基础上
//! 完成分页与整体状态精化。

use singularity_agent::{
    message::{AgentMessageRole, ContentBlock},
    session::{LedgerRecord, OperationKind, SessionEntry, SessionMetadata},
};
use singularity_protocol::{HistoryItem, ThreadTurn, TurnStatus};

/// 将内部 SessionEntry 转成稳定的公开 history item。该边界只复制用户可见的
/// message/thinking/tool/settings/compaction 字段，绝不序列化原始 entry
/// 或其 `provider_reasoning_replay`。ledger 记录全部是审计与恢复事实：
/// run 终态由 [`project_turn_history`] 归入 `ThreadTurn` 的身份与状态，
/// 其余记录（step/provider/tool/control 与 compaction operation）不进入公开历史。
pub(crate) fn project_public_history(entry: &SessionEntry) -> Vec<HistoryItem> {
    match entry {
        SessionEntry::Message { message, id, .. } => match message.role() {
            AgentMessageRole::User | AgentMessageRole::Assistant => {
                let role = if matches!(message.role(), AgentMessageRole::User) {
                    "user"
                } else {
                    "assistant"
                };
                let mut items = Vec::new();
                let mut text_index = 0usize;
                let mut thinking_index = 0usize;
                for block in message.content() {
                    match block {
                        ContentBlock::Text { text } if !text.is_empty() => {
                            items.push(HistoryItem::Message {
                                id: format!("{id}:text:{text_index}"),
                                role: role.to_string(),
                                text: text.clone(),
                            });
                            text_index += 1;
                        }
                        ContentBlock::Thinking { thinking, .. } if !thinking.is_empty() => {
                            items.push(HistoryItem::Thinking {
                                id: format!("{id}:thinking:{thinking_index}"),
                                text: thinking.clone(),
                            });
                            thinking_index += 1;
                        }
                        ContentBlock::ToolCall {
                            id: call_id,
                            name,
                            args,
                        } => {
                            items.push(HistoryItem::ToolCall {
                                id: call_id.clone(),
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
                    .tool_call_id()
                    .cloned()
                    .unwrap_or_else(|| id.clone()),
                output: message.content_text(),
                is_error: message.is_error().unwrap_or(false),
            }],
        },
        SessionEntry::Compaction { compaction, id, .. } => vec![HistoryItem::Compaction {
            id: id.clone(),
            summary: compaction.summary.clone(),
        }],
        SessionEntry::Metadata { metadata, id, .. } => match metadata {
            // thread 名称不是公开历史条目。
            SessionMetadata::ThreadName { .. } => Vec::new(),
            SessionMetadata::ThreadSettings {
                provider,
                model,
                reasoning,
            } => vec![HistoryItem::Settings {
                id: id.clone(),
                provider: provider.clone(),
                model: model.clone(),
                reasoning: reasoning.clone(),
            }],
        },
        SessionEntry::Record { .. } => Vec::new(),
    }
}

/// thread/read 的按轮分组投影。
///
/// run operation 的 `operation_started` 划定轮次边界；同 turn id 的
/// `operation_finished` 写入轮次状态而不是条目，message/compaction/settings
/// 投影为轮内条目。首个开始标记之前存在落盘条目时，它们构成一个
/// 无归属 turn 的前导组（turnId/status 为 null）；没有任何条目时不产生空组。
///
/// 崩溃遗留的未终止轮按 interrupted 投影；只有调用方确认本进程持有该
/// Thread 的活动写者时，末组才投影为 running。
pub(crate) fn project_turn_history(entries: &[SessionEntry], live_run: bool) -> Vec<ThreadTurn> {
    // 前导组按需创建：一旦出现过 turn 开始标记，后续条目都归属当前组。
    fn leading_or_last(turns: &mut Vec<ThreadTurn>) -> &mut ThreadTurn {
        if turns.is_empty() {
            turns.push(ThreadTurn {
                turn_id: None,
                status: None,
                items: Vec::new(),
            });
        }
        // 不变量：刚 push 过，last_mut 必存在。
        #[allow(clippy::expect_used)]
        turns.last_mut().expect("group just ensured")
    }

    let mut turns: Vec<ThreadTurn> = Vec::new();
    for entry in entries {
        match entry {
            SessionEntry::Record { record, .. } => match record {
                LedgerRecord::OperationStarted {
                    kind: OperationKind::Run,
                    turn_id,
                    ..
                } => turns.push(ThreadTurn {
                    turn_id: turn_id.clone(),
                    status: None,
                    items: Vec::new(),
                }),
                LedgerRecord::OperationFinished {
                    turn_id: Some(finished_turn_id),
                    outcome,
                    ..
                } => {
                    let last = leading_or_last(&mut turns);
                    // 只接受与当前轮身份相符的终态；错位记录不改变任何轮的状态，
                    // 事实本身完整保留在 ledger 中。
                    if last.status.is_none()
                        && last.turn_id.as_deref() == Some(finished_turn_id.as_str())
                    {
                        last.status = Some(*outcome);
                    }
                }
                // 独立 compaction 与审计记录不划定 turn 边界。
                _ => leading_or_last(&mut turns)
                    .items
                    .extend(project_public_history(entry)),
            },
            _ => leading_or_last(&mut turns)
                .items
                .extend(project_public_history(entry)),
        }
    }
    // 末组未终止轮只在本进程存在活动写者时投影为 running。
    if let Some(last) = turns.last_mut()
        && last.turn_id.is_some()
        && last.status.is_none()
    {
        last.status = Some(if live_run {
            TurnStatus::Running
        } else {
            TurnStatus::Interrupted
        });
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
