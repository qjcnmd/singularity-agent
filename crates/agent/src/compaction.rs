//! 会话上下文压缩引擎（Context Compaction）。
//!
//! 当请求占用达到模型窗口阈值时，缩减早期历史并保留近期上下文。
//! 系统提示词与工具定义属于请求包络，不被摘要替换；文件指令属于有来源的
//! 会话上下文，压缩后由文件加载过程重新注入。
//!
//! 核心流程：
//! 1. 触发判定（should_compact）：请求压力达到窗口的 90% 时触发。
//!    压力包含系统、工具、历史的估价，以及最近一次同模型请求的实测校正。
//! 2. 工具剪枝（prune_tool_content）：仅保留区之前的旧工具输出保留头尾，替换内容
//!    追加到 Session ledger；完整原文继续供历史和轨迹查看。重新计量后，
//!    若压力低于阈值，不请求摘要。
//! 3. 切点选择（find_cut_point）：原样保留至少窗口 10% 的
//!    近期内容，向前调整切点以保持完整工具调用/结果批次。手动压缩和明确
//!    溢出恢复跳过比例预算，但仍保留最后一个完整消息或工具单元。
//! 4. 摘要生成（complete_summarization）：复用原系统提示词、工具定义
//!    和原生消息前缀，末尾追加摘要指令。空白、截断或不产生缩减的摘要失败，
//!    不修改当前历史；摘要请求不会执行工具。
//! 5. 持久化（SessionManager::append_compaction_with_id）：先提交摘要
//!    和保留锚点，再由 ContextView 按日志顺序归约有效历史。连续压缩只替换
//!    当前前缀，不重新引入已被覆盖的消息或摘要。

use crate::events::AgentEvents;
use crate::message::{COMPACTION_SUMMARY_PREFIX, COMPACTION_SUMMARY_SUFFIX, ContentBlock};
use crate::request_execution::{
    AttemptLedger, SendOutcome, output_token_budget, send_with_retry, stream_completion_once,
};
use crate::session::context::{
    entry_to_llm_messages, entry_token_estimate, estimate_tokens_of, is_context_entry,
};
use crate::session::{
    CompactionEntry, SessionEntry, SessionError, lock_writer, turn_usage_from_model_usage,
};
use singularity_core::CancellationToken;
use singularity_model::{
    ModelMessage, ModelPreferences, ModelRole, ModelTurnRequest, ModelUsage, Provider,
    ProviderError,
};
use std::sync::Arc;
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
    /// 工具输出剪枝已经持久化，上下文可重新发送。
    Pruned,
    Compacted {
        first_kept_entry_id: String,
        tokens_before: u64,
    },
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

/// 当前请求及其派生历史；模板保留系统、工具定义和兼容的 reasoning 前缀。
pub(crate) struct CompactionInput<'a> {
    pub entries: &'a [SessionEntry],
    pub keep_recent_tokens: u64,
    pub tokens_before: u64,
    pub request: ModelTurnRequest,
}

/// 摘要引擎不拥有独立提示词或工具注册表。
pub struct CompactionEngine {
    provider: Arc<dyn Provider + Send + Sync>,
    model: singularity_model::ModelConfigurationSnapshot,
}
impl CompactionEngine {
    pub fn new(
        provider: Arc<dyn Provider + Send + Sync>,
        model: singularity_model::ModelConfigurationSnapshot,
    ) -> Self {
        Self { provider, model }
    }
    pub(crate) fn compact(
        &mut self,
        ledger: &mut AttemptLedger<'_>,
        input: CompactionInput<'_>,
        events: &mut AgentEvents,
        cancellation: &CancellationToken,
    ) -> Result<CompactionOutcome> {
        if cancellation.is_cancelled() {
            return Err(CompactionError::Aborted);
        }
        let entries = input.entries;
        if entries.is_empty() {
            return Ok(CompactionOutcome::NotNeeded);
        }
        let cut = Self::find_cut_point(entries, input.keep_recent_tokens);
        let prefix = &entries[..cut];
        let prefix_messages: Vec<_> = prefix.iter().flat_map(entry_to_llm_messages).collect();
        if prefix_messages.is_empty() {
            return Ok(CompactionOutcome::NotNeeded);
        }
        let before: u64 = prefix.iter().map(entry_token_estimate).sum();
        let first_kept_entry_id = entries[cut].id().to_string();
        let mut request = input.request;
        // 原模板中的系统消息不属于历史替换范围。
        request
            .messages
            .retain(|message| matches!(message.role, ModelRole::System | ModelRole::Developer));
        request.messages.extend(prefix_messages);
        request
            .messages
            .push(ModelMessage::text(ModelRole::User, COMPACTION_INSTRUCTION));
        let retained: u64 = entries[cut..].iter().map(entry_token_estimate).sum();
        let pressure = input
            .tokens_before
            .saturating_sub(retained)
            .saturating_add(estimate_tokens_of(COMPACTION_INSTRUCTION) + 8);
        let summary =
            self.complete_summarization(request, pressure, ledger, events, cancellation)?;
        let framed = format!(
            "{COMPACTION_SUMMARY_PREFIX}{}{COMPACTION_SUMMARY_SUFFIX}",
            summary.text
        );
        if estimate_tokens_of(&framed) + 8 >= before {
            return Err(CompactionError::InvalidResponse(
                "summary is not smaller than the replaced history".into(),
            ));
        }
        if cancellation.is_cancelled() {
            return Err(CompactionError::Aborted);
        }
        let entry = CompactionEntry {
            summary: summary.text,
            first_kept_entry_id: first_kept_entry_id.clone(),
            usage: summary
                .usage
                .usage_present
                .then(|| turn_usage_from_model_usage(&summary.usage, true)),
            details: None,
        };
        let id = ledger.result_entry_id().to_string();
        lock_writer(ledger.writer()).append_compaction_with_id(&id, entry)?;
        Ok(CompactionOutcome::Compacted {
            first_kept_entry_id,
            tokens_before: input.tokens_before,
        })
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

    fn complete_summarization(
        &self,
        mut request: ModelTurnRequest,
        pressure: u64,
        ledger: &mut AttemptLedger<'_>,
        events: &mut AgentEvents,
        cancellation: &CancellationToken,
    ) -> Result<SummaryResponse> {
        let cap = output_token_budget(
            self.model.context_window(),
            pressure,
            DEFAULT_SUMMARY_MAX_TOKENS.min(self.model.capabilities.max_output_tokens),
        );
        if cap == 0 {
            return Err(CompactionError::InvalidResponse(
                "insufficient context space for a summary response".into(),
            ));
        }
        request.model_preferences = ModelPreferences {
            model_name: Some(self.model.model.clone()),
            max_output_tokens: Some(cap),
        };
        let response = match send_with_retry(
            |ledger, events| {
                stream_completion_once(
                    &self.provider,
                    &mut request,
                    ledger,
                    events,
                    cancellation,
                    0,
                    singularity_protocol::RequestPurpose::Compaction,
                )
            },
            ledger,
            self.model.retry,
            events,
            cancellation,
        ) {
            SendOutcome::Response(response) => *response,
            SendOutcome::Aborted => return Err(CompactionError::Aborted),
            SendOutcome::Failed(_) if cancellation.is_cancelled() => {
                return Err(CompactionError::Aborted);
            }
            SendOutcome::Failed(error) => return Err(CompactionError::Provider(error)),
            SendOutcome::Store(error) => return Err(CompactionError::Session(error)),
        };
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
        Ok(SummaryResponse {
            text,
            usage: response.usage,
        })
    }
}
struct SummaryResponse {
    text: String,
    usage: ModelUsage,
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
