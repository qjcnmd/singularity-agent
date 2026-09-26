//! 把 JSONL 会话条目投影成公开历史。
//!
//! IndexedTurn::project 只复制用户能看到的 message/thinking/tool/settings/
//! compaction 字段，绝不序列化原始 entry 或它的
//! provider_reasoning_replay。index_turn_history 按 run operation 的起点划出每轮的
//! 条目范围，并归约出每个回合的终态和手动停止事实；summarize_thread 从同一份索引
//! 派生目录摘要；ThreadSnapshot 只投影请求页内的轮次，并按内容引用还原请求详情。

use std::collections::HashMap;

use singularity_agent::{
    message::{AgentMessage, ContentBlock, ItemScope},
    session::{
        LedgerRecord, OperationKind, SessionData, SessionEntry, SessionError, SessionMetadata,
    },
};
use singularity_protocol::{
    HistoryItem, RequestObservation, SessionModelUsage, ThreadSummary, ThreadTurn, TurnStatus,
};

/// thread/read 的按轮分组投影。
///
/// run operation 的 operation_started 划定轮次边界；同 turn id 的
/// operation_finished 写进轮次状态而不是条目，message/compaction/settings 则投影成
/// 轮内条目。第一个开始标记之前如果有已落盘的条目，它们构成一个不属于任何 turn 的
/// 前导组（turnId/status 为 null）；一条条目都没有时不产生空组。
///
/// 崩溃遗留、没有终态的轮按 interrupted 投影；只有调用方确认本进程持有该 Thread 的
/// 活动写者时，最后一组才投影为 running。
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
                        // 结果按调用身份关联到对应的调用条目；只有确实找不到配对
                        // ToolCall 的孤立记录，才退回用它自己的调用 ID 当展示身份，
                        // 不把「找不到身份」伪装成条目身份。
                        let item_id = tool_items
                            .get(tool_call_id)
                            .cloned()
                            .unwrap_or_else(|| tool_call_id.clone());
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
                    let request_id = observation.request_id.clone();
                    // 开始时刻只由开始观测建立，后续的终态观测只更新观测载荷。
                    let mut started_at = (observation.status
                        == singularity_protocol::ProviderAttemptStatus::Started)
                        .then(|| timestamp.clone());
                    if let Some(context) = context {
                        match session.request_head(context) {
                            Ok(head) => observation.request_head = Some(head),
                            Err(error) => {
                                observation.request_error = Some(error.to_string().into_boxed_str())
                            }
                        }
                    // 终态观测不再内嵌请求详情：沿用先前观测已解析的部分。
                    } else if let Some(&position) = request_positions.get(&request_id) {
                        if let HistoryItem::Request {
                            observation: previous,
                            started_at: previous_started_at,
                        } = &mut items[position]
                        {
                            observation.request_head = previous.request_head.take();
                            observation.request_error = previous.request_error.take();
                            started_at = previous_started_at.take();
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
                        started_at,
                        observation,
                    };
                    if let Some(&position) = request_positions.get(&request_id) {
                        items[position] = request;
                    } else {
                        request_positions.insert(request_id, items.len());
                        items.push(request);
                    }
                }
                SessionEntry::Record {
                    record: LedgerRecord::AssistantInterrupted { items: interrupted },
                    ..
                } => {
                    items.extend(interrupted.iter().cloned());
                }
                SessionEntry::Record {
                    timestamp,
                    record:
                        LedgerRecord::OperationStarted {
                            kind: OperationKind::Run,
                            turn_id,
                            ..
                        },
                    ..
                } if turn_id == &self.turn_id => started_at = Some(timestamp.clone()),
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

/// 最近一次独立压缩的终态反馈：既是当前操作要显示的提示，也是冷读公开历史时恢复这份反馈的
/// 唯一来源。只看账本里**最后一条** operation：新的 Run 或压缩一开始，上一条终态就不再代表
/// 当前反馈（和热读清除提示的规则一致），只有它是独立压缩（没有绑定 turn）时才给出终态——
/// 失败或中断给出各自的终态和原因；完成但没有落盘压缩条目说明这次压缩没有可替换的内容
/// （手动压缩的 `NotNeeded`），给出不带消息的完成终态；完成并落盘了压缩条目时不给终态，
/// 摘要正文本身就是那条反馈。前一个 Run 的完成状态留在它自己的轮次里，不会被这次压缩改写。
pub(crate) fn compaction_terminal(
    entries: &[SessionEntry],
) -> Option<singularity_protocol::SessionTerminalSnapshot> {
    let mut terminal: Option<crate::CompactionOutcome> = None;
    let mut reduced = false;
    // 只有最后一次操作影响反馈，所以从尾部往前读到它的起点就够。
    for entry in entries.iter().rev() {
        match entry {
            SessionEntry::Record {
                record: LedgerRecord::OperationStarted { turn_id, .. },
                ..
            } => {
                return terminal
                    .filter(|_| turn_id.is_none())
                    .and_then(|mut outcome| {
                        outcome.reduced = reduced;
                        outcome.terminal()
                    });
            }
            SessionEntry::Record {
                record: LedgerRecord::OperationFinished { outcome, error, .. },
                ..
            } => {
                terminal = Some(crate::CompactionOutcome {
                    status: *outcome,
                    reduced: false,
                    error: error.clone(),
                });
            }
            SessionEntry::Compaction { .. } => reduced = true,
            _ => {}
        }
    }
    None
}

/// 这里只索引轮次的条目范围、终态、失败细节和手动停止事实；公开正文和请求详情
/// 等到请求分页时才构建。
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

/// 整份账本累计的模型用量，供工作台展示成本和速度。requestId 标识一次具体的 provider 请求，
/// 每次 attempt 都会生成一个新的：按它归并折叠的是同一个请求自己的 started 与终态两行观测
/// （取末次），而不是把重试合并成最后一次；重试、后续轮次和摘要请求各有自己的 requestId，
/// 全部计入合计。`IndexedTurn::project` 用同一套身份规则折叠同一个请求的多行，因此会话合计
/// 等于工作台逐请求展示的数字之和。
///
/// 与 turn 级 usage 的差异在范围和字段，不是两套重试口径：turn 的 RequestAccounting 只累计
/// 本轮请求（含本轮的重试），并且带总数和思考 token；本视图跨轮次累计输入、输出和耗时。
/// 没报告 usage 的请求只把 usage_complete 置为 false，不计入任何计数。
fn session_usage(entries: &[SessionEntry]) -> SessionModelUsage {
    let mut latest: HashMap<&str, &RequestObservation> = HashMap::new();
    for entry in entries {
        let SessionEntry::Record {
            record: LedgerRecord::ModelRequest { observation, .. },
            ..
        } = entry
        else {
            continue;
        };
        latest.insert(observation.request_id.as_str(), observation);
    }
    let mut usage = SessionModelUsage {
        usage_complete: true,
        cache_usage_complete: true,
        ..SessionModelUsage::default()
    };
    for observation in latest.values() {
        if observation.input_tokens.is_none() && observation.output_tokens.is_none() {
            usage.usage_complete = false;
            usage.cache_usage_complete = false;
            continue;
        }
        usage.input_tokens += observation.input_tokens.unwrap_or(0);
        usage.cached_input_tokens += observation.cached_input_tokens.unwrap_or(0);
        usage.output_tokens += observation.output_tokens.unwrap_or(0);
        usage.total_tokens += observation.total_tokens.unwrap_or_else(|| {
            observation
                .input_tokens
                .unwrap_or(0)
                .saturating_add(observation.output_tokens.unwrap_or(0))
        });
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

/// 从回合索引与元数据/消息条目派生目录摘要；不修复会话，也不写入会话。
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
    // 反向遍历取最近一次的设置和名称；没有名字时回落到第一条用户输入。
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
                reasoning.as_deref(),
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
            default_title(message.content())
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
        usage: session_usage(session.entries()),
    }
}
