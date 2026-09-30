//! 把 JSONL 会话条目投影成公开历史。
//!
//! IndexedTurn::project 只复制用户能看到的 message/thinking/tool/settings/
//! compaction 字段，绝不序列化原始 entry 或它的
//! provider_reasoning_replay。index_turn_history 按 run operation 的起点划出每轮的
//! 条目范围，并归约出每个回合的终态和手动停止事实；summarize_thread 从同一份索引
//! 派生目录摘要；ThreadSnapshot 只投影请求页内的轮次，并按内容引用还原请求详情。

use singularity_agent::{
    message::{AgentMessage, ContentBlock, ItemScope},
    session::{LedgerRecord, SessionData, SessionEntry, SessionMetadata},
};
use singularity_protocol::{HistoryItem, SessionModelUsage, ThreadSummary, ThreadTurn, TurnStatus};

/// thread/read 的按轮分组投影。
///
/// run operation 的 operation_started 划定轮次边界；同 turn id 的
/// operation_finished 写进轮次状态而不是条目，message/compaction/settings 则投影成
/// 轮内条目。第一个开始标记之前如果有已落盘的条目，它们构成一个不属于任何 turn 的
/// 前导组（turnId/status 为 null）；一条条目都没有时不产生空组。
///
/// 没有终态的持久记录按 interrupted 投影；工作台的活动状态由执行器提供。
pub(crate) struct IndexedTurn {
    pub turn_id: Option<String>,
    pub status: Option<TurnStatus>,
    /// 本轮终态记录里落盘的失败细节；非失败轮和旧日志是 None。
    pub error: Option<singularity_protocol::TurnErrorDetail>,
    /// 本轮以 interrupted 结束，而且是由用户停止触发的；没有终态记录的回合为 false。
    pub manually_stopped: bool,
    pub entries: std::ops::Range<usize>,
}

impl IndexedTurn {
    /// 本轮公开分页用的 cursor：`turn:{turnId}`，不属于任何 turn 的前导组是 `turn:leading`。
    /// thread/read 的 before_turn 和返回的 next_cursor 都用这个值，而不是某个 item id。
    pub fn cursor(&self) -> String {
        self.turn_id
            .as_ref()
            .map_or_else(|| "turn:leading".into(), |id| format!("turn:{id}"))
    }

    /// 按轮遍历持久条目，直接写出最终的公开 items；工具 wire ID 的映射、同一个
    /// request 多次观测的归并都在这里完成。请求详情在这一轮条目合并完之后才展开
    /// 一次，避免先生成临时身份再回头改写。
    pub fn project(&self, session: &SessionData) -> ThreadTurn {
        let mut items = Vec::new();
        let mut started_at = None;
        let mut finished_at = None;
        let mut compacted = false;
        let mut request_positions = std::collections::HashMap::new();
        let mut tool_items = std::collections::HashMap::new();
        for entry in &session.entries()[self.entries.clone()] {
            match entry {
                SessionEntry::Message { message, id, .. } => match message {
                    AgentMessage::User { .. } | AgentMessage::Assistant { .. } => {
                        for (ordinal, call) in message.tool_calls().enumerate() {
                            tool_items.insert(
                                call.tool_call_id.clone(),
                                singularity_agent::session::tool_item_id(id, ordinal),
                            );
                        }
                        items.extend(message.public_items(id, ItemScope::History));
                    }
                    AgentMessage::ToolResult {
                        tool_call_id,
                        is_error,
                        duration_ms,
                        diff,
                        read_source,
                        ..
                    } => {
                        // 每个工具结果引用此前已落盘的调用；按调用位置取得展示身份。
                        let item_id = tool_items[tool_call_id].clone();
                        items.push(HistoryItem::ToolResult {
                            id: item_id,
                            output: message.content_text(),
                            is_error: *is_error,
                            duration_ms: *duration_ms,
                            diff: diff.clone(),
                            read_source: *read_source,
                        });
                    }
                },
                SessionEntry::Compaction { compaction, id, .. } => {
                    compacted = true;
                    items.push(HistoryItem::Compaction {
                        id: id.clone(),
                        summary: compaction.summary.clone(),
                    })
                }
                SessionEntry::Metadata { metadata, id, .. } => match metadata {
                    // thread 名称不作为公开历史条目。
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
                    timestamp,
                    record:
                        LedgerRecord::ModelRequest {
                            observation,
                            context,
                        },
                    ..
                } => {
                    let mut observation = observation.clone();
                    if let Some(context) = context {
                        observation.request_head = Some(session.request_head(context));
                        request_positions.insert(observation.request_id.clone(), items.len());
                        items.push(HistoryItem::Request {
                            started_at: Some(timestamp.clone()),
                            observation,
                        });
                    } else {
                        // 每次请求只有一个终态观测；开始时刻和请求详情保留在原条目。
                        let position = request_positions[&observation.request_id];
                        let HistoryItem::Request {
                            observation: previous,
                            ..
                        } = &mut items[position]
                        else {
                            unreachable!()
                        };
                        observation.request_head = previous.request_head.take();
                        *previous = observation;
                    }
                }
                SessionEntry::Record {
                    record: LedgerRecord::AssistantInterrupted { items: interrupted },
                    ..
                } => {
                    items.extend(interrupted.iter().cloned());
                }
                SessionEntry::Record {
                    record: LedgerRecord::OperationStarted { turn_id: None },
                    ..
                } => compacted = false,
                SessionEntry::Record {
                    id,
                    record:
                        LedgerRecord::OperationFinished {
                            turn_id: None,
                            outcome,
                            error,
                            ..
                        },
                    ..
                } if !compacted || *outcome != TurnStatus::Completed => {
                    items.push(HistoryItem::CompactionResult {
                        id: id.clone(),
                        status: *outcome,
                        message: error.as_ref().map(|error| error.message.clone()),
                    });
                }
                SessionEntry::Record {
                    timestamp,
                    record: LedgerRecord::OperationStarted { turn_id, .. },
                    ..
                } if turn_id.is_some() && turn_id == &self.turn_id => {
                    started_at = Some(timestamp.clone())
                }
                SessionEntry::Record {
                    timestamp,
                    record:
                        LedgerRecord::OperationFinished {
                            turn_id: Some(id), ..
                        },
                    ..
                } if Some(id) == self.turn_id.as_ref() => finished_at = Some(timestamp.clone()),
                SessionEntry::Record { .. } => {}
            }
        }
        ThreadTurn {
            started_at,
            finished_at,
            turn_id: self.turn_id.clone(),
            status: self.status,
            error: self.error.clone(),
            items,
        }
    }
}

/// 这里只索引轮次的条目范围、终态、失败细节和手动停止事实；公开正文和请求详情
/// 等到请求分页时才构建。
pub(crate) fn index_turn_history(entries: &[SessionEntry]) -> Vec<IndexedTurn> {
    let mut turns: Vec<IndexedTurn> = Vec::new();
    for (position, entry) in entries.iter().enumerate() {
        if let SessionEntry::Record {
            record: LedgerRecord::OperationStarted { turn_id, .. },
            ..
        } = entry
            && turn_id.is_some()
        {
            if let Some(last) = turns.last_mut() {
                last.entries.end = position;
            }
            turns.push(IndexedTurn {
                turn_id: turn_id.clone(),
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
                    turn_id: Some(id),
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
/// 个字符；逐块借用正文，不把整段文本复制出来，没有内容时是 None。
fn default_title(content: &[ContentBlock]) -> Option<String> {
    let mut title = String::new();
    let mut remaining = MAX_SESSION_TITLE_CHARS;
    let words = content.iter().filter_map(|block| match block {
        ContentBlock::Text { text } => Some(text.split_whitespace()),
        ContentBlock::Thinking { .. } | ContentBlock::ToolCall(_) => None,
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

/// 整份账本累计的模型用量，供工作台展示成本和速度。每次 attempt 只有一条终态观测
/// 可以带用量，开始观测没有用量；重试、后续轮次和摘要请求各自计入合计。
///
/// 与 turn 级 usage 的差异在范围和字段，不是两套重试口径：turn 的 RequestAccounting 只累计
/// 本轮请求（含本轮的重试），并且带总数和思考 token；本视图跨轮次累计输入、输出和耗时。
/// 只有上报了 usage 的请求参与合计：进行中、失败或取消的请求没有消费记录，既不进入计数
/// 也不影响完整性，合计因此是「已上报用量的合计」。
fn session_usage(entries: &[SessionEntry]) -> SessionModelUsage {
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
        if observation.input_tokens.is_none() && observation.output_tokens.is_none() {
            continue;
        }
        usage.input_tokens += observation.input_tokens.unwrap_or(0);
        usage.cached_input_tokens += observation.cached_input_tokens.unwrap_or(0);
        usage.output_tokens += observation.output_tokens.unwrap_or(0);
        usage.total_tokens += observation
            .total_tokens
            .expect("reported usage has a total");
        usage.cache_usage_complete &= observation.cached_input_tokens.is_some();
        if let (Some(ms), Some(tokens)) = (observation.decode_ms, observation.output_tokens)
            && ms > 0
        {
            usage.decode_ms += ms;
            usage.decode_tokens += tokens;
        }
        usage.generation_ms += observation.duration_ms;
        usage.usage_present = true;
    }
    usage
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
            default_title(message.content())
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
