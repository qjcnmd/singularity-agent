//! 上下文视图：从 Session ledger 派生的模型请求输入与唯一计量。
//!
//! ContextView 按日志顺序归约模型可见条目，并统一计算内容估价、
//! 实测校正和合法压缩切点。原始会话始终由 Session ledger 持有。
//! 请求装配、压缩判定与溢出恢复共用同一视图。

use singularity_model::{ModelMessage, ModelRole, ModelUsage};

use crate::message::{AgentMessageRole, COMPACTION_SUMMARY_PREFIX, COMPACTION_SUMMARY_SUFFIX};

use super::format::{LedgerRecord, Result, SessionEntry, SessionError};
use super::manager::SessionManager;

/// 基于 UTF-16 字符数的启发式 Token 估算（ceil(chars / 4)）：全仓唯一实现。
pub(crate) fn estimate_tokens_of(text: &str) -> u64 {
    let chars = text.encode_utf16().count() as u64;
    chars.div_ceil(4)
}

/// 估算模型可见内容及角色、内容块的结构开销；操作记录与元数据计零。
pub(crate) fn entry_token_estimate(entry: &SessionEntry) -> u64 {
    match entry {
        SessionEntry::Message { message, .. } => message_token_estimate(message),
        SessionEntry::Compaction { compaction, .. } => {
            estimate_tokens_of(&format!(
                "{COMPACTION_SUMMARY_PREFIX}{}{COMPACTION_SUMMARY_SUFFIX}",
                compaction.summary
            )) + 8
        }
        SessionEntry::Record {
            record: LedgerRecord::Instructions { text } | LedgerRecord::SkillInstructions { text },
            ..
        } => estimate_tokens_of(text) + 8,
        SessionEntry::Metadata { .. } | SessionEntry::Record { .. } => 0,
    }
}

pub(crate) fn message_token_estimate(message: &crate::message::AgentMessage) -> u64 {
    use crate::message::ContentBlock;
    let mut tokens = 4u64;
    for block in message.content() {
        tokens = tokens.saturating_add(match block {
            ContentBlock::Text { text } => estimate_tokens_of(text) + 4,
            ContentBlock::Thinking { thinking, .. } => estimate_tokens_of(thinking) + 4,
            ContentBlock::ToolCall { name, args, .. } => {
                estimate_tokens_of(name) + estimate_tokens_of(&args.to_string()) + 4
            }
        });
    }
    if message.role() == AgentMessageRole::ToolResult {
        tokens += 4;
    }
    tokens
}

/// 从 ledger 派生的模型上下文视图。
#[derive(Debug, Clone)]
pub struct ContextView {
    entries: Vec<SessionEntry>,
    /// 条目内容的估算求和（usage 基线缺失时的兜底计量）。
    estimated_tokens: u64,
    /// 实测总量超出同一请求完整启发式估价的部分；替换历史时保留这一校正。
    usage_correction: u64,
}

impl ContextView {
    /// 校验引用必须指向当时活动的模型上下文；不允许复活已被摘要替换的历史。
    pub fn validate(session: &SessionManager) -> Result<()> {
        build_context_entries(session).map(|_| ())
    }

    pub fn derive(session: &SessionManager) -> Result<Self> {
        let entries = build_context_entries(session)?;
        let estimated_tokens = entries.iter().map(entry_token_estimate).sum();
        Ok(Self {
            entries,
            estimated_tokens,
            usage_correction: 0,
        })
    }

    pub fn entries(&self) -> &[SessionEntry] {
        &self.entries
    }

    /// 系统、工具和当前历史的估价，加上同一模型最近一次请求的实测校正。
    pub(crate) fn request_tokens(&self, overhead: u64) -> u64 {
        self.estimated_tokens
            .saturating_add(overhead)
            .saturating_add(self.usage_correction)
    }

    /// 实测输入和输出与同一请求估价对齐；usage 缺失时只使用启发式计量。
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

    pub fn append_entry(&mut self, entry: &SessionEntry) {
        self.estimated_tokens = self
            .estimated_tokens
            .saturating_add(entry_token_estimate(entry));
        push_context_entry(&mut self.entries, entry);
    }

    /// 替换只改变启发式差量，不丢弃同一模型请求包络的实测锚点。
    pub fn rebuild(&mut self, session: &SessionManager) -> Result<()> {
        let correction = self.usage_correction;
        *self = Self::derive(session)?;
        self.usage_correction = correction;
        Ok(())
    }
}

/// 按日志顺序归约唯一活动历史；摘要替换前缀，剪枝原位替换工具文本。
fn build_context_entries(session: &SessionManager) -> Result<Vec<SessionEntry>> {
    let mut context: Vec<SessionEntry> = Vec::new();
    for entry in session.entries() {
        match entry {
            SessionEntry::Compaction { compaction, .. } => {
                let index = context
                    .iter()
                    .position(|candidate| candidate.id() == compaction.first_kept_entry_id)
                    .ok_or_else(|| SessionError::LedgerCorrupt {
                        reason: "invalid_compaction_anchor".into(),
                        detail: format!(
                            "compaction {} references an inactive anchor {}",
                            entry.id(),
                            compaction.first_kept_entry_id
                        ),
                    })?;
                if !balanced_before(&context, index) {
                    return Err(SessionError::LedgerCorrupt {
                        reason: "invalid_compaction_anchor".into(),
                        detail: "compaction splits a tool call/result pair".into(),
                    });
                }
                context.drain(..index);
                context.insert(0, entry.clone());
            }
            SessionEntry::Record {
                record: LedgerRecord::ToolResultPruned { entry_id, content },
                ..
            } => {
                let original = context
                    .iter_mut()
                    .find(|candidate| candidate.id() == entry_id);
                let Some(SessionEntry::Message {
                    message:
                        crate::message::AgentMessage::ToolResult {
                            content: target, ..
                        },
                    ..
                }) = original
                else {
                    return Err(SessionError::LedgerCorrupt {
                        reason: "invalid_prune_anchor".into(),
                        detail: format!("pruning references inactive tool result {entry_id}"),
                    });
                };
                *target = content.clone();
            }
            _ if is_context_entry(entry) => push_context_entry(&mut context, entry),
            _ => {}
        }
    }
    Ok(context)
}

/// Completion order is a durable fact, while provider replay orders sibling
/// results by the assistant's calls. Apply the same projection live and on reopen.
fn push_context_entry(context: &mut Vec<SessionEntry>, entry: &SessionEntry) {
    if let SessionEntry::Message { message, .. } = entry
        && let Some(call_id) = message.tool_call_id()
        && let Some((assistant_index, call_ids)) =
            context
                .iter()
                .enumerate()
                .rev()
                .find_map(|(index, candidate)| {
                    let SessionEntry::Message { message, .. } = candidate else {
                        return None;
                    };
                    let ids = message
                        .tool_calls()
                        .filter_map(|call| {
                            if let crate::message::ContentBlock::ToolCall { id, .. } = call {
                                Some(id.clone())
                            } else {
                                None
                            }
                        })
                        .collect::<Vec<_>>();
                    ids.iter().any(|id| id == call_id).then_some((index, ids))
                })
    {
        let ordinal = call_ids.iter().position(|id| id == call_id);
        let insert_at = context
            .iter()
            .enumerate()
            .skip(assistant_index + 1)
            .find_map(|(index, candidate)| {
                let SessionEntry::Message { message, .. } = candidate else {
                    return None;
                };
                let id = message.tool_call_id()?;
                let existing = call_ids.iter().position(|call| call == id)?;
                (Some(existing) > ordinal).then_some(index)
            })
            .unwrap_or(context.len());
        context.insert(insert_at, entry.clone());
    } else {
        context.push(entry.clone());
    }
}

/// 指定切点之前的工具调用必须全部闭合，孤立结果不构成合法边界。
pub(crate) fn balanced_before(entries: &[SessionEntry], end: usize) -> bool {
    let mut pending = std::collections::HashSet::new();
    for entry in &entries[..end] {
        if let SessionEntry::Message { message, .. } = entry {
            for call in message.tool_calls() {
                if let crate::message::ContentBlock::ToolCall { id, .. } = call {
                    pending.insert(id);
                }
            }
            if let crate::message::AgentMessage::ToolResult { tool_call_id, .. } = message
                && !tool_call_id.as_ref().is_some_and(|id| pending.remove(id))
            {
                return false;
            }
        }
    }
    pending.is_empty()
}

/// 所有请求复用同一消息投影，包括摘要前缀和重新注入的文件指令。
pub(crate) fn entry_to_llm_messages(entry: &SessionEntry) -> Vec<ModelMessage> {
    match entry {
        SessionEntry::Message { message, .. } => match message.role() {
            AgentMessageRole::User => {
                vec![ModelMessage::text(ModelRole::User, message.content_text())]
            }
            AgentMessageRole::Assistant => {
                let tool_calls = message
                    .tool_calls()
                    .filter_map(super::super::message::ContentBlock::to_model_tool_call)
                    .collect::<Vec<_>>();
                let llm = ModelMessage {
                    role: ModelRole::Assistant,
                    content: message.content_text(),
                    tool_call_id: None,
                    tool_calls,
                    provider_reasoning_replay: message.provider_reasoning_replay().cloned(),
                };
                vec![llm]
            }
            AgentMessageRole::ToolResult => {
                let mut llm = ModelMessage::text(ModelRole::Tool, message.content_text());
                llm.tool_call_id = message.tool_call_id().cloned();
                vec![llm]
            }
        },
        SessionEntry::Compaction { compaction, .. } => vec![ModelMessage::text(
            ModelRole::User,
            format!(
                "{COMPACTION_SUMMARY_PREFIX}{}{COMPACTION_SUMMARY_SUFFIX}",
                compaction.summary
            ),
        )],
        SessionEntry::Record {
            record: LedgerRecord::Instructions { text } | LedgerRecord::SkillInstructions { text },
            ..
        } => vec![ModelMessage::text(ModelRole::User, text)],
        SessionEntry::Metadata { .. } | SessionEntry::Record { .. } => Vec::new(),
    }
}

/// 有模型消息的条目；指令记录和摘要遵循与普通消息相同的保留边界。
pub(crate) fn is_context_entry(entry: &SessionEntry) -> bool {
    matches!(
        entry,
        SessionEntry::Message { .. }
            | SessionEntry::Compaction { .. }
            | SessionEntry::Record {
                record: LedgerRecord::Instructions { .. } | LedgerRecord::SkillInstructions { .. },
                ..
            }
    )
}
