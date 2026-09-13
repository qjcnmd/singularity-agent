//! JSONL 会话条目 → 公开历史投影。
//!
//! IndexedTurn::project 只复制用户可见的 message/thinking/tool/settings/
//! compaction 字段，绝不序列化原始 entry 或其
//! provider_reasoning_replay。index_turn_history 按 run operation 起点建立条目范围，
//! ThreadSnapshot 仅投影请求页内的轮次，并按内容引用还原请求详情。

use singularity_agent::{
    message::{AgentMessage, ContentBlock},
    session::{LedgerRecord, OperationKind, SessionEntry, SessionMetadata},
};
use singularity_protocol::{HistoryItem, ThreadTurn, TurnStatus};

/// thread/read 的按轮分组投影。
///
/// run operation 的 operation_started 划定轮次边界；同 turn id 的
/// operation_finished 写入轮次状态而不是条目，message/compaction/settings
/// 投影为轮内条目。首个开始标记之前存在落盘条目时，它们构成一个
/// 无归属 turn 的前导组（turnId/status 为 null）；没有任何条目时不产生空组。
///
/// 崩溃遗留的未终止轮按 interrupted 投影；只有调用方确认本进程持有该
/// Thread 的活动写者时，末组才投影为 running。
pub(crate) struct IndexedTurn {
    pub turn_id: Option<String>,
    pub status: Option<TurnStatus>,
    pub entries: std::ops::Range<usize>,
}

impl IndexedTurn {
    pub fn cursor(&self) -> String {
        self.turn_id
            .as_ref()
            .map_or_else(|| "turn:leading".into(), |id| format!("turn:{id}"))
    }

    /// 按轮遍历持久条目并直接写入最终公开 items；工具 wire ID 映射和同一
    /// request 的多次观测归并都在这里完成。请求详情在本轮条目合并完成后
    /// 只展开一次，避免先生成临时身份再二次改写。
    pub fn project(&self, session: &singularity_agent::session::SessionData) -> ThreadTurn {
        let mut items = Vec::new();
        let mut request_positions = std::collections::HashMap::new();
        let mut tool_items = std::collections::HashMap::new();
        for entry in &session.entries()[self.entries.clone()] {
            match entry {
                SessionEntry::Message { message, id, .. } => match message {
                    AgentMessage::User { .. } | AgentMessage::Assistant { .. } => {
                        let role = if matches!(message, AgentMessage::User { .. }) {
                            "user"
                        } else {
                            "assistant"
                        };
                        let mut text_index = 0usize;
                        let mut thinking_index = 0usize;
                        let mut call_index = 0usize;
                        for block in message.content() {
                            match block {
                                ContentBlock::Text { text } if !text.is_empty() => {
                                    items.push(HistoryItem::Message {
                                        id: singularity_agent::session::text_item_id(
                                            id, text_index,
                                        ),
                                        role: role.to_string(),
                                        text: text.clone(),
                                    });
                                    text_index += 1;
                                }
                                ContentBlock::Thinking { thinking, .. } if !thinking.is_empty() => {
                                    items.push(HistoryItem::Thinking {
                                        id: singularity_agent::session::thinking_item_id(
                                            id,
                                            thinking_index,
                                        ),
                                        text: thinking.clone(),
                                    });
                                    thinking_index += 1;
                                }
                                ContentBlock::ToolCall {
                                    id: call_id,
                                    name,
                                    args,
                                } => {
                                    let item_id =
                                        singularity_agent::session::tool_item_id(id, call_index);
                                    call_index += 1;
                                    tool_items.insert(call_id.clone(), item_id.clone());
                                    items.push(HistoryItem::ToolCall {
                                        id: item_id,
                                        name: name.clone(),
                                        args: args.clone(),
                                    });
                                }
                                _ => {}
                            }
                        }
                    }
                    AgentMessage::ToolResult {
                        tool_call_id,
                        is_error,
                        duration_ms,
                        diff,
                        ..
                    } => {
                        let raw_id = tool_call_id.clone().unwrap_or_else(|| id.clone());
                        let item_id = tool_items.get(&raw_id).cloned().unwrap_or(raw_id);
                        items.push(HistoryItem::ToolResult {
                            id: item_id,
                            output: message.content_text(),
                            is_error: is_error.unwrap_or(false),
                            duration_ms: *duration_ms,
                            diff: diff.clone(),
                        });
                    }
                },
                SessionEntry::Compaction { compaction, id, .. } => {
                    items.push(HistoryItem::Compaction {
                        id: id.clone(),
                        summary: compaction.summary.clone(),
                    })
                }
                SessionEntry::Metadata { metadata, id, .. } => match metadata {
                    // thread 名称不是公开历史条目。
                    SessionMetadata::ThreadName { .. } => {}
                    SessionMetadata::ThreadSettings {
                        provider,
                        model,
                        reasoning,
                    } => items.push(HistoryItem::Settings {
                        id: id.clone(),
                        provider: provider.clone(),
                        model: model.clone(),
                        reasoning: reasoning.clone(),
                    }),
                },
                SessionEntry::Record {
                    id,
                    timestamp,
                    record: LedgerRecord::ModelRequest { observation, .. },
                } => {
                    let mut observation = observation.clone();
                    if observation.request_id.is_empty() {
                        observation.request_id = id.clone();
                    }
                    let request_id = observation.request_id.clone();
                    let request = HistoryItem::Request {
                        id: request_id.clone(),
                        timestamp: timestamp.clone(),
                        observation,
                    };
                    if let Some(&position) = request_positions.get(&request_id) {
                        items[position] = request;
                    } else {
                        request_positions.insert(request_id, items.len());
                        items.push(request);
                    }
                }
                SessionEntry::Record { .. } => {}
            }
        }
        // Started and Finished records share one immutable request. Merge their
        // observations first so the request head is expanded only once.
        for item in &mut items {
            let HistoryItem::Request {
                id, observation, ..
            } = item
            else {
                continue;
            };
            match session.request_head(id) {
                Ok(head) => observation.request_head = Some(head),
                Err(error) => observation.request_error = Some(error.to_string().into_boxed_str()),
            }
        }
        ThreadTurn {
            turn_id: self.turn_id.clone(),
            status: self.status,
            items,
        }
    }
}

/// 只索引轮次的条目范围与终态；公开正文和请求详情在请求分页时才构建。
pub(crate) fn index_turn_history(entries: &[SessionEntry], live_run: bool) -> Vec<IndexedTurn> {
    let mut turns: Vec<IndexedTurn> = Vec::new();
    for (position, entry) in entries.iter().enumerate() {
        if let SessionEntry::Record {
            record:
                LedgerRecord::OperationStarted {
                    kind: OperationKind::Run,
                    turn_id,
                    ..
                },
            ..
        } = entry
        {
            if let Some(last) = turns.last_mut() {
                last.entries.end = position;
            }
            turns.push(IndexedTurn {
                turn_id: turn_id.clone(),
                status: None,
                entries: position..entries.len(),
            });
            continue;
        }
        if turns.is_empty() {
            turns.push(IndexedTurn {
                turn_id: None,
                status: None,
                entries: position..entries.len(),
            });
        }
        if let SessionEntry::Record {
            record:
                LedgerRecord::OperationFinished {
                    turn_id: Some(id),
                    outcome,
                    ..
                },
            ..
        } = entry
            && let Some(last) = turns.last_mut()
            && last.status.is_none()
            && last.turn_id.as_ref() == Some(id)
        {
            last.status = Some(*outcome);
        }
    }
    let trailing = turns.len().saturating_sub(1);
    for (index, turn) in turns.iter_mut().enumerate() {
        if turn.turn_id.is_some() && turn.status.is_none() {
            turn.status = Some(if index == trailing && live_run {
                TurnStatus::Running
            } else {
                TurnStatus::Interrupted
            });
        }
    }
    turns
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)] // 测试断言惯例

    use super::*;
    use singularity_agent::session::{CompactionEntry, SessionManager, SessionMetadata};

    /// 压缩点与设置变更进入公开历史，供客户端回放；任务名称不属于会话内容。
    /// 测试通过真正的轮次投影入口，而不是单条 entry 的中间投影。
    #[test]
    fn compaction_and_settings_survive_the_public_projection() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = SessionManager::create(dir.path(), &dir.path().join("sessions")).unwrap();
        session
            .append_compaction_with_id(
                "c1",
                CompactionEntry {
                    summary: "kept summary".to_string(),
                    first_kept_entry_id: "m1".to_string(),
                    usage: None,
                    details: None,
                },
            )
            .unwrap();
        session
            .append_metadata(SessionMetadata::thread_settings(
                "opencode-go",
                "qwen3.8-flash",
                Some("high".to_string()),
            ))
            .unwrap();
        session
            .append_metadata(SessionMetadata::thread_name("x"))
            .unwrap();

        let indexed = index_turn_history(session.entries(), false);
        let projected = indexed[0].project(&session);
        assert_eq!(projected.items.len(), 2);
        assert!(matches!(
            &projected.items[0],
            HistoryItem::Compaction { id, summary }
                if id == "c1" && summary == "kept summary"
        ));
        assert!(matches!(
            &projected.items[1],
            HistoryItem::Settings {
                provider,
                model,
                reasoning,
                ..
            } if provider == "opencode-go"
                && model == "qwen3.8-flash"
                && reasoning.as_deref() == Some("high")
        ));
    }
}
