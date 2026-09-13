//! 上下文缩减策略、摘要请求构造与结果校验。
//!
//! 切点保持完整工具批次，摘要仅替换早期历史；原系统提示和工具定义保留。
//! Agent 负责统一请求执行、取消与持久提交，文件指令在压缩后重新加载。

use crate::message::{COMPACTION_SUMMARY_PREFIX, COMPACTION_SUMMARY_SUFFIX, ContentBlock};
use crate::request_execution::output_token_budget;
use crate::session::context::{
    entry_to_llm_message, entry_token_estimate, estimate_tokens_of, is_context_entry,
};
use crate::session::{CompactionEntry, SessionEntry, SessionError, turn_usage_from_model_usage};
use singularity_model::{
    ModelConfigurationSnapshot, ModelMessage, ModelPreferences, ModelRole, ModelToolSchema,
    ModelTurnRequest, ModelTurnResponse, ProviderError,
};
use thiserror::Error;

/// 摘要请求的最大输出 Token 数，受当前模型输出上限约束。
pub const DEFAULT_SUMMARY_MAX_TOKENS: u32 = 8192;

/// 按模型窗口缩放的触发与近期历史保留比例。
#[derive(Debug, Clone, PartialEq)]
pub struct CompactionConfig {
    /// 触发主动压缩的窗口占用比例。
    pub threshold_ratio: f64,
    /// 原样保留近期历史的最低窗口比例。
    pub retain_ratio: f64,
}
impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            threshold_ratio: 0.9,
            retain_ratio: 0.1,
        }
    }
}
impl CompactionConfig {
    /// 达到窗口阈值即触发，包括相等的边界。
    pub fn should_compact(&self, context_tokens: u64, context_window: u64) -> bool {
        context_tokens >= (context_window as f64 * self.threshold_ratio).floor() as u64
    }

    pub(crate) fn retain_tokens(&self, window: u64) -> u64 {
        (window as f64 * self.retain_ratio).floor() as u64
    }
}

const COMPACTION_INSTRUCTION: &str = r#"Condense the conversation above into a checkpoint for another coding assistant to resume the task. System instructions remain outside the replaced history. File-based instructions may occur in the conversation; their authoritative text will be reloaded independently, so do not treat this checkpoint as a substitute for those files. Do not use tools or continue the task.
Output these Markdown sections in this order, using concise English bullets and '(none)' for empty sections:
## Primary Request and Intent
Original and evolving user goals; preserve exact wording when consequential.
## Key Technical Concepts
Relevant technologies, conventions and patterns.
## Files and Code
Exact paths, their purpose, changes and essential snippets.
## Errors and Fixes
Failures, resolutions and related user corrections.
## Pending Jobs
Explicitly requested work that is unfinished.
## Current Work
Precisely what is in progress.
## Next Step
The next action consistent with the latest request, or '(none)'.
## Critical Context
Decisions and reasons, constraints, preferences, open questions and necessary data.
Preserve exact commands, identifiers, paths, numbers and error strings. Merge any prior <compacted-summary> checkpoint with newer facts; discard stale facts. Capture user feedback faithfully. Output only the checkpoint, without mentioning this request or compaction."#;

/// compact 入口的结果。
#[derive(Debug, Clone, PartialEq)]
pub enum CompactionOutcome {
    /// 未触发或无可摘要内容。
    NotNeeded,
    /// 摘要或工具输出剪枝已经持久化。
    Reduced,
}

/// Compaction 错误。
#[derive(Debug, Error)]
pub enum CompactionError {
    /// 压缩请求被取消（与采样取消同一语义；不视为压缩故障）。
    #[error("compaction aborted")]
    Aborted,
    #[error("summarization provider error: {0}")]
    Provider(#[from] ProviderError),
    #[error("session error: {0}")]
    Session(#[from] SessionError),
    #[error("{0}")]
    InvalidResponse(String),
}

/// compact 结果别名。
pub type Result<T> = std::result::Result<T, CompactionError>;

/// 摘要请求及其替换边界；响应通过校验后才能生成持久条目。
pub(crate) struct PreparedCompaction {
    pub(crate) request: ModelTurnRequest,
    first_kept_entry_id: String,
    replaced_tokens: u64,
}
impl PreparedCompaction {
    pub(crate) fn new(
        entries: &[SessionEntry],
        keep_recent_tokens: u64,
        tokens_before: u64,
        instruction: Option<&ModelMessage>,
        tools: &[ModelToolSchema],
        model: &ModelConfigurationSnapshot,
    ) -> Result<Option<Self>> {
        if entries.is_empty() {
            return Ok(None);
        }
        let cut = find_cut_point(entries, keep_recent_tokens);
        let prefix = &entries[..cut];
        let prefix_messages: Vec<_> = prefix.iter().filter_map(entry_to_llm_message).collect();
        if prefix_messages.is_empty() {
            return Ok(None);
        }
        let before: u64 = prefix.iter().map(entry_token_estimate).sum();
        let first_kept_entry_id = entries[cut].id().to_string();
        let retained: u64 = entries[cut..].iter().map(entry_token_estimate).sum();
        let pressure = tokens_before
            .saturating_sub(retained)
            .saturating_add(estimate_tokens_of(COMPACTION_INSTRUCTION) + 8);
        let cap = output_token_budget(
            model.context_window(),
            pressure,
            DEFAULT_SUMMARY_MAX_TOKENS.min(model.max_output_tokens),
        );
        if cap == 0 {
            return Err(CompactionError::InvalidResponse(
                "insufficient context space for a summary response".into(),
            ));
        }
        let mut messages =
            Vec::with_capacity(prefix_messages.len() + usize::from(instruction.is_some()) + 1);
        if let Some(instruction) = instruction {
            messages.push(instruction.clone());
        }
        messages.extend(prefix_messages);
        messages.push(ModelMessage::text(ModelRole::User, COMPACTION_INSTRUCTION));
        let request = ModelTurnRequest {
            request_id: String::new(),
            messages,
            tools: tools.to_vec(),
            model_preferences: ModelPreferences {
                max_output_tokens: Some(cap),
            },
        };
        Ok(Some(Self {
            request,
            first_kept_entry_id,
            replaced_tokens: before,
        }))
    }

    pub(crate) fn into_entry(self, response: ModelTurnResponse) -> Result<CompactionEntry> {
        if response.is_length_truncated() {
            return Err(CompactionError::InvalidResponse(
                "summary reached the output limit (incomplete checkpoint)".into(),
            ));
        }
        if !response.tool_calls().is_empty() {
            return Err(CompactionError::InvalidResponse(
                "summary attempted to call a tool".into(),
            ));
        }
        let text = response.assistant_message.content;
        if text.trim().is_empty() {
            return Err(CompactionError::InvalidResponse(
                "summary contains no text".into(),
            ));
        }
        let framed = format!("{COMPACTION_SUMMARY_PREFIX}{text}{COMPACTION_SUMMARY_SUFFIX}");
        if estimate_tokens_of(&framed) + 8 >= self.replaced_tokens {
            return Err(CompactionError::InvalidResponse(
                "summary is not smaller than the replaced history".into(),
            ));
        }
        Ok(CompactionEntry {
            summary: text,
            first_kept_entry_id: self.first_kept_entry_id,
            usage: response
                .usage
                .usage_present
                .then(|| turn_usage_from_model_usage(&response.usage, true)),
            details: None,
        })
    }
}

/// 向后累加到保留预算，再向前退到工具对闭合处；零预算仍保留最后一个完整单元。
pub(crate) fn find_cut_point(entries: &[SessionEntry], keep_recent_tokens: u64) -> usize {
    let mut accumulated = 0u64;
    for index in (0..entries.len()).rev() {
        if !is_context_entry(&entries[index]) {
            continue;
        }
        accumulated = accumulated.saturating_add(entry_token_estimate(&entries[index]));
        if accumulated >= keep_recent_tokens {
            return (0..=index)
                .rev()
                .find(|&cut| {
                    is_context_entry(&entries[cut])
                        && crate::session::context::balanced_before(entries, cut)
                })
                .unwrap_or(0);
        }
    }
    0
}

/// 无模型剪枝：超过 8192 个 Unicode 字符时保留 4096 头部和 1024 尾部。
/// 只改变文本块；保留其他内容块及其相对顺序。
pub(crate) fn prune_tool_content(content: &[ContentBlock]) -> Option<Vec<ContentBlock>> {
    let total: usize = content
        .iter()
        .map(|block| match block {
            ContentBlock::Text { text } => text.chars().count(),
            _ => 0,
        })
        .sum();
    if total <= 8192 {
        return None;
    }
    let mut consumed = 0;
    let mut marked = false;
    Some(
        content
            .iter()
            .filter_map(|block| {
                let ContentBlock::Text { text } = block else {
                    return Some(block.clone());
                };
                let mut kept = String::new();
                for ch in text.chars() {
                    if consumed < 4096 || consumed >= total - 1024 {
                        kept.push(ch);
                    } else if !marked {
                        kept.push_str("\n\n[... tool result middle pruned ...]\n\n");
                        marked = true;
                    }
                    consumed += 1;
                }
                (!kept.is_empty()).then_some(ContentBlock::Text { text: kept })
            })
            .collect(),
    )
}

#[cfg(test)]
#[path = "compaction_tests.rs"]
mod tests;
