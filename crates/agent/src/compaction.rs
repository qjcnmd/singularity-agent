//! 上下文缩减策略、摘要请求构造与结果校验。
//!
//! 摘要仅替换早期历史，使用原系统提示与冻结的工具定义。
//! Agent 负责统一请求执行、取消与持久提交，文件指令在压缩后重新加载。

use crate::message::ContentBlock;
use crate::session::CompactionEntry;
use crate::session::context::CompactionPrefix;

use crate::agent::{AgentError, Result};
use singularity_model::{
    ModelConfigurationSnapshot, ModelMessage, ModelPreferences, ModelRole, ModelToolSchema,
    ModelTurnRequest, ModelTurnResponse,
};

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

const COMPACTION_INSTRUCTION: &str = r#"You are now acting as a compaction engine for this AI coding assistant. Condense the conversation ABOVE into a structured checkpoint that lets another model resume the work with no loss of essential context. System instructions remain outside the replaced history. File-based instructions may occur in the conversation; their authoritative text will be reloaded independently, so do not treat this checkpoint as a substitute for those files.

Output EXACTLY the Markdown structure below: keep every section, in order. Use terse bullets, not prose paragraphs. Write "(none)" for an empty section — never drop a section.

## Primary Request and Intent
- [the user's original and evolving goals; quote verbatim where the exact wording matters]

## Key Technical Concepts
- [technologies, frameworks, patterns, and conventions in play]

## Files and Code
- [exact path: why it matters, key changes or snippets]

## Errors and Fixes
- [error: how it was resolved, plus any related user feedback]

## Pending Jobs
- [explicitly requested work not yet completed]

## Current Work
- [precisely what was in progress at this checkpoint]

## Next Step
- [the single next action, directly in line with the most recent request, or "(none)"]

## Critical Context
- [decisions and their rationale, constraints, user preferences, open questions, data needed to continue]

Rules:
- Write concise English engineering prose. Preserve exact file paths, commands, error strings, identifiers, numeric values, function signatures, and syntax fragments.
- Capture user feedback and explicit instructions faithfully, especially corrections.
- Do NOT mention this summarization request or that the context was compacted.
- Output only the checkpoint text: do not call any tool or take any other action.
- If the conversation already contains a <compacted-summary> block, it is a PRIOR checkpoint. Do not copy it forward verbatim: preserve still-true facts, drop stale ones, and merge newer information into a single consolidated summary under the same structure."#;

/// compact 入口的结果。
#[derive(Debug, Clone, PartialEq)]
pub enum CompactionOutcome {
    /// 未触发或无可摘要内容。
    NotNeeded,
    /// 摘要或工具输出剪枝已经持久化。
    Reduced,
}

/// 摘要请求及其替换边界；响应通过校验后才能生成持久条目。
pub(crate) struct PreparedCompaction {
    pub(crate) request: ModelTurnRequest,
    first_kept_entry_id: String,
}
impl PreparedCompaction {
    /// 摘要请求复用原系统提示与冻结的工具定义，使这次调用成为上一次真实请求的
    /// 真前缀；输出上限只受模型输出上限约束。
    pub(crate) fn new(
        prefix: CompactionPrefix,
        instruction: Option<&ModelMessage>,
        tools: &[ModelToolSchema],
        model: &ModelConfigurationSnapshot,
    ) -> Self {
        let mut messages =
            Vec::with_capacity(prefix.messages.len() + usize::from(instruction.is_some()) + 1);
        if let Some(instruction) = instruction {
            messages.push(instruction.clone());
        }
        messages.extend(prefix.messages);
        messages.push(ModelMessage::text(ModelRole::User, COMPACTION_INSTRUCTION));
        let request = ModelTurnRequest {
            request_id: String::new(),
            messages,
            tools: tools.to_vec(),
            model_preferences: ModelPreferences {
                max_output_tokens: Some(DEFAULT_SUMMARY_MAX_TOKENS.min(model.max_output_tokens)),
            },
        };
        Self {
            request,
            first_kept_entry_id: prefix.first_kept_entry_id,
        }
    }

    pub(crate) fn into_entry(self, response: ModelTurnResponse) -> Result<CompactionEntry> {
        if response.is_length_truncated() {
            return Err(AgentError::InvalidSummary(
                "summary reached the output limit (incomplete checkpoint)".into(),
            ));
        }
        let text = response.assistant_message.content;
        if text.trim().is_empty() {
            return Err(AgentError::InvalidSummary(
                "summary contains no text".into(),
            ));
        }
        // 摘要请求的计量由统一请求账本记录（该请求自己的 request observation），
        // 会话累计与展示从中读取；条目只保留摘要与保留锚点。
        Ok(CompactionEntry {
            summary: text,
            first_kept_entry_id: self.first_kept_entry_id,
        })
    }
}

/// 剪枝正文的最小字符数：低于它不剪。
const PRUNE_MIN_CHARS: usize = 8192;
/// 剪枝后保留的头部与尾部字符数。
const PRUNE_KEEP_HEAD_CHARS: usize = 4096;
const PRUNE_KEEP_TAIL_CHARS: usize = 1024;

/// 无模型剪枝：超过 [`PRUNE_MIN_CHARS`] 个 Unicode 字符时保留头部与尾部。
/// 只改变文本块；保留其他内容块及其相对顺序。
pub(crate) fn prune_tool_content(content: &[ContentBlock]) -> Option<Vec<ContentBlock>> {
    let total: usize = content
        .iter()
        .map(|block| match block {
            ContentBlock::Text { text } => text.chars().count(),
            _ => 0,
        })
        .sum();
    if total <= PRUNE_MIN_CHARS {
        return None;
    }
    // 头尾预算是跨全部文本块累计的：字符计数与省略标记的推进必须写在同一段
    // 顺序代码里，不能表达成每块独立的过滤／映射。
    let mut text_chars_seen = 0;
    let mut ellipsis_written = false;
    let mut pruned = Vec::with_capacity(content.len());
    for block in content {
        let ContentBlock::Text { text } = block else {
            pruned.push(block.clone());
            continue;
        };
        let mut kept = String::new();
        for ch in text.chars() {
            if text_chars_seen < PRUNE_KEEP_HEAD_CHARS
                || text_chars_seen >= total - PRUNE_KEEP_TAIL_CHARS
            {
                kept.push(ch);
            } else if !ellipsis_written {
                kept.push_str("\n\n[... tool result middle pruned ...]\n\n");
                ellipsis_written = true;
            }
            text_chars_seen += 1;
        }
        if !kept.is_empty() {
            pruned.push(ContentBlock::Text { text: kept });
        }
    }
    Some(pruned)
}

#[cfg(test)]
#[path = "compaction_tests.rs"]
mod tests;
