//! 上下文视图：从会话 ledger 派生出模型请求的输入，并统一计量。
//!
//! 只从持久账本派生对话历史；当前文件指令由 Agent 在请求装配时加入。
//! 请求装配、压缩判定与溢出恢复共用这一个视图。原始会话始终由会话 ledger 持有。

mod resolution;

use self::resolution::{absorb_tool_pairing, push_context_entry, resolve_context_entries};

use singularity_model::{ModelMessage, ModelRole, ModelUsage};

use crate::message::{
    AgentMessage, COMPACTION_SUMMARY_PREFIX, COMPACTION_SUMMARY_SUFFIX, ContentBlock,
};

use super::format::{LedgerRecord, Result, SessionEntry};
use super::manager::SessionData;

/// 按 UTF-16 字符数做启发式 Token 估算（ceil(chars / 4)），全仓只此一份实现。
pub(crate) fn estimate_tokens_of(text: &str) -> u64 {
    let chars = text.encode_utf16().count() as u64;
    chars.div_ceil(4)
}

fn compaction_summary(summary: &str) -> String {
    format!("{COMPACTION_SUMMARY_PREFIX}{summary}{COMPACTION_SUMMARY_SUFFIX}")
}

enum ContextEntry<'a> {
    Message(&'a AgentMessage),
    Summary(&'a str),
    SkillInstructions(&'a str),
}

/// 把 ledger 条目穷尽分类；计量、保留边界和请求投影共用这一处可见性判断。
fn context_entry(entry: &SessionEntry) -> Option<ContextEntry<'_>> {
    match entry {
        SessionEntry::Message { message, .. } => Some(ContextEntry::Message(message)),
        SessionEntry::Compaction { compaction, .. } => {
            Some(ContextEntry::Summary(&compaction.summary))
        }
        SessionEntry::Metadata { .. } => None,
        SessionEntry::Record { record, .. } => match record {
            LedgerRecord::SkillInstructions { text } => Some(ContextEntry::SkillInstructions(text)),
            LedgerRecord::ToolResultPruned { .. }
            | LedgerRecord::AssistantInterrupted { .. }
            | LedgerRecord::ModelRequest { .. }
            | LedgerRecord::RequestDefinitions { .. }
            | LedgerRecord::OperationStarted { .. }
            | LedgerRecord::OperationFinished { .. } => None,
        },
    }
}

/// 估算可压缩历史的模型内容；当前文件指令由 Agent 另行计量。
pub(crate) fn entry_token_estimate(entry: &SessionEntry) -> u64 {
    match context_entry(entry) {
        Some(ContextEntry::Message(message)) => message_token_estimate(message),
        Some(ContextEntry::Summary(summary)) => {
            estimate_tokens_of(&compaction_summary(summary)) + 8
        }
        Some(ContextEntry::SkillInstructions(text)) => estimate_tokens_of(text) + 8,
        None => 0,
    }
}

pub(crate) fn message_token_estimate(message: &crate::message::AgentMessage) -> u64 {
    content_token_estimate(
        message.content(),
        matches!(message, AgentMessage::ToolResult { .. }),
    )
}

/// 只估算模型请求真正会带上的内容：公开思考不进入文本投影
/// （见 `ContextPosition::model_message`），所以计零。
fn content_token_estimate(content: &[ContentBlock], tool_result: bool) -> u64 {
    let mut tokens = 4u64;
    for block in content {
        tokens = tokens.saturating_add(match block {
            ContentBlock::Text { text } => estimate_tokens_of(text) + 4,
            ContentBlock::Thinking { .. } => 0,
            ContentBlock::ToolCall(call) => {
                estimate_tokens_of(&call.tool_name)
                    + estimate_tokens_of(&call.arguments.to_string())
                    + 4
            }
        });
    }
    if tool_result {
        tokens += 4;
    }
    tokens
}

#[derive(Debug, Clone, Default)]
pub struct ContextView {
    entries: Vec<ContextPosition>,
    /// 各条目估算值之和；usage 基线缺失时就用它兜底计量。
    estimated_tokens: u64,
    /// 最近一次同形状请求的实测总量相对本次估价的差量；结构替换后由
    /// [`Self::rebuild`] 作废。
    usage_correction: u64,
}

/// 已按工具配对边界选好的摘要前缀。
pub(crate) struct CompactionPrefix {
    pub(crate) messages: Vec<ModelMessage>,
    pub(crate) previous_summary: Option<String>,
    pub(crate) first_kept_entry_id: String,
}

impl ContextView {
    /// 归约出模型的有效历史。压缩锚点或剪枝引用失效会在这里报错；唯一的调用方是
    /// 构建执行上下文的 `Agent::new()`（含独立压缩）。
    pub fn derive(session: &SessionData) -> Result<Self> {
        let entries = resolve_context_entries(session)?;
        let estimated_tokens = entries
            .iter()
            .map(|position| position.token_estimate(session))
            .sum();
        Ok(Self {
            entries,
            estimated_tokens,
            usage_correction: 0,
        })
    }

    pub(crate) fn messages(&self, session: &SessionData) -> Vec<ModelMessage> {
        self.entries
            .iter()
            .filter_map(|position| position.model_message(session))
            .collect()
    }

    pub(crate) fn compaction_prefix(
        &self,
        session: &SessionData,
        keep_recent_tokens: u64,
    ) -> Option<CompactionPrefix> {
        let entries = &self.entries;
        let cut = find_cut_point(entries, session, keep_recent_tokens);
        if cut == 0 {
            return None;
        }
        let previous_summary = match entries[0].entry(session) {
            SessionEntry::Compaction { compaction, .. } => Some(compaction.summary.clone()),
            _ => None,
        };
        let messages: Vec<_> = entries[usize::from(previous_summary.is_some())..cut]
            .iter()
            .filter_map(|position| position.model_message(session))
            .collect();
        if messages.is_empty() {
            return None;
        }
        Some(CompactionPrefix {
            messages,
            previous_summary,
            first_kept_entry_id: entries[cut].entry(session).id().to_string(),
        })
    }

    pub(crate) fn pruned_tool_results(&self, session: &SessionData) -> Vec<LedgerRecord> {
        self.entries
            .iter()
            .filter_map(|position| match position.entry(session) {
                SessionEntry::Message {
                    id,
                    message: AgentMessage::ToolResult { .. },
                    ..
                } => crate::compaction::prune_tool_content(position.content(session)).map(
                    |content| LedgerRecord::ToolResultPruned {
                        entry_id: id.clone(),
                        content,
                    },
                ),
                _ => None,
            })
            .collect()
    }

    /// 历史估价加请求装配提供的指令与工具开销，再加实测校正。
    pub(crate) fn request_tokens(&self, overhead: u64) -> u64 {
        self.estimated_tokens
            .saturating_add(overhead)
            .saturating_add(self.usage_correction)
    }

    /// 把实测的输入与输出对齐到同一请求的估价上；usage 缺失时只用启发式计量。
    pub(crate) fn record_usage(
        &mut self,
        usage: &ModelUsage,
        assistant_tokens: u64,
        overhead: u64,
    ) {
        let estimated = self
            .estimated_tokens
            .saturating_add(assistant_tokens)
            .saturating_add(overhead);
        self.usage_correction = if usage.usage_present {
            usage.total_tokens.saturating_sub(estimated)
        } else {
            0
        };
    }

    /// 文件指令重新读取后，旧请求的实测校正不再适用。
    pub(crate) fn reset_usage_correction(&mut self) {
        self.usage_correction = 0;
    }

    /// 把刚提交的日志位置推进到视图里；正常追加和恢复走同一套排序规则。
    pub fn append_entry(&mut self, session: &SessionData, index: usize) -> Result<()> {
        let entry = &session.entries()[index];
        // 压缩与剪枝替换了历史结构，必须整表重建视图。
        if matches!(
            entry,
            SessionEntry::Compaction { .. }
                | SessionEntry::Record {
                    record: LedgerRecord::ToolResultPruned { .. },
                    ..
                }
        ) {
            return self.rebuild(session);
        }
        if is_context_entry(entry) {
            self.estimated_tokens = self
                .estimated_tokens
                .saturating_add(entry_token_estimate(entry));
            push_context_entry(
                &mut self.entries,
                ContextPosition {
                    index,
                    pruned_index: None,
                },
                session,
            );
        }
        Ok(())
    }

    /// 结构替换（压缩、工具结果剪枝）之后重建视图：被替换掉的内容已经不是产生旧
    /// 实测校正的那份请求形状，所以校正一并作废。正常追加不重建，校正继续有效。
    pub fn rebuild(&mut self, session: &SessionData) -> Result<()> {
        *self = Self::derive(session)?;
        Ok(())
    }
}

/// 日志只追加，所以位置在同一份 SessionData 内始终稳定；剪枝正文仍由原始日志持有。
#[derive(Debug, Clone, Copy)]
struct ContextPosition {
    index: usize,
    pruned_index: Option<usize>,
}

impl ContextPosition {
    fn entry<'a>(&self, session: &'a SessionData) -> &'a SessionEntry {
        &session.entries()[self.index]
    }

    fn content<'a>(&self, session: &'a SessionData) -> &'a [ContentBlock] {
        if let Some(index) = self.pruned_index {
            let SessionEntry::Record {
                record: LedgerRecord::ToolResultPruned { content, .. },
                ..
            } = &session.entries()[index]
            else {
                unreachable!("pruned position references its ledger record");
            };
            return content;
        }
        let SessionEntry::Message { message, .. } = self.entry(session) else {
            unreachable!("content is only read for messages");
        };
        message.content()
    }

    fn token_estimate(&self, session: &SessionData) -> u64 {
        match self.entry(session) {
            SessionEntry::Message { message, .. } => content_token_estimate(
                self.content(session),
                matches!(message, AgentMessage::ToolResult { .. }),
            ),
            entry => entry_token_estimate(entry),
        }
    }

    /// 对话历史、Skill 与摘要前缀共用同一套消息投影；文件指令由 Agent 加入。
    fn model_message(&self, session: &SessionData) -> Option<ModelMessage> {
        Some(match context_entry(self.entry(session))? {
            ContextEntry::Message(message) => match message {
                AgentMessage::User { .. } => ModelMessage::text(
                    ModelRole::User,
                    crate::message::content_text(self.content(session)),
                ),
                AgentMessage::Assistant { .. } => ModelMessage {
                    role: ModelRole::Assistant,
                    content: crate::message::content_text(self.content(session)),
                    tool_call_id: None,
                    tool_calls: message.tool_calls().cloned().collect(),
                    provider_reasoning_replay: message.provider_reasoning_replay().cloned(),
                },
                AgentMessage::ToolResult { tool_call_id, .. } => {
                    let mut llm = ModelMessage::text(
                        ModelRole::Tool,
                        crate::message::content_text(self.content(session)),
                    );
                    llm.tool_call_id = Some(tool_call_id.clone());
                    llm
                }
            },
            ContextEntry::Summary(summary) => {
                ModelMessage::text(ModelRole::User, compaction_summary(summary))
            }
            ContextEntry::SkillInstructions(text) => ModelMessage::text(ModelRole::User, text),
        })
    }
}

/// 按日志顺序归约出唯一的活动历史：摘要替换掉前缀，剪枝记录只借用替换后的正文。
/// 会进入可压缩历史的条目；文件指令由 Agent 直接加入请求。
pub(crate) fn is_context_entry(entry: &SessionEntry) -> bool {
    context_entry(entry).is_some()
}

/// 从后往前累加到保留预算，再在候选上界之内取最后一个工具对闭合的切点；
/// 预算为零时仍保留最后一个完整单元。
fn find_cut_point(
    entries: &[ContextPosition],
    session: &SessionData,
    keep_recent_tokens: u64,
) -> usize {
    let mut accumulated = 0u64;
    for index in (0..entries.len()).rev() {
        accumulated = accumulated.saturating_add(entries[index].token_estimate(session));
        if accumulated >= keep_recent_tokens {
            return last_balanced_cut(entries, session, index);
        }
    }
    0
}

/// 一次向前扫描，取出候选上界内最后一个闭合前缀的长度。前缀最多到 upper_bound
/// （那个位置的条目属于保留部分）；一旦出现孤立结果，更长的前缀都不合法，可以直接停。
fn last_balanced_cut(
    entries: &[ContextPosition],
    session: &SessionData,
    upper_bound: usize,
) -> usize {
    let mut pending = std::collections::HashSet::new();
    let mut cut = 0usize;
    for (position, candidate) in entries.iter().take(upper_bound).enumerate() {
        let Some(closed) = absorb_tool_pairing(&mut pending, candidate.entry(session)) else {
            break;
        };
        // Skill 与触发它的用户输入一同保留或一同摘要。
        if closed
            && !matches!(
                candidate.entry(session),
                SessionEntry::Record {
                    record: LedgerRecord::SkillInstructions { .. },
                    ..
                }
            )
        {
            cut = position + 1;
        }
    }
    cut
}
