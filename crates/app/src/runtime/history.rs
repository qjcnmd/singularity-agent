//! 从会话日志生成工作台看到的历史、任务摘要和累计用量。
//! 历史只包含用户能看到的消息、思考、工具和压缩内容，不带原始日志行或
//! 提供方私有字段；任务摘要直接读取最近回合的状态，完整历史才划分全部回合。

use singularity_agent::session::{LedgerRecord, SessionData, SessionEntry, SessionError, SessionMetadata};
use singularity_model::{ContentBlock, Message};
use singularity_protocol::{
    HistoryItem, SessionModelUsage, ThreadSummary, ThreadTurn, ToolStatus, TurnStatus,
};

/// 一个回合在日志中的条目范围和最终状态；初始设置不构成回合。
pub(crate) struct IndexedTurn {
    pub turn_id: String,
    pub status: TurnStatus,
    pub error: Option<singularity_protocol::TurnErrorDetail>,
    /// 用户主动停止导致的中断。
    pub manually_stopped: bool,
    pub entries: std::ops::Range<usize>,
}

impl IndexedTurn {
    /// 生成这个回合的公开历史；工具结果合并到对应的调用条目里。
    pub fn project(&self, session: &SessionData) -> Result<ThreadTurn, SessionError> {
        let mut items = Vec::new();
        let started_at = session.entries()[self.entries.start].timestamp().to_owned();
        let mut finished_at = None;
        let mut first_token_at = None;
        let mut tool_items = std::collections::HashMap::new();
        // 每条回复显示截至当时的会话累计用量：先累计本回合之前的请求，再随条目推进。
        let mut usage = session_usage(&session.entries()[..self.entries.start]);
        for entry in &session.entries()[self.entries.clone()] {
            if let Some(observation) = entry.request_observation() {
                accumulate_request_usage(&mut usage, observation);
            }
            match entry {
                SessionEntry::Message { message, id, observation, timestamp, .. } => match message {
                    Message::Developer { .. } | Message::Notification { .. } => {}
                    Message::User { .. } | Message::Assistant { .. } => {
                        let mut calls = message.tool_calls();
                        // 收到第一个生成增量的时间 = 消息保存时间 - 生成耗时。
                        if first_token_at.is_none()
                            && matches!(message, Message::Assistant { .. })
                            && let Some(decode_ms) =
                                observation.as_ref().and_then(|observation| observation.decode_ms)
                        {
                            first_token_at = singularity_core::iso_minus_millis(timestamp, decode_ms);
                        }
                        let status = match observation.as_ref().map(|observation| observation.status) {
                            Some(singularity_protocol::ProviderAttemptStatus::Error) => TurnStatus::Failed,
                            Some(singularity_protocol::ProviderAttemptStatus::Cancelled) => {
                                TurnStatus::Interrupted
                            }
                            _ => TurnStatus::Completed,
                        };
                        let mut public = singularity_agent::message::public_items(
                            message,
                            id,
                            status,
                            ToolStatus::Interrupted,
                            timestamp,
                        );
                        if observation.is_some() {
                            singularity_agent::message::attach_usage(&mut public, &usage);
                        }
                        for item in public {
                            if matches!(item, HistoryItem::Tool { .. }) {
                                let call = calls.next().expect("public tools follow message order");
                                tool_items.insert(call.tool_call_id.clone(), items.len());
                            }
                            items.push(item);
                        }
                    }
                    Message::ToolResult { tool_call_id, .. } => {
                        // 结果写回调用所在的位置，界面拿到的是完整的工具条目。
                        let position = *tool_items.get(tool_call_id).ok_or_else(|| {
                            SessionError::InvalidSession(format!(
                                "tool result {id} refers to missing tool call {tool_call_id}"
                            ))
                        })?;
                        let HistoryItem::Tool { result, status, .. } = &mut items[position] else {
                            unreachable!("tool positions contain calls");
                        };
                        let displayed = singularity_agent::message::public_tool_result(message)
                            .expect("matched a tool result message");
                        *status = if displayed.is_error {
                            ToolStatus::Failed
                        } else {
                            ToolStatus::Completed
                        };
                        *result = Some(displayed);
                    }
                },
                SessionEntry::Compaction { compaction, id, .. } => {
                    items.push(HistoryItem::Compaction {
                        id: id.clone(),
                        summary: compaction.summary.clone(),
                    });
                }
                SessionEntry::Metadata { .. } => {}
                SessionEntry::Record {
                    timestamp,
                    record: LedgerRecord::OperationFinished { turn_id: id, .. },
                    ..
                } if id == &self.turn_id => finished_at = Some(timestamp.clone()),
                SessionEntry::Record { .. } => {}
            }
        }
        Ok(ThreadTurn {
            started_at,
            finished_at,
            first_token_at,
            turn_id: self.turn_id.clone(),
            status: self.status,
            manually_stopped: self.manually_stopped,
            error: self.error.clone(),
            items,
        })
    }
}

/// 用户主动停止造成的中断；写入结束记录和读取历史共用这一判定。
pub(crate) fn is_manually_stopped(outcome: TurnStatus, user_stopped: bool) -> bool {
    outcome == TurnStatus::Interrupted && user_stopped
}

/// 按回合开始记录划分日志，只记下范围、最终状态、错误和是否手动停止；
/// 读取完整历史时才生成具体内容。没有结束记录的回合视为中断。
pub(crate) fn index_turn_history(entries: &[SessionEntry]) -> Vec<IndexedTurn> {
    let mut turns: Vec<IndexedTurn> = Vec::new();
    for (position, entry) in entries.iter().enumerate() {
        if let SessionEntry::Record {
            id: turn_id,
            record: LedgerRecord::OperationStarted,
            ..
        } = entry
        {
            if let Some(last) = turns.last_mut() {
                last.entries.end = position;
            }
            turns.push(IndexedTurn {
                turn_id: turn_id.clone(),
                status: TurnStatus::Interrupted,
                error: None,
                manually_stopped: false,
                entries: position..entries.len(),
            });
            continue;
        }
        if let SessionEntry::Record {
            record:
                LedgerRecord::OperationFinished {
                    turn_id: id,
                    outcome,
                    error,
                    user_stopped,
                    ..
                },
            ..
        } = entry
            && let Some(last) = turns.last_mut()
            && &last.turn_id == id
        {
            last.status = *outcome;
            last.error = error.clone();
            last.manually_stopped = is_manually_stopped(*outcome, *user_stopped);
        }
    }
    turns
}

const MAX_SESSION_TITLE_CHARS: usize = 8;

/// 从最近一次生成请求读取实际输入用量；压缩成功或切换模型后不沿用旧读数。
pub(crate) fn context_usage(
    entries: &[SessionEntry],
    selected: &singularity_protocol::ModelSelection,
    capacity: Option<u64>,
) -> Option<singularity_protocol::ContextUsage> {
    let capacity = capacity.filter(|capacity| *capacity > 0)?;
    for entry in entries.iter().rev() {
        match entry {
            SessionEntry::Compaction { .. } => return None,
            SessionEntry::Metadata {
                metadata: SessionMetadata::ThreadSettings { selection },
                ..
            } if selection.provider != selected.provider || selection.model != selected.model => return None,
            _ => {}
        }
        let Some(request) = entry.request_observation() else {
            continue;
        };
        if request.purpose == singularity_protocol::RequestPurpose::Compaction {
            continue;
        }
        let Some(usage) = &request.usage else { continue };
        if request.provider != selected.provider || request.model != selected.model {
            return None;
        }
        return Some(singularity_protocol::ContextUsage { used: usage.input_tokens, capacity });
    }
    None
}

/// 默认标题：把第一条用户消息里的连续空白合并成一个空格，取前 MAX_SESSION_TITLE_CHARS
/// 个字符；没有文字时返回 None。
fn default_title(message: &Message) -> Option<String> {
    let mut title = String::new();
    let mut remaining = MAX_SESSION_TITLE_CHARS;
    let words = message.content().iter().enumerate().filter_map(|(index, block)| match block {
        ContentBlock::Text { text, .. } => Some(message.text_for_display(index, text).split_whitespace()),
        ContentBlock::Thinking { .. } | ContentBlock::ToolCall(_) | ContentBlock::Image(_) => None,
    });
    for word in words.flatten() {
        if remaining == 0 {
            break;
        }
        if !title.is_empty() {
            if remaining == 1 {
                break;
            }
            title.push(' ');
            remaining -= 1;
        }
        for character in word.chars().take(remaining) {
            title.push(character);
            remaining -= 1;
        }
    }
    (!title.is_empty()).then_some(title)
}

/// 整个会话的模型用量合计，供工作台显示成本和速度。
/// 每次请求尝试只保存一次结果；重试、后续回合和摘要请求都计入合计。
///
/// 首 Token 耗时只统计实际测到的请求；Token 数只统计上报了用量的请求，
/// 没有用量记录的请求不影响“缓存计数是否完整”。
pub(crate) fn session_usage(entries: &[SessionEntry]) -> SessionModelUsage {
    let mut usage = SessionModelUsage {
        cache_usage_complete: true,
        ..SessionModelUsage::default()
    };
    for entry in entries {
        if let Some(observation) = entry.request_observation() {
            accumulate_request_usage(&mut usage, observation);
        }
    }
    usage
}

/// 历史读取和实时事件共用的用量累计规则；每个请求的结束记录只计一次。
pub fn accumulate_request_usage(
    usage: &mut SessionModelUsage,
    observation: &singularity_protocol::RequestObservation,
) {
    if let Some(ttft_ms) = observation.ttft_ms {
        usage.ttft_ms += ttft_ms;
        usage.ttft_requests += 1;
    }
    let Some(request_usage) = &observation.usage else {
        return;
    };
    usage.input_tokens += request_usage.input_tokens;
    usage.cached_input_tokens += request_usage.cached_input_tokens.unwrap_or(0);
    usage.output_tokens += request_usage.output_tokens;
    usage.total_tokens += request_usage.total_tokens;
    usage.cache_usage_complete &= request_usage.cached_input_tokens.is_some();
    if let Some(ms) = observation.decode_ms
        && ms > 0
    {
        usage.decode_ms += ms;
        usage.decode_tokens += request_usage.output_tokens;
    }
    usage.generation_ms += observation.duration_ms;
    usage.usage_present = true;
}

/// 任务列表里的一项：标题、时间和最近一个回合的状态。
pub(crate) fn summarize_thread(session: &SessionData) -> ThreadSummary {
    let mut title = None;
    // 回合串行写入，最近的开始或结束记录就能说明最后一轮的状态。
    let last_turn = session.entries().iter().rev().find_map(|entry| match entry {
        SessionEntry::Record {
            record: LedgerRecord::OperationStarted, ..
        } => Some((TurnStatus::Interrupted, false)),
        SessionEntry::Record {
            record: LedgerRecord::OperationFinished { outcome, user_stopped, .. },
            ..
        } => Some((*outcome, is_manually_stopped(*outcome, *user_stopped))),
        _ => None,
    });
    // 最近一次命名优先；未命名时使用第一条用户输入。
    for entry in session.entries().iter().rev() {
        if let SessionEntry::Metadata {
            metadata: SessionMetadata::ThreadName { name },
            ..
        } = entry
        {
            title = Some(name.clone());
            break;
        }
    }
    let title = title.or_else(|| {
        session.entries().iter().find_map(|entry| {
            let SessionEntry::Message { message, .. } = entry else {
                return None;
            };
            if !matches!(message, Message::User { .. }) {
                return None;
            }
            default_title(message)
        })
    });
    let created_at = session.created_at().to_string();
    let updated_at = session
        .entries()
        .iter()
        .rev()
        .find(|entry| !matches!(entry, SessionEntry::Metadata { .. }))
        .map(|entry| entry.timestamp().to_owned())
        .unwrap_or_else(|| created_at.clone());
    ThreadSummary {
        thread_id: session.session_id().to_string(),
        cwd: session.cwd_string(),
        created_at,
        updated_at,
        title,
        status: last_turn.map(|(status, _)| status),
        manually_stopped: last_turn.is_some_and(|(_, manually_stopped)| manually_stopped),
    }
}
