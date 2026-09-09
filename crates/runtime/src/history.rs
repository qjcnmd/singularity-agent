//! JSONL 会话条目 → 公开历史投影。
//!
//! project_public_history 只复制用户可见的 message/thinking/tool/settings/
//! compaction 字段，绝不序列化原始 entry 或其
//! provider_reasoning_replay。index_turn_history 按 run operation 起点建立条目范围，
//! ThreadSnapshot 仅投影请求页内的轮次，并按内容引用还原请求详情。

use singularity_agent::{
    message::{AgentMessage, ContentBlock},
    session::{LedgerRecord, OperationKind, SessionEntry, SessionMetadata, reduce_controls},
};
use singularity_protocol::{ControlSnapshot, HistoryItem, ThreadTurn, TurnStatus};

/// 将 durable control ledger 折叠为浏览器可见的完整控制生命周期。identity、
/// channel、sequence 与最终 disposition 全部来自同一条 ledger 归约路径。
pub(crate) fn project_control_history(entries: &[SessionEntry]) -> Vec<ControlSnapshot> {
    reduce_controls(entries)
        .into_iter()
        .map(|control| ControlSnapshot {
            control_id: control.control_id,
            turn_id: control.turn_id,
            channel: control.channel,
            sequence: control.sequence,
            text: control.text,
            disposition: control.disposition,
        })
        .collect()
}

/// 将内部 SessionEntry 转成稳定的公开 history item。该边界只复制用户可见的
/// message/thinking/tool/settings/compaction 字段，绝不序列化原始 entry
/// 或其 provider_reasoning_replay。文件指令与剪枝替换只影响模型视图：
/// run 终态由轮次索引归入 ThreadTurn 的身份与状态，
/// 其余记录（step/provider/tool/control 与 compaction operation）不进入公开历史。
pub(crate) fn project_public_history(entry: &SessionEntry) -> Vec<HistoryItem> {
    match entry {
        SessionEntry::Message { message, id, .. } => match message {
            AgentMessage::User { .. } | AgentMessage::Assistant { .. } => {
                let role = if matches!(message, AgentMessage::User { .. }) {
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
            AgentMessage::ToolResult {
                tool_call_id,
                is_error,
                duration_ms,
                diff,
                ..
            } => vec![HistoryItem::ToolResult {
                id: tool_call_id.clone().unwrap_or_else(|| id.clone()),
                output: message.content_text(),
                is_error: is_error.unwrap_or(false),
                duration_ms: *duration_ms,
                diff: diff.clone(),
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
        SessionEntry::Record {
            id,
            timestamp,
            record: LedgerRecord::ModelRequest { observation, .. },
        } => vec![HistoryItem::Request {
            id: id.clone(),
            timestamp: timestamp.clone(),
            observation: observation.clone(),
        }],
        SessionEntry::Record { .. } => Vec::new(),
    }
}

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

    pub fn project(
        &self,
        session: &singularity_agent::session::SessionData,
    ) -> Result<ThreadTurn, String> {
        let mut items = Vec::new();
        let mut request_positions = std::collections::HashMap::new();
        for entry in &session.entries()[self.entries.clone()] {
            for mut item in project_public_history(entry) {
                if let HistoryItem::Request {
                    id, observation, ..
                } = &mut item
                {
                    if observation.request_id.is_empty() {
                        observation.request_id = id.clone();
                    }
                    *id = observation.request_id.clone();
                    match session.request_head(id) {
                        Ok(head) => observation.request_head = Some(head),
                        Err(error) => {
                            observation.request_error = Some(error.to_string().into_boxed_str())
                        }
                    }
                    observation.request = None;
                    if let Some(&position) = request_positions.get(id) {
                        items[position] = item;
                        continue;
                    }
                    request_positions.insert(id.clone(), items.len());
                }
                items.push(item);
            }
        }
        Ok(ThreadTurn {
            turn_id: self.turn_id.clone(),
            status: self.status,
            items,
        })
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
    use singularity_agent::session::CompactionEntry;

    const TS: &str = "2026-09-02T00:00:00.000Z";

    /// 压缩点与设置变更进入公开历史，供客户端回放；任务名称不属于会话内容。
    #[test]
    fn compaction_and_settings_survive_the_public_projection() {
        let compaction = project_public_history(&SessionEntry::Compaction {
            id: "c1".to_string(),
            timestamp: TS.to_string(),
            compaction: CompactionEntry {
                summary: "kept summary".to_string(),
                first_kept_entry_id: "m1".to_string(),
                usage: None,
                details: None,
            },
        });
        assert!(matches!(&compaction[..],
                [HistoryItem::Compaction { id, summary }] if id == "c1" && summary == "kept summary"));

        let settings = project_public_history(&SessionEntry::Metadata {
            id: "s1".to_string(),
            timestamp: TS.to_string(),
            metadata: SessionMetadata::ThreadSettings {
                provider: "opencode-go".to_string(),
                model: "qwen3.8-flash".to_string(),
                reasoning: Some("high".to_string()),
            },
        });
        assert!(matches!(&settings[..],
                [HistoryItem::Settings { provider, model, reasoning, .. }]
                    if provider == "opencode-go"
                        && model == "qwen3.8-flash"
                        && reasoning.as_deref() == Some("high")));

        assert!(
            project_public_history(&SessionEntry::Metadata {
                id: "n1".to_string(),
                timestamp: TS.to_string(),
                metadata: SessionMetadata::ThreadName {
                    name: "x".to_string()
                },
            })
            .is_empty()
        );
    }
}
