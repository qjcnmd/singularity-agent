//! 把 JSONL 会话条目投影成公开历史。IndexedTurn::project 只投影用户能看到的
//! message/thinking/tool/settings/compaction 字段，不序列化原始 entry 或它的
//! provider_reasoning_replay；索引、摘要和分页共用同一份回合事实，分页只展开请求页内的轮次，
//! 注入内容直接展示，请求记录用于计量。

use singularity_agent::{
    message::{AgentMessage, ContentBlock, ItemScope},
    session::{LedgerRecord, SessionData, SessionEntry, SessionError, SessionMetadata},
};
use singularity_protocol::{HistoryItem, SessionModelUsage, ThreadSummary, ThreadTurn, TurnStatus};

/// 会话历史的按轮分组索引。
pub(crate) struct IndexedTurn {
    pub turn_id: Option<String>,
    pub status: Option<TurnStatus>,
    /// 回合失败终态中的详细错误信息
    pub error: Option<singularity_protocol::TurnErrorDetail>,
    /// 是否由用户主动中断
    pub manually_stopped: bool,
    pub entries: std::ops::Range<usize>,
}

impl IndexedTurn {
    /// 回合分页游标字符串：`turn:{turnId}`，前导组为 `turn:leading`。
    pub fn cursor(&self) -> String {
        self.turn_id.as_ref().map_or_else(|| "turn:leading".into(), |id| format!("turn:{id}"))
    }

    /// 将持久化条目投影为公开的 `ThreadTurn` 历史视图对象。
    pub fn project(&self, session: &SessionData) -> Result<ThreadTurn, SessionError> {
        let mut items = Vec::new();
        let mut started_at = None;
        let mut finished_at = None;
        let mut tool_items = std::collections::HashMap::new();
        for entry in &session.entries()[self.entries.clone()] {
            match entry {
                SessionEntry::Message { message, id, .. } => match message {
                    AgentMessage::Notification { .. } => {}
                    AgentMessage::User { .. } | AgentMessage::Assistant { .. } => {
                        let mut calls = message.tool_calls();
                        for item in message.public_items(id, ItemScope::History) {
                            if matches!(item, HistoryItem::Tool { .. }) {
                                let call = calls.next().expect("public tools follow message order");
                                tool_items.insert(call.tool_call_id.clone(), items.len());
                            }
                            items.push(item);
                        }
                    }
                    AgentMessage::ToolResult {
                        tool_call_id,
                        is_error,
                        duration_ms,
                        read_source,
                        ..
                    } => {
                        // 结果写回调用所在位置，历史消费者直接读取完整工具条目。
                        let position = *tool_items.get(tool_call_id).ok_or_else(|| {
                            SessionError::InvalidSession(format!(
                                "tool result {id} refers to missing tool call {tool_call_id}"
                            ))
                        })?;
                        let HistoryItem::Tool { result, .. } = &mut items[position] else {
                            unreachable!("tool positions contain calls");
                        };
                        *result = Some(singularity_protocol::ToolResult {
                            output: message.content_text(),
                            images: message.images().cloned().collect(),
                            is_error: *is_error,
                            duration_ms: *duration_ms,
                            read_source: *read_source,
                        });
                    }
                },
                SessionEntry::Compaction { compaction, id, .. } => {
                    items.push(HistoryItem::Compaction {
                        id: id.clone(),
                        summary: compaction.summary.clone(),
                    });
                }
                SessionEntry::Metadata { metadata, id, .. } => {
                    if let Some(item) = metadata_item(id, metadata) {
                        items.push(item);
                    }
                }
                SessionEntry::Record {
                    record: LedgerRecord::ModelRequest { observation },
                    ..
                } => {
                    items.push(HistoryItem::Request { observation: observation.clone() });
                }
                SessionEntry::Record {
                    id,
                    record: LedgerRecord::AssistantInterrupted { message },
                    ..
                } => {
                    items.extend(message.public_items(id, ItemScope::Completion));
                }
                SessionEntry::Record {
                    id,
                    record: LedgerRecord::ContextInjected { name, content },
                    ..
                } => items.push(HistoryItem::Context {
                    id: id.clone(),
                    name: name.clone(),
                    content: content.clone(),
                }),
                SessionEntry::Record {
                    timestamp,
                    record: LedgerRecord::OperationStarted { turn_id, .. },
                    ..
                } if Some(turn_id) == self.turn_id.as_ref() => started_at = Some(timestamp.clone()),
                SessionEntry::Record {
                    timestamp,
                    record: LedgerRecord::OperationFinished { turn_id: id, .. },
                    ..
                } if Some(id) == self.turn_id.as_ref() => finished_at = Some(timestamp.clone()),
                SessionEntry::Record { .. } => {}
            }
        }
        Ok(ThreadTurn {
            started_at,
            finished_at,
            turn_id: self.turn_id.clone(),
            status: self.status,
            manually_stopped: self.manually_stopped,
            error: self.error.clone(),
            items,
        })
    }
}

/// 只索引轮次的条目范围、终态、失败细节和手动停止事实；公开正文和请求详情等分页时才构建。
pub(crate) fn index_turn_history(entries: &[SessionEntry]) -> Vec<IndexedTurn> {
    let mut turns: Vec<IndexedTurn> = Vec::new();
    for (position, entry) in entries.iter().enumerate() {
        if let SessionEntry::Record {
            record: LedgerRecord::OperationStarted { turn_id, .. },
            ..
        } = entry
        {
            if let Some(last) = turns.last_mut() {
                last.entries.end = position;
            }
            turns.push(IndexedTurn {
                turn_id: Some(turn_id.clone()),
                status: None,
                error: None,
                manually_stopped: false,
                entries: position..entries.len(),
            });
            continue;
        }
        if turns.is_empty() {
            turns.push(IndexedTurn {
                turn_id: None,
                status: None,
                error: None,
                manually_stopped: false,
                entries: position..entries.len(),
            });
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
            && last.status.is_none()
            && last.turn_id.as_ref() == Some(id)
        {
            last.status = Some(*outcome);
            last.error = error.clone();
            last.manually_stopped = *outcome == TurnStatus::Interrupted && *user_stopped;
        }
    }
    for turn in &mut turns {
        if turn.turn_id.is_some() && turn.status.is_none() {
            turn.status = Some(TurnStatus::Interrupted);
        }
    }
    turns
}

const MAX_SESSION_TITLE_CHARS: usize = 8;

/// 默认标题：把用户消息正文里的连续空白压成一个空格，再截取前 MAX_SESSION_TITLE_CHARS
/// 个字符；逐块借用正文，不复制整段文本，没有内容时是 None。
fn default_title(message: &AgentMessage) -> Option<String> {
    let mut title = String::new();
    let mut remaining = MAX_SESSION_TITLE_CHARS;
    let words = message.content().iter().enumerate().filter_map(|(index, block)| match block {
        ContentBlock::Text { text } => Some(message.text_for_display(index, text).split_whitespace()),
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

/// 整份账本累计的模型用量，供工作台展示成本和速度。每次 attempt 只有一条终态观测能带
/// 用量，开始观测没有用量；重试、后续轮次和摘要请求各自计入合计。
///
/// 跨轮次累计输入、输出和耗时。TTFT
/// 只计实测样本；消费只计上报了 usage 的请求，缺消费记录的请求不影响缓存完整性。
pub(crate) fn session_usage(entries: &[SessionEntry]) -> SessionModelUsage {
    let mut usage = SessionModelUsage {
        cache_usage_complete: true,
        ..SessionModelUsage::default()
    };
    for entry in entries {
        let SessionEntry::Record {
            record: LedgerRecord::ModelRequest { observation, .. },
            ..
        } = entry
        else {
            continue;
        };
        accumulate_request_usage(&mut usage, observation);
    }
    usage
}

/// 历史读取和实时事件共用的用量累计规则。每个请求的终态观测只计一次。
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

/// 从回合索引与元数据/消息条目派生只读目录摘要。
pub(crate) fn summarize_thread(session: &SessionData, turns: &[IndexedTurn]) -> ThreadSummary {
    let mut title = None;
    let mut turn_count = 0usize;
    let mut status = None;
    let mut manually_stopped = false;
    for turn in turns.iter().filter(|turn| turn.turn_id.is_some()) {
        turn_count += 1;
        status = turn.status;
        manually_stopped = turn.manually_stopped;
    }
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
            if !matches!(message, AgentMessage::User { .. }) {
                return None;
            }
            default_title(message)
        })
    });
    let created_at = session.created_at().to_string();
    let updated_at = session
        .entries()
        .last()
        .map(|entry| entry.timestamp().to_owned())
        .unwrap_or_else(|| created_at.clone());
    ThreadSummary {
        thread_id: session.session_id().to_string(),
        cwd: session.cwd_string(),
        created_at,
        updated_at,
        title,
        status,
        manually_stopped,
        turn_count,
        usage: session_usage(session.entries()),
    }
}

/// 设置的历史条目与实时通知共用同一份字段映射。
pub(crate) fn metadata_item(id: &str, metadata: &SessionMetadata) -> Option<HistoryItem> {
    match metadata {
        SessionMetadata::ThreadName { .. } => None,
        SessionMetadata::ThreadSettings { provider, model, reasoning } => Some(HistoryItem::Settings {
            id: id.to_owned(),
            provider: provider.clone(),
            model: model.clone(),
            reasoning: reasoning.clone(),
        }),
    }
}
