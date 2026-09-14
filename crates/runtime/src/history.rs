//! JSONL 会话条目 → 公开历史投影。
//!
//! IndexedTurn::project 只复制用户可见的 message/thinking/tool/settings/
//! compaction 字段，绝不序列化原始 entry 或其
//! provider_reasoning_replay。index_turn_history 按 run operation 起点建立条目范围，
//! 并归约每个回合的终态与手动停止事实；summarize_thread 从同一索引派生目录摘要，
//! ThreadSnapshot 仅投影请求页内的轮次，并按内容引用还原请求详情。

use singularity_agent::{
    message::{AgentMessage, ContentBlock},
    session::{
        LedgerRecord, OperationKind, SessionData, SessionEntry, SessionError, SessionMetadata,
    },
};
use singularity_protocol::{HistoryItem, ThreadSummary, ThreadTurn, TurnStatus};

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
    /// 本轮以 interrupted 结束且由用户停止触发；终态记录之外的回合为 false。
    pub manually_stopped: bool,
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
    pub fn project(&self, session: &SessionData) -> ThreadTurn {
        let mut items = Vec::new();
        let mut request_positions = std::collections::HashMap::new();
        let mut tool_items = std::collections::HashMap::new();
        for entry in &session.entries()[self.entries.clone()] {
            match entry {
                SessionEntry::Message { message, id, .. } => match message {
                    AgentMessage::User { .. } | AgentMessage::Assistant { .. } => {
                        for (ordinal, call) in message.tool_calls().enumerate() {
                            if let ContentBlock::ToolCall { id: call_id, .. } = call {
                                tool_items.insert(
                                    call_id.clone(),
                                    singularity_agent::session::tool_item_id(id, ordinal),
                                );
                            }
                        }
                        items.extend(message.public_items(id));
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
                    record:
                        LedgerRecord::ModelRequest {
                            observation,
                            context,
                        },
                } => {
                    let mut observation = observation.clone();
                    let missing_request_id = observation.request_id.is_empty();
                    if missing_request_id {
                        observation.request_id = id.clone();
                    }
                    let request_id = observation.request_id.clone();
                    if !missing_request_id && let Some(context) = context {
                        match session.request_head(context) {
                            Ok(head) => observation.request_head = Some(head),
                            Err(error) => {
                                observation.request_error = Some(error.to_string().into_boxed_str())
                            }
                        }
                    } else if let Some(&position) = request_positions.get(&request_id) {
                        if let HistoryItem::Request {
                            observation: previous,
                            ..
                        } = &mut items[position]
                        {
                            observation.request_head = previous.request_head.take();
                            observation.request_error = previous.request_error.take();
                        }
                    } else {
                        observation.request_error = Some(
                            SessionError::InvalidStructure(format!(
                                "request header not found: {request_id}"
                            ))
                            .to_string()
                            .into_boxed_str(),
                        );
                    }
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
        ThreadTurn {
            turn_id: self.turn_id.clone(),
            status: self.status,
            items,
        }
    }
}

/// 只索引轮次的条目范围、终态与手动停止事实；公开正文和请求详情在请求分页时才构建。
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
                manually_stopped: false,
                entries: position..entries.len(),
            });
            continue;
        }
        if turns.is_empty() {
            turns.push(IndexedTurn {
                turn_id: None,
                status: None,
                manually_stopped: false,
                entries: position..entries.len(),
            });
        }
        if let SessionEntry::Record {
            record:
                LedgerRecord::OperationFinished {
                    turn_id: Some(id),
                    outcome,
                    user_stopped,
                    ..
                },
            ..
        } = entry
            && let Some(last) = turns.last_mut()
            && last.status.is_none()
            && last.turn_id.as_ref() == Some(id)
        {
            last.status = Some(*outcome);
            last.manually_stopped = *outcome == TurnStatus::Interrupted && *user_stopped;
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

/// 列表摘要标题的长度上限。
const MAX_SESSION_TITLE_CHARS: usize = 8;

/// 从同一份回合索引派生目录摘要：轮数、最近一轮终态与手动停止取自索引，
/// 标题、模型设置和更新时间取自元数据与消息条目。不修复也不写入会话。
pub(crate) fn summarize_thread(session: &SessionData, turns: &[IndexedTurn]) -> ThreadSummary {
    let mut model = None;
    let mut title = None;
    let mut turn_count = 0usize;
    let mut status = None;
    let mut manually_stopped = false;
    for turn in turns.iter().filter(|turn| turn.turn_id.is_some()) {
        turn_count += 1;
        status = turn.status;
        manually_stopped = turn.manually_stopped;
    }
    // 反向遍历取最近的设置与名称；未命名时回落到首条用户输入。
    for entry in session.entries().iter().rev() {
        let SessionEntry::Metadata { metadata, .. } = entry else {
            continue;
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
        if title.is_none()
            && let SessionMetadata::ThreadName { name } = metadata
        {
            title = Some(name.clone());
        }
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
