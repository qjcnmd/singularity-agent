//! JSONL 会话条目 → 公开历史投影。
//!
//! IndexedTurn::project 只复制用户可见的 message/thinking/tool/settings/
//! compaction 字段，绝不序列化原始 entry 或其
//! provider_reasoning_replay。index_turn_history 按 run operation 起点建立条目范围，
//! 并归约每个回合的终态与手动停止事实；summarize_thread 从同一索引派生目录摘要，
//! ThreadSnapshot 仅投影请求页内的轮次，并按内容引用还原请求详情。

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
/// operation_finished 写入轮次状态而不是条目，message/compaction/settings
/// 投影为轮内条目。首个开始标记之前存在落盘条目时，它们构成一个
/// 无归属 turn 的前导组（turnId/status 为 null）；没有任何条目时不产生空组。
///
/// 崩溃遗留的未终止轮按 interrupted 投影；只有调用方确认本进程持有该
/// Thread 的活动写者时，末组才投影为 running。
pub(crate) struct IndexedTurn {
    pub turn_id: Option<String>,
    pub status: Option<TurnStatus>,
    /// 本轮终态记录里的持久失败细节；非失败轮与旧日志为 None。
    pub error: Option<singularity_protocol::TurnErrorDetail>,
    /// 本轮以 interrupted 结束且由用户停止触发；终态记录之外的回合为 false。
    pub manually_stopped: bool,
    pub entries: std::ops::Range<usize>,
}

impl IndexedTurn {
    /// 本轮的公开分页 cursor：`turn:{turnId}`，无归属的前导组为
    /// `turn:leading`。thread/read 的 before_turn 与返回的 next_cursor 都用
    /// 这个值，而不是某个 item id。
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
                        // 结果按调用身份关联到调用条目；只有确实找不到配对
                        // ToolCall 的孤立记录才退回使用自己的调用 ID 作为展示
                        // 身份，不把缺失身份伪装成条目身份。
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
                    // 开始事实只由开始观测建立，后续终态观测只更新观测载荷。
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
                SessionEntry::Record { .. } => {}
            }
        }
        ThreadTurn {
            turn_id: self.turn_id.clone(),
            status: self.status,
            error: self.error.clone(),
            items,
        }
    }
}

/// 最近一次独立压缩的终态反馈：它是当前的操作反馈提示，也是冷读公开历史恢复
/// 这份反馈的唯一来源。
///
/// 只看账本里**最后一条** operation：新的 Run 或压缩一开始，上一条终态就不再
/// 代表当前反馈（与热读在同一处清除提示的规则一致）。只有最后一条 operation
/// 是独立压缩（无 turn 绑定）时才给出终态：
///
/// - 失败/中断给出各自的终态与原因；
/// - 完成且没有落盘任何压缩条目，说明这次压缩没有可替换的内容（手动压缩的
///   `NotNeeded`），给出无消息的完成终态，让界面照常显示“没有可压缩的内容”；
/// - 完成并落盘了压缩条目时不给终态：摘要正文本身就是那条反馈。
///
/// 前一个 Run 的完成状态留在它自己的轮次里，不被这次压缩改写。
pub(crate) fn compaction_terminal(
    entries: &[SessionEntry],
) -> Option<singularity_protocol::SessionTerminalSnapshot> {
    let mut terminal: Option<crate::CompactionOutcome> = None;
    let mut reduced = false;
    // 只有最后一次操作影响反馈，从尾部读到它的起点即可。
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

/// 只索引轮次的条目范围、终态、失败细节与手动停止事实；公开正文和请求详情
/// 在请求分页时才构建。
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

/// 列表摘要标题的长度上限。
const MAX_SESSION_TITLE_CHARS: usize = 8;

/// 默认标题：把用户消息正文的空白序列压缩为单个空格后截取前
/// MAX_SESSION_TITLE_CHARS 个字符。逐块借用正文，只构造实际标题，
/// 不物化整段文本；无内容时为 None。
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

/// 整份账本的累计模型用量，供工作台展示成本与速度。
///
/// requestId 标识一次具体的 provider 请求，每次 attempt 都会生成一个新的：
/// 按它归并折叠的是同一请求自己的 started 与终态两行观测（取末次），而不是把
/// 重试合成最后一次。重试、后续轮次与摘要请求各有自己的 requestId，全部计入
/// 合计。`IndexedTurn::project` 用同一身份规则把同一请求的多行折叠成一条历史，
/// 因此会话合计等于工作台逐请求展示的数字之和。
///
/// 与 turn 级 usage 的差异在范围与字段，不是两套重试口径：turn 的
/// RequestAccounting 只累计本轮请求（含本轮重试）并携带总数与思考 Token，
/// 本视图跨轮次累计输入、输出与耗时。
/// 未报告 usage 的请求只把 usage_complete 置为 false，不计入任何计数。
fn session_usage(entries: &[SessionEntry]) -> SessionModelUsage {
    // 同一 requestId 的后续观测覆盖先前观测：只认末次，避免同一请求的
    // started 行与终态行被算两次。
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
        ..SessionModelUsage::default()
    };
    for observation in latest.values() {
        if observation.input_tokens.is_none() && observation.output_tokens.is_none() {
            usage.usage_complete = false;
            continue;
        }
        usage.input_tokens += observation.input_tokens.unwrap_or(0);
        usage.cached_input_tokens += observation.cached_input_tokens.unwrap_or(0);
        usage.output_tokens += observation.output_tokens.unwrap_or(0);
        usage.generation_ms += observation.duration_ms;
        usage.usage_present = true;
    }
    usage
}

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
