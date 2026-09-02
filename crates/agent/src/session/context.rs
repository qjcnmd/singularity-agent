//! 上下文视图：从 Session ledger 派生的模型请求输入与唯一计量。
//!
//! [`ContextView`] 是 Context 子系统的权威视图（data-model.md 的 Context
//! View）：compaction 感知的有序条目、内容估算、provider usage 基线与上报后
//! 尾部增量、合法压缩切点。它只由 ledger 派生、可重算，绝不持有第二份历史。
//! 请求装配、压缩判定与溢出恢复共用同一视图。

use singularity_model::{ModelMessage, ModelRole, ModelUsage};

use crate::message::{AgentMessageRole, COMPACTION_SUMMARY_PREFIX, COMPACTION_SUMMARY_SUFFIX};

use super::format::{Result, SessionEntry, SessionError};
use super::manager::SessionManager;

/// 基于 UTF-16 字符数的启发式 Token 估算（`ceil(chars / 4)`）：全仓唯一实现。
pub(crate) fn estimate_tokens_of(text: &str) -> u64 {
    let chars = text.encode_utf16().count() as u64;
    chars.div_ceil(4)
}

/// 估算单条会话条目贡献的 Token 数量：文本、工具调用（id/name/args）、
/// thinking 块与 provider reasoning replay 全部计入；metadata 与 ledger
/// 记录不进入模型上下文，计 0。
pub(crate) fn entry_token_estimate(entry: &SessionEntry) -> u64 {
    match entry {
        SessionEntry::Message { message, .. } => message_token_estimate(message),
        SessionEntry::Compaction { compaction, .. } => estimate_tokens_of(&compaction.summary),
        SessionEntry::Metadata { .. } | SessionEntry::Record { .. } => 0,
    }
}

fn message_token_estimate(message: &crate::message::AgentMessage) -> u64 {
    use crate::message::ContentBlock;
    let mut tokens = estimate_tokens_of(&message.content_text());
    for block in message.content() {
        match block {
            ContentBlock::ToolCall { id, name, args } => {
                tokens = tokens
                    .saturating_add(estimate_tokens_of(id))
                    .saturating_add(estimate_tokens_of(name))
                    .saturating_add(estimate_tokens_of(&args.to_string()));
            }
            ContentBlock::Thinking { thinking, .. } => {
                tokens = tokens.saturating_add(estimate_tokens_of(thinking));
            }
            ContentBlock::Text { .. } => {}
        }
    }
    if let Some(replay) = message.provider_reasoning_replay() {
        tokens = tokens.saturating_add(estimate_tokens_of(
            &serde_json::to_string(replay).unwrap_or_default(),
        ));
    }
    tokens
}

/// 从 ledger 派生的模型上下文视图。
#[derive(Debug, Clone)]
pub struct ContextView {
    entries: Vec<SessionEntry>,
    /// 条目内容的估算求和（usage 基线缺失时的兜底计量）。
    estimated_tokens: u64,
    /// provider 最后上报的上下文 token 数（请求发出时的真实占用）。
    usage_baseline: Option<u64>,
    /// 上报之后追加到会话的条目的 token 估算。
    trailing_estimate: u64,
}

impl ContextView {
    /// 校验所有 compaction 锚点，并返回可观测的存储错误而不是静默丢弃历史。
    pub fn validate(session: &SessionManager) -> Result<()> {
        let entries = session.entries();
        for (index, entry) in entries.iter().enumerate() {
            let SessionEntry::Compaction { compaction, .. } = entry else {
                continue;
            };
            let Some(anchor_index) = entries[..index]
                .iter()
                .position(|candidate| candidate.id() == compaction.first_kept_entry_id)
            else {
                return Err(SessionError::LedgerCorrupt {
                    reason: "invalid_compaction_anchor".to_string(),
                    detail: format!(
                        "compaction {} references missing first kept entry {}",
                        entry.id(),
                        compaction.first_kept_entry_id
                    ),
                });
            };
            if !matches!(
                entries[anchor_index],
                SessionEntry::Message { .. } | SessionEntry::Compaction { .. }
            ) {
                return Err(SessionError::LedgerCorrupt {
                    reason: "invalid_compaction_anchor".to_string(),
                    detail: format!(
                        "compaction {} anchors to non-context entry {}",
                        entry.id(),
                        compaction.first_kept_entry_id
                    ),
                });
            }
        }
        Ok(())
    }

    /// 校验后派生上下文；损坏锚点以 typed storage error 返回。
    pub fn derive(session: &SessionManager) -> Result<Self> {
        Self::validate(session)?;
        Ok(Self::derive_unchecked(session))
    }

    fn derive_unchecked(session: &SessionManager) -> Self {
        let entries = build_context_entries(session);
        let estimated_tokens = entries.iter().map(entry_token_estimate).sum();
        Self {
            entries,
            estimated_tokens,
            usage_baseline: None,
            trailing_estimate: 0,
        }
    }

    pub fn entries(&self) -> &[SessionEntry] {
        &self.entries
    }

    /// 内容估算求和：usage 基线缺失时（首轮、压缩重写后）的请求前计量。
    pub fn estimated_tokens(&self) -> u64 {
        self.estimated_tokens
    }

    /// 请求前上下文规模的唯一计量：usage 基线 + 尾部增量；无基线时返回
    /// `None`，调用方以 [`Self::estimated_tokens`] 兜底。
    pub fn effective_tokens(&self) -> Option<u64> {
        Some(self.usage_baseline?.saturating_add(self.trailing_estimate))
    }

    /// 发送前该按多大的上下文做决策（压缩判定与输出预算共用这一个取数口径）：
    /// 有 usage 基线时用基线 + 尾部增量，否则用内容估算求和。调用方不再各自
    /// 展开 `effective_tokens().unwrap_or_else(...)`。
    pub fn request_tokens(&self) -> u64 {
        self.effective_tokens()
            .unwrap_or_else(|| self.estimated_tokens())
    }

    /// 记录 provider 上报的 usage：尾部增量归零（本轮追加的条目从下一轮起入账）。
    pub fn record_usage(&mut self, usage: &ModelUsage) {
        if usage.usage_present {
            self.usage_baseline = Some(usage.total_tokens);
            self.trailing_estimate = 0;
        }
    }

    /// turn 内追加一条模型可见条目：并入视图尾部、更新计量。
    /// Assistant 消息的 token 消耗在调用完成时已含于 `record_usage` 的
    /// total_tokens，尾部增量只对非 assistant 条目累加，防双重计入；内容估算
    /// 求和则对所有条目累加（usage 基线缺失时的兜底）。
    pub fn append_entry(&mut self, entry: &SessionEntry) {
        let estimate = entry_token_estimate(entry);
        self.estimated_tokens = self.estimated_tokens.saturating_add(estimate);
        let assistant = matches!(
            entry,
            SessionEntry::Message { message, .. }
                if matches!(message.role(), AgentMessageRole::Assistant)
        );
        if !assistant {
            self.trailing_estimate = self.trailing_estimate.saturating_add(estimate);
        }
        self.entries.push(entry.clone());
    }

    /// compaction 重写会话尾部后重建视图：条目与内容估算全部按 ledger
    /// 重算，usage 基线作废（回退到装配估算兜底）。
    pub fn rebuild(&mut self, session: &SessionManager) -> Result<()> {
        *self = Self::derive(session)?;
        Ok(())
    }
}

/// 构建活跃的、compaction 感知的条目列表：最新压缩节点 + 其保留尾 + 之后条目。
fn build_context_entries(session: &SessionManager) -> Vec<SessionEntry> {
    let entries = session.entries();
    let mut compaction_index = None;
    for (index, entry) in entries.iter().enumerate() {
        if matches!(entry, SessionEntry::Compaction { .. }) {
            compaction_index = Some(index);
        }
    }
    let Some(compaction_index) = compaction_index else {
        return entries.to_vec();
    };
    let first_kept = match &entries[compaction_index] {
        SessionEntry::Compaction { compaction, .. } => {
            Some(compaction.first_kept_entry_id.as_str())
        }
        _ => None,
    };
    let mut context = vec![entries[compaction_index].clone()];
    let mut found_first_kept = false;
    for entry in &entries[..compaction_index] {
        if Some(entry.id()) == first_kept {
            found_first_kept = true;
        }
        if found_first_kept {
            context.push(entry.clone());
        }
    }
    context.extend_from_slice(&entries[compaction_index + 1..]);
    context
}

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
                if tool_calls.is_empty() {
                    return vec![ModelMessage::text(
                        ModelRole::Assistant,
                        message.content_text(),
                    )];
                }
                let mut llm = ModelMessage::assistant_tool_calls(tool_calls);
                llm.content = message.content_text();
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
        SessionEntry::Metadata { .. } | SessionEntry::Record { .. } => Vec::new(),
    }
}
