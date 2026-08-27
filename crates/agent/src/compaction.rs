//! 会话上下文自动压缩引擎（Context Compaction）。
//!
//! 在长程多轮会话中，当累计的上下文 Token 数接近模型上下文窗口上限时，
//! 压缩引擎会自动提取历史对话的结构化摘要，修剪早期详细历史并保留最新对话上下文。
//!
//! 核心流程：
//! 1. **触发判定**（`should_compact`）：请求发出前优先基于上一轮 provider usage
//!    判定，首轮或 usage 缺失时使用本轮装配估算；超过「上下文窗口 −
//!    `reserve_tokens` 预留」即触发，预留空间供模型回答使用。
//! 2. **切点查找**（`find_cut_point`）：从最新消息向后回溯，保留 `keep_recent_tokens` 预算内的最新消息；
//!    保证切点绝不切在工具结果（`tool_result`）中间，避免破坏模型工具调用配对结构；超长轮次支持 split turn 前缀摘要。
//! 3. **结构化摘要生成**（`generate_summary`）：调用模型提供方生成结构化摘要，若存在前次摘要则执行增量合并（UPDATE 模式），
//!    同时自动累积会话中读取与修改的文件列表（`<read-files>` 与 `<modified-files>`）。
//! 4. **持久化落盘**（`SessionManager::append_compaction`）：将生成的 `CompactionEntry` 写入会话文件，
//!    `build_context_entries` 即可基于最新压缩节点快速重建上下文。

use std::collections::BTreeSet;
use std::sync::Arc;

use serde_json::{Value, json};
use singularity_core::CancellationToken;
use singularity_model::{
    ModelError, ModelErrorKind, ModelMessage, ModelPreferences, ModelRole, ModelTurnRequest,
    ModelTurnStatus, ModelUsage, Provider, ProviderError,
};
use thiserror::Error;
use uuid::Uuid;

use crate::agent::{AgentEvents, SendOutcome, TurnRetryConfig, send_with_retry};
use crate::message::{AgentMessage, AgentMessageRole, ContentBlock};
use crate::session::{CompactionEntry, SessionEntry, SessionError, SessionManager};

/// 默认为模型回答预留的 Token 空间：usage 或 fallback 估算超过
/// `context_window - reserve_tokens` 时触发压缩。
pub const DEFAULT_RESERVE_TOKENS: u64 = 16_384;
/// 默认保留的最近上下文 Token 数。
pub const DEFAULT_KEEP_RECENT_TOKENS: u64 = 20_000;
/// 摘要请求的默认最大输出 token 数。
pub const DEFAULT_SUMMARY_MAX_TOKENS: u32 = 8192;
/// 生成摘要时单条工具结果序列化的最大字符数截断上限。
const TOOL_RESULT_MAX_CHARS: usize = 2000;

/// 摘要生成系统指令。
const SUMMARIZATION_SYSTEM_PROMPT: &str = r#"You are a context summarization assistant. Your task is to read a conversation between a user and an AI assistant, then produce a structured summary following the exact format specified.

Do NOT continue the conversation. Do NOT respond to any questions in the conversation. ONLY output the structured summary."#;

/// 首次生成全量结构化摘要时的 Prompt 模板。
const SUMMARIZATION_PROMPT: &str = r#"The messages above are a conversation to summarize. Create a structured context checkpoint summary that another LLM will use to continue the work.

Use this EXACT format:

## Goal
[What is the user trying to accomplish? Can be multiple items if the session covers different tasks.]

## Constraints & Preferences
- [Any constraints, preferences, or requirements mentioned by user]
- [Or "(none)" if none were mentioned]

## Progress
### Done
- [x] [Completed tasks/changes]

### In Progress
- [ ] [Current work]

### Blocked
- [Issues preventing progress, if any]

## Key Decisions
- **[Decision]**: [Brief rationale]

## Next Steps
1. [Ordered list of what should happen next]

## Critical Context
- [Any data, examples, or references needed to continue]
- [Or "(none)" if not applicable]

Keep each section concise. Preserve exact file paths, function names, and error messages."#;

/// 存在前次摘要时执行增量合并的 Prompt 模板。
const UPDATE_SUMMARIZATION_PROMPT: &str = r#"The messages above are NEW conversation messages to incorporate into the existing summary provided in <previous-summary> tags.

Update the existing structured summary with new information. RULES:
- PRESERVE all existing information from the previous summary
- ADD new progress, decisions, and context from the new messages
- UPDATE the Progress section: move items from "In Progress" to "Done" when completed
- UPDATE "Next Steps" based on what was accomplished
- PRESERVE exact file paths, function names, and error messages
- If something is no longer relevant, you may remove it

Use this EXACT format:

## Goal
[Preserve existing goals, add new ones if the task expanded]

## Constraints & Preferences
- [Preserve existing, add new ones discovered]

## Progress
### Done
- [x] [Include previously done items AND newly completed items]

### In Progress
- [ ] [Current work - update based on progress]

### Blocked
- [Current blockers - remove if resolved]

## Key Decisions
- **[Decision]**: [Brief rationale] (preserve all previous, add new)

## Next Steps
1. [Update based on current state]

## Critical Context
- [Preserve important context, add new if needed]

Keep each section concise. Preserve exact file paths, function names, and error messages."#;

/// 超长单轮（Split Turn）前缀摘要的 Prompt 模板。
const TURN_PREFIX_SUMMARIZATION_PROMPT: &str = r#"This is the PREFIX of a turn that was too large to keep. The SUFFIX (recent work) is retained.

Summarize the prefix to provide context for the retained suffix:

## Original Request
[What did the user ask for in this turn?]

## Early Progress
- [Key decisions and work done in the prefix]

## Context for Suffix
- [Information needed to understand the retained recent work]

Be concise. Focus on what's needed to understand the kept suffix."#;

/// 会话无历史消息时的占位摘要文本。
const NO_PRIOR_HISTORY: &str = "No prior history.";

/// 上下文压缩的用户可配置策略。
#[derive(Debug, Clone, PartialEq)]
pub struct CompactionConfig {
    /// 为模型回答预留的 Token 空间；usage 或 fallback 估算超过
    /// `context_window - reserve_tokens` 时在请求前触发压缩。
    pub reserve_tokens: u64,
    pub keep_recent_tokens: u64,
    pub summary_max_tokens: u32,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            reserve_tokens: DEFAULT_RESERVE_TOKENS,
            keep_recent_tokens: DEFAULT_KEEP_RECENT_TOKENS,
            summary_max_tokens: DEFAULT_SUMMARY_MAX_TOKENS,
        }
    }
}

impl CompactionConfig {
    /// 校验压缩策略：`reserve_tokens` 必须小于 `context_window`（为上下文内容
    /// 留出空间），近期保留预算与摘要输出上限必须为正；非法配置 fail closed。
    pub fn validate(&self, context_window: u64, provider_max_output_tokens: u32) -> Result<()> {
        if self.keep_recent_tokens == 0 {
            return Err(CompactionError::Config(
                "keep_recent_tokens must be positive".to_string(),
            ));
        }
        if self.reserve_tokens >= context_window {
            return Err(CompactionError::Config(format!(
                "reserve_tokens must be smaller than the model context window ({context_window})"
            )));
        }
        if self.summary_max_tokens == 0 || self.summary_max_tokens > provider_max_output_tokens {
            return Err(CompactionError::Config(format!(
                "summary_max_tokens must be positive and no greater than provider output limit ({provider_max_output_tokens})"
            )));
        }
        Ok(())
    }
}

/// 一次压缩的运行预算（由模型窗口和压缩策略计算）。
#[derive(Debug, Clone, PartialEq)]
pub struct CompactionBudget {
    pub context_window: u64,
    pub reserve_tokens: u64,
    pub keep_recent_tokens: u64,
}

impl CompactionBudget {
    pub fn from_config(context_window: u64, config: &CompactionConfig) -> Self {
        Self {
            context_window,
            reserve_tokens: config.reserve_tokens,
            keep_recent_tokens: config.keep_recent_tokens,
        }
    }

    fn threshold_tokens(&self) -> u64 {
        self.context_window.saturating_sub(self.reserve_tokens)
    }

    fn retain_tokens(&self) -> u64 {
        self.keep_recent_tokens
    }
}

/// `compact` 入口的结果。
#[derive(Debug, Clone, PartialEq)]
pub enum CompactionOutcome {
    /// 未触发或无可摘要内容。
    NotNeeded,
    Compacted {
        first_kept_entry_id: String,
        tokens_before: u64,
        usage: ModelUsage,
        usage_complete: bool,
    },
}

/// Compaction 错误。
#[derive(Debug, Error)]
pub enum CompactionError {
    #[error("summarization provider error: {0}")]
    Provider(#[from] ProviderError),
    #[error("session error: {0}")]
    Session(#[from] SessionError),
    #[error("{0}")]
    InvalidResponse(String),
    #[error("compaction configuration error: {0}")]
    Config(String),
}

/// `compact` 结果别名。
pub type Result<T> = std::result::Result<T, CompactionError>;

/// 切点查找的内部计算结果（穷尽两态：切点落在条目上，或超长单轮被切开）。
#[derive(Debug, Clone, PartialEq)]
enum CutWindow {
    /// 保留区自该条目起（条目不保证是轮边界；轮起点定位失败时也回落此态）。
    FromEntry { first_kept_entry_index: usize },
    /// 超长单轮被切开：保留区自 `first_kept_entry_index` 起，其所属轮起始于
    /// `turn_start_index`，轮内被切掉的前缀另出摘要。
    SplitTurn {
        turn_start_index: usize,
        first_kept_entry_index: usize,
    },
}

impl CutWindow {
    fn first_kept_entry_index(&self) -> usize {
        match self {
            CutWindow::FromEntry {
                first_kept_entry_index,
            }
            | CutWindow::SplitTurn {
                first_kept_entry_index,
                ..
            } => *first_kept_entry_index,
        }
    }
}

/// 上下文压缩引擎：负责判定触发时机、查找安全切点并调用模型生成结构化摘要。
pub struct CompactionEngine {
    provider: Arc<dyn Provider + Send + Sync>,
    model_preferences: ModelPreferences,
    summary_max_tokens: u32,
    /// 摘要请求与正常采样共用同一 agent 层重试策略。
    retry: TurnRetryConfig,
}

impl CompactionEngine {
    /// 创建压缩引擎实例，默认使用标准模型偏好与默认保留 Token 预算。
    pub fn new(provider: Arc<dyn Provider + Send + Sync>) -> Self {
        Self {
            provider,
            model_preferences: ModelPreferences::default(),
            summary_max_tokens: DEFAULT_SUMMARY_MAX_TOKENS,
            retry: TurnRetryConfig::default(),
        }
    }

    /// 绑定摘要请求的模型偏好配置（如模型名称、温度等）。
    pub fn with_model_preferences(mut self, preferences: ModelPreferences) -> Self {
        self.model_preferences = preferences;
        self
    }

    /// 绑定摘要生成的独立输出上限。
    pub fn with_summary_max_tokens(mut self, summary_max_tokens: u32) -> Self {
        self.summary_max_tokens = summary_max_tokens;
        self
    }

    /// 绑定摘要请求的重试策略（与正常采样同源，由 `Agent::new` 注入）。
    pub fn with_retry(mut self, retry: TurnRetryConfig) -> Self {
        self.retry = retry;
        self
    }

    /// 判定是否应当触发压缩：上下文估算超过「窗口 − reserve_tokens 预留」时触发。
    /// 恰好等于阈值不触发。
    pub fn should_compact(&self, context_tokens: u64, budget: &CompactionBudget) -> bool {
        context_tokens > budget.threshold_tokens()
    }

    /// 在给定的会话条目列表中查找安全切点，返回保留区域起始条目的索引。
    /// 若条目为空则返回 `None`。
    #[cfg(test)]
    fn find_cut_point(&self, entries: &[SessionEntry], budget: &CompactionBudget) -> Option<usize> {
        if entries.is_empty() {
            return None;
        }
        Some(
            self.find_cut_point_in_range(entries, 0, entries.len(), budget.retain_tokens())
                .first_kept_entry_index(),
        )
    }

    /// 估算文本的 Token 消耗：按字符编码启发式估算（`ceil(UTF-16 字符数 / 4)`）。
    pub fn estimate_tokens(&self, text: &str) -> u64 {
        estimate_tokens_of(text)
    }

    /// 将消息列表序列化为适合输入给摘要模型的纯文本对话格式。
    ///
    /// 为不同角色标注 `[User]`、`[Assistant]`、`[Assistant tool calls]`、`[Tool result]`；
    /// 工具返回结果单条超过上限时执行截断并追加截断标记。
    pub fn serialize_conversation(&self, messages: &[AgentMessage]) -> String {
        let mut parts = Vec::new();
        for message in messages {
            let text = message.content_text();
            match message.role() {
                AgentMessageRole::User => {
                    if !text.is_empty() {
                        parts.push(format!("[User]: {text}"));
                    }
                }
                AgentMessageRole::Assistant => {
                    if !text.is_empty() {
                        parts.push(format!("[Assistant]: {text}"));
                    }
                    for block in message.tool_calls() {
                        if let ContentBlock::ToolCall { name, args, .. } = block {
                            parts.push(format!(
                                "[Assistant tool calls]: {name}({})",
                                format_tool_call_args(args)
                            ));
                        }
                    }
                }
                AgentMessageRole::ToolResult => {
                    if !text.is_empty() {
                        parts.push(format!(
                            "[Tool result]: {}",
                            truncate_for_summary(&text, TOOL_RESULT_MAX_CHARS)
                        ));
                    }
                }
            }
        }
        parts.join("\n\n")
    }

    /// 调用模型生成或更新会话结构化摘要。
    ///
    /// 对话内容使用 `<conversation>` 标签包裹；若存在历史摘要，则把其文本放入
    /// `<previous-summary>` 标签并使用 UPDATE 模板引导模型进行增量合并。
    /// 支持通过取消信号提前终止。
    fn generate_summary(
        &self,
        conversation: &str,
        previous_text: Option<&str>,
        cancellation: &CancellationToken,
    ) -> Result<SummaryResponse> {
        let mut prompt = format!("<conversation>\n{conversation}\n</conversation>\n\n");
        if let Some(previous_text) = previous_text {
            prompt.push_str(&format!(
                "<previous-summary>\n{previous_text}\n</previous-summary>\n\n"
            ));
        }
        prompt.push_str(if previous_text.is_some() {
            UPDATE_SUMMARIZATION_PROMPT
        } else {
            SUMMARIZATION_PROMPT
        });
        self.complete_summarization(&prompt, "summarization failed", cancellation)
    }

    /// 基于给定会话与预算执行压缩。阈值判定（`should_compact`）由调用方在
    /// 进入前完成；此处只负责切点查找、摘要生成与压缩条目落盘。调用方须传入
    /// 压缩前重建上下文的真实估算值。
    pub fn compact(
        &mut self,
        session: &mut SessionManager,
        budget: &CompactionBudget,
        tokens_before: u64,
        cancellation: &CancellationToken,
    ) -> Result<CompactionOutcome> {
        let entries = session.build_context_entries()?;
        if entries.is_empty() {
            return Ok(CompactionOutcome::NotNeeded);
        }
        // 若最新条目已是压缩节点，则说明尚无新的未压缩内容。
        if session.entries().last().map(|entry| entry.id()) == Some(entries[0].id())
            && matches!(entries[0], SessionEntry::Compaction { .. })
        {
            return Ok(CompactionOutcome::NotNeeded);
        }
        // 二次压缩起点：定位前次压缩节点记录的 first_kept_entry_id。
        let boundary_start = match &entries[0] {
            SessionEntry::Compaction { compaction, .. } => match &compaction.first_kept_entry_id {
                Some(first_kept) => entries
                    .iter()
                    .position(|entry| entry.id() == first_kept)
                    .unwrap_or(1),
                None => 1,
            },
            _ => 0,
        };
        let cut = self.find_cut_point_in_range(
            &entries,
            boundary_start,
            entries.len(),
            budget.retain_tokens(),
        );
        let (history_end, turn_prefix_range) = match &cut {
            CutWindow::FromEntry {
                first_kept_entry_index,
            } => (*first_kept_entry_index, None),
            CutWindow::SplitTurn {
                turn_start_index,
                first_kept_entry_index,
            } => (
                *turn_start_index,
                Some((*turn_start_index, *first_kept_entry_index)),
            ),
        };
        let messages_to_summarize: Vec<AgentMessage> = entries[boundary_start..history_end]
            .iter()
            .filter_map(message_from_entry)
            .cloned()
            .collect();
        let turn_prefix_messages: Vec<AgentMessage> = match turn_prefix_range {
            Some((start, end)) => entries[start..end]
                .iter()
                .filter_map(message_from_entry)
                .cloned()
                .collect(),
            None => Vec::new(),
        };
        if messages_to_summarize.is_empty() && turn_prefix_messages.is_empty() {
            return Ok(CompactionOutcome::NotNeeded);
        }
        let first_kept_entry_id = entries[cut.first_kept_entry_index()].id().to_string();
        // 从被压缩消息和上一代摘要的 details 中累积文件读取与修改清单。
        let mut file_ops = FileOps::default();
        let previous_text = match &entries[0] {
            SessionEntry::Compaction { compaction, .. } => {
                let (read_files, modified_files) =
                    file_lists_from_details(compaction.details.as_ref());
                for file in read_files {
                    file_ops.read.insert(file);
                }
                for file in modified_files {
                    file_ops.edited.insert(file);
                }
                Some(compaction.summary.clone())
            }
            _ => None,
        };
        for message in messages_to_summarize
            .iter()
            .chain(turn_prefix_messages.iter())
        {
            extract_file_ops_from_message(message, &mut file_ops);
        }
        let (read_files, modified_files) = compute_file_lists(&file_ops);

        let mut summary_usage = ModelUsage::default();
        let mut summary_usage_complete = true;
        // 历史摘要只写一份（无历史时以占位文本表达）；split-turn 前缀摘要
        // 是其后可选的一个追加步骤，不复制第二份摘要调用流程。
        let summary_text = if messages_to_summarize.is_empty() {
            NO_PRIOR_HISTORY.to_string()
        } else {
            let summary = self.generate_summary(
                &self.serialize_conversation(&messages_to_summarize),
                previous_text.as_deref(),
                cancellation,
            )?;
            summary_usage.merge(&summary.usage);
            summary_usage_complete &= summary.usage_complete;
            summary.text
        };
        let mut summary_text =
            if matches!(cut, CutWindow::SplitTurn { .. }) && !turn_prefix_messages.is_empty() {
                let prefix = self.generate_turn_prefix_summary(
                    &self.serialize_conversation(&turn_prefix_messages),
                    cancellation,
                )?;
                summary_usage.merge(&prefix.usage);
                summary_usage_complete &= prefix.usage_complete;
                format!(
                    "{summary_text}\n\n---\n\n**Turn Context (split turn):**\n\n{}",
                    prefix.text
                )
            } else {
                summary_text
            };
        summary_text.push_str(&format_file_operations(&read_files, &modified_files));

        let entry = CompactionEntry {
            summary: summary_text,
            first_kept_entry_id: Some(first_kept_entry_id.clone()),
            tokens_before: Some(tokens_before),
            usage: summary_usage.usage_present.then_some(summary_usage.clone()),
            details: Some(json!({
                "readFiles": read_files,
                "modifiedFiles": modified_files,
            })),
        };
        session.append_compaction(entry)?;
        Ok(CompactionOutcome::Compacted {
            first_kept_entry_id,
            tokens_before,
            usage: summary_usage,
            usage_complete: summary_usage_complete,
        })
    }

    /// 在指定范围内查找安全切点。
    fn find_cut_point_in_range(
        &self,
        entries: &[SessionEntry],
        start_index: usize,
        end_index: usize,
        keep_recent_tokens: u64,
    ) -> CutWindow {
        let cut_points: Vec<usize> = (start_index..end_index)
            .filter(|&index| is_cut_point_entry(&entries[index]))
            .collect();
        if cut_points.is_empty() {
            return CutWindow::FromEntry {
                first_kept_entry_index: start_index,
            };
        }
        // 从最新条目向后回溯累加 Token 估算值，达到保留预算时选择 >= 当前条目的最近合法切点。
        // 切点绝不切在 ToolResult 上（ToolResult 必须紧随其 ToolCall 保持在同一侧）；
        // 尾部 ToolResult 自身跨过保留预算且其后无合法切点时，回退到所属轮次起点，
        // 完整保留当前轮并摘要更早全部历史。
        let mut accumulated_tokens = 0u64;
        let mut cut_index = cut_points[0];
        for index in (start_index..end_index).rev() {
            let message_tokens = entry_token_estimate(&entries[index]);
            if message_tokens == 0 {
                continue;
            }
            accumulated_tokens += message_tokens;
            if accumulated_tokens >= keep_recent_tokens {
                cut_index = match cut_points.iter().find(|&&cut| cut >= index) {
                    Some(&next) => next,
                    None => {
                        find_turn_start_index(entries, index, start_index).unwrap_or(start_index)
                    }
                };
                break;
            }
        }
        // 向前回溯吸收不影响会话语义的相邻元数据条目。
        while cut_index > start_index {
            let previous = &entries[cut_index - 1];
            if matches!(previous, SessionEntry::Compaction { .. })
                || matches!(previous, SessionEntry::Message { .. })
            {
                break;
            }
            cut_index -= 1;
        }
        let starts_turn = is_turn_start_entry(&entries[cut_index]);
        if starts_turn {
            CutWindow::FromEntry {
                first_kept_entry_index: cut_index,
            }
        } else {
            match find_turn_start_index(entries, cut_index, start_index) {
                Some(turn_start_index) => CutWindow::SplitTurn {
                    turn_start_index,
                    first_kept_entry_index: cut_index,
                },
                None => CutWindow::FromEntry {
                    first_kept_entry_index: cut_index,
                },
            }
        }
    }

    /// 生成超长单轮（Split Turn）前缀摘要。
    fn generate_turn_prefix_summary(
        &self,
        conversation: &str,
        cancellation: &CancellationToken,
    ) -> Result<SummaryResponse> {
        let prompt = format!(
            "<conversation>\n{conversation}\n</conversation>\n\n{TURN_PREFIX_SUMMARIZATION_PROMPT}"
        );
        self.complete_summarization(&prompt, "turn prefix summarization failed", cancellation)
    }

    fn summary_max_output_tokens(&self) -> u32 {
        self.summary_max_tokens
            .min(self.provider.protocol_contract().max_output_tokens)
            .min(
                self.model_preferences
                    .max_output_tokens
                    .unwrap_or(self.summary_max_tokens),
            )
    }

    /// 执行摘要模型的具体补全调用，处理安全预算与错误映射。
    fn complete_summarization(
        &self,
        prompt_text: &str,
        error_prefix: &str,
        cancellation: &CancellationToken,
    ) -> Result<SummaryResponse> {
        let cap = self.summary_max_output_tokens();
        let contract = self.provider.protocol_contract();
        let prompt_tokens = estimate_tokens_of(prompt_text)
            + estimate_tokens_of(SUMMARIZATION_SYSTEM_PROMPT)
            + cap as u64;
        if contract
            .max_context_tokens
            .is_some_and(|window| prompt_tokens > window as u64)
        {
            return Err(CompactionError::InvalidResponse(format!(
                "{error_prefix}: summary request exceeds provider context window"
            )));
        }
        let mut preferences = self.model_preferences.clone();
        preferences.max_output_tokens = Some(cap);
        let mut request = ModelTurnRequest::new(
            format!("compaction-{}", Uuid::now_v7()),
            crate::agent::instruction_message(SUMMARIZATION_SYSTEM_PROMPT)
                .into_iter()
                .chain(std::iter::once(ModelMessage::text(
                    ModelRole::User,
                    prompt_text,
                )))
                .collect(),
        );
        request.model_preferences = preferences;
        // 摘要请求与正常采样经同一 helper 复用同一传输策略：可重试错误
        // 指数退避重试；摘要请求没有事件出口，重试诊断在此路径不投影。
        let mut summary_events = AgentEvents::new();
        let response = match send_with_retry(
            |_events| self.provider.complete(&request, cancellation),
            self.retry,
            &mut summary_events,
            cancellation,
        ) {
            SendOutcome::Response(response) => *response,
            // 退避等待被取消与 provider 取消请求同形收敛。
            SendOutcome::Aborted => {
                return Err(CompactionError::Provider(ProviderError::from_model_error(
                    ModelError::new(ModelErrorKind::Cancelled, "provider request cancelled"),
                )));
            }
            SendOutcome::Failed(error) => return Err(CompactionError::Provider(error)),
        };
        if response.status != ModelTurnStatus::Success {
            let detail = response
                .error
                .as_ref()
                .map(|error| error.message.as_str())
                .unwrap_or("unknown provider error");
            return Err(CompactionError::InvalidResponse(format!(
                "{error_prefix}: {detail}"
            )));
        }
        let usage_complete = response.usage.usage_present;
        let usage = response.usage.clone();
        let text = response
            .assistant_message
            .map(|message| message.content)
            .ok_or_else(|| {
                CompactionError::InvalidResponse(format!(
                    "{error_prefix}: missing assistant message"
                ))
            })?;
        Ok(SummaryResponse {
            text,
            usage,
            usage_complete,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
struct SummaryResponse {
    text: String,
    usage: ModelUsage,
    usage_complete: bool,
}

/// 基于 UTF-16 字符数的启发式 Token 估算函数（`ceil(chars / 4)`）。
fn estimate_tokens_of(text: &str) -> u64 {
    let chars = text.encode_utf16().count() as u64;
    chars.div_ceil(4)
}

/// 估算单条会话条目贡献的 Token 数量：文本、工具调用（id/name/args）、
/// thinking 块与 provider reasoning replay 全部计入。
pub(crate) fn entry_token_estimate(entry: &SessionEntry) -> u64 {
    match entry {
        SessionEntry::Message { message, .. } => message_token_estimate(message),
        SessionEntry::Compaction { compaction, .. } => estimate_tokens_of(&compaction.summary),
        SessionEntry::Metadata { .. } => 0,
    }
}

fn message_token_estimate(message: &AgentMessage) -> u64 {
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

/// 请求前上下文规模的唯一计量（参照 pi `estimateContextTokens = usageTokens +
/// trailingTokens`）：provider 最后上报的 usage 与上报之后新增条目的估算合成。
///
/// usage 基线缺失时（首轮、压缩重写后）返回 `None`，调用方以完整装配估算兜底；
/// 装配固定项（tools schema、reasoning replay、输出预算与固定余量）属于估算
/// 函数族（`estimate_assembled`/`entry_token_estimate`），ledger 只组合
/// "usage 基线 + 尾部增量"。
pub(crate) struct ContextLedger {
    /// provider 最后上报的上下文 token 数（请求发出时的真实占用）。
    last_reported_tokens: Option<u64>,
    /// 上报之后追加到会话的条目的 token 估算（assistant 消息、toolResult 等）。
    trailing_estimate: u64,
}

impl ContextLedger {
    pub(crate) fn new() -> Self {
        Self {
            last_reported_tokens: None,
            trailing_estimate: 0,
        }
    }

    /// 记录 provider 上报的 usage：尾部增量归零（本轮追加的条目从下一轮起入账）。
    pub(crate) fn record_usage(&mut self, usage: &ModelUsage) {
        if usage.usage_present {
            self.last_reported_tokens = Some(usage.total_tokens);
            self.trailing_estimate = 0;
        }
    }

    /// 上报之后追加到会话的条目入账。
    pub(crate) fn record_appended(&mut self, entry: &SessionEntry) {
        self.trailing_estimate = self
            .trailing_estimate
            .saturating_add(entry_token_estimate(entry));
    }

    /// 压缩重写会话后 usage 基线作废：回退到装配估算兜底。
    pub(crate) fn invalidate(&mut self) {
        self.last_reported_tokens = None;
        self.trailing_estimate = 0;
    }

    /// 唯一计量 = provider usage + 尾部增量；无 usage 基线时返回 `None`。
    pub(crate) fn estimate(&self) -> Option<u64> {
        Some(
            self.last_reported_tokens?
                .saturating_add(self.trailing_estimate),
        )
    }
}

/// 判断某条目是否为合法的压缩切点（除 ToolResult 之外的消息均可作为切点）。
fn is_cut_point_entry(entry: &SessionEntry) -> bool {
    match entry {
        SessionEntry::Message { message, .. } => {
            !matches!(message.role(), AgentMessageRole::ToolResult)
        }
        _ => false,
    }
}

/// 判断某条目是否为新轮次的起始（User 角色消息）。
fn is_turn_start_entry(entry: &SessionEntry) -> bool {
    match entry {
        SessionEntry::Message { message, .. } => matches!(message.role(), AgentMessageRole::User),
        _ => false,
    }
}

/// 在指定条目及之前查找所属轮次的起始 User 消息索引。
fn find_turn_start_index(
    entries: &[SessionEntry],
    entry_index: usize,
    start_index: usize,
) -> Option<usize> {
    (start_index..=entry_index)
        .rev()
        .find(|&index| is_turn_start_entry(&entries[index]))
}

/// 从会话条目中提取消息引用（若非消息类型则返回 None）。
fn message_from_entry(entry: &SessionEntry) -> Option<&AgentMessage> {
    match entry {
        SessionEntry::Message { message, .. } => Some(message),
        _ => None,
    }
}

/// 获取字符串的 UTF-16 代码单元长度。
fn utf16_len(text: &str) -> usize {
    text.encode_utf16().count()
}

/// 对文本进行定长截断并追加截断字符数说明。
fn truncate_for_summary(text: &str, max_chars: usize) -> String {
    let total = utf16_len(text);
    if total <= max_chars {
        return text.to_string();
    }
    let mut kept_units = 0;
    let mut cut = 0;
    for (index, ch) in text.char_indices() {
        let units = ch.len_utf16();
        if kept_units + units > max_chars {
            cut = index;
            break;
        }
        kept_units += units;
        if kept_units == max_chars {
            cut = index + ch.len_utf8();
            break;
        }
    }
    format!(
        "{}\n\n[... {} more characters truncated]",
        &text[..cut],
        total - max_chars
    )
}

/// 将工具调用参数格式化为可读的键值对参数列表文本。
fn format_tool_call_args(args: &Value) -> String {
    let Some(object) = args.as_object() else {
        return String::new();
    };
    object
        .iter()
        .map(|(key, value)| format!("{key}={}", serde_json::to_string(value).unwrap_or_default()))
        .collect::<Vec<_>>()
        .join(", ")
}

/// 文件操作路径累积集合。
#[derive(Default)]
struct FileOps {
    read: BTreeSet<String>,
    written: BTreeSet<String>,
    edited: BTreeSet<String>,
}

/// 从 Assistant 消息的 ToolCall 内容块中提取文件操作路径。
fn extract_file_ops_from_message(message: &AgentMessage, file_ops: &mut FileOps) {
    if message.role() != AgentMessageRole::Assistant {
        return;
    }
    for block in message.tool_calls() {
        let ContentBlock::ToolCall { name, args, .. } = block else {
            continue;
        };
        let Some(path) = args.get("path").and_then(Value::as_str) else {
            continue;
        };
        match name.as_str() {
            "read" => {
                file_ops.read.insert(path.to_string());
            }
            "write" => {
                file_ops.written.insert(path.to_string());
            }
            "edit" => {
                file_ops.edited.insert(path.to_string());
            }
            _ => {}
        }
    }
}

/// 根据操作集合计算最终读取与修改的文件列表（修改列表为 edited ∪ written；读取列表剔除已修改文件）。
fn compute_file_lists(file_ops: &FileOps) -> (Vec<String>, Vec<String>) {
    let modified: BTreeSet<String> = file_ops
        .edited
        .iter()
        .chain(file_ops.written.iter())
        .cloned()
        .collect();
    let read_files: Vec<String> = file_ops
        .read
        .iter()
        .filter(|file| !modified.contains(*file))
        .cloned()
        .collect();
    (read_files, modified.into_iter().collect())
}

/// 将文件列表格式化为 XML 标签块（`<read-files>` 与 `<modified-files>`）。
fn format_file_operations(read_files: &[String], modified_files: &[String]) -> String {
    let mut sections = Vec::new();
    if !read_files.is_empty() {
        sections.push(format!(
            "<read-files>\n{}\n</read-files>",
            read_files.join("\n")
        ));
    }
    if !modified_files.is_empty() {
        sections.push(format!(
            "<modified-files>\n{}\n</modified-files>",
            modified_files.join("\n")
        ));
    }
    if sections.is_empty() {
        return String::new();
    }
    format!("\n\n{}", sections.join("\n\n"))
}

/// 从会话压缩条目的 details 元数据中解析读取与修改的文件列表。
fn file_lists_from_details(details: Option<&Value>) -> (Vec<String>, Vec<String>) {
    let read_files = details
        .and_then(|details| details.get("readFiles"))
        .and_then(Value::as_array)
        .map(|files| {
            files
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let modified_files = details
        .and_then(|details| details.get("modifiedFiles"))
        .and_then(Value::as_array)
        .map(|files| {
            files
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    (read_files, modified_files)
}

#[cfg(test)]
#[path = "compaction_tests.rs"]
mod tests;
