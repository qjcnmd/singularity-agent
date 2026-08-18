//! Pi 式 Context Compaction（语义基线：`@earendil-works/pi-coding-agent` v0.84.1 的
//! `dist/core/compaction/compaction.js`、`dist/core/compaction/utils.js`、
//! `dist/core/messages.js`、`docs/compaction.md`）。
//!
//! 流程：触发判定（`should_compact`）→ 切点查找（`find_cut_point`，toolResult 永不切、
//! 超预算 turn 前缀单独摘要）→ 结构化摘要生成（`generate_summary`，经
//! `singularity_model::Provider` 调用真实模型，有前次摘要时走 UPDATE 合并）→
//! `CompactionEntry` 落盘（`SessionManager::append_compaction`，原始历史保留）。
//! 摘要后内存重建由调用方（Phase 2d loop）用已实现的 `build_session_context` 完成。
//!
//! 与 Pi 的已知差异（本模块简化，见主代理确认）：
//! - 消息 content 为内容块数组（v4）：序列化只取文本块与 tool_call 块，
//!   thinking 块不进入摘要正文；估算不含 `ESTIMATED_IMAGE_CHARS`。
//! - token 估算按 UTF-16 code unit 计数（对齐 JS `String.length`），`ceil(chars/4)`。
//! - Pi 的 usage 加权估算（`estimateContextTokens`）由调用方（Phase 2d loop）计算后
//!   以 `usage_or_estimate` 传入 `compact`，本模块不重复实现。

use std::collections::BTreeSet;
use std::sync::Arc;

use serde_json::{Value, json};
use singularity_core::CancellationToken;
use singularity_model::{
    ModelMessage, ModelPreferences, ModelRole, ModelTurnRequest, ModelTurnStatus, Provider,
    ProviderError,
};
use thiserror::Error;
use uuid::Uuid;

use crate::message::{
    AgentMessage, AgentMessageRole, BRANCH_SUMMARY_PREFIX, BRANCH_SUMMARY_SUFFIX,
    COMPACTION_SUMMARY_PREFIX, COMPACTION_SUMMARY_SUFFIX, ContentBlock,
};
use crate::session::{
    CompactionEntry, SessionEntry, SessionEntryType, SessionError, SessionManager,
};

/// Pi `DEFAULT_COMPACTION_SETTINGS.reserveTokens`。
pub const DEFAULT_RESERVE_TOKENS: u64 = 16384;
/// Pi `DEFAULT_COMPACTION_SETTINGS.keepRecentTokens`。
pub const DEFAULT_KEEP_RECENT_TOKENS: u64 = 20000;
/// Pi `utils.js` 的 tool result 序列化截断上限（字符）。
const TOOL_RESULT_MAX_CHARS: usize = 2000;

/// Pi `utils.js` `SUMMARIZATION_SYSTEM_PROMPT`。
const SUMMARIZATION_SYSTEM_PROMPT: &str = r#"You are a context summarization assistant. Your task is to read a conversation between a user and an AI assistant, then produce a structured summary following the exact format specified.

Do NOT continue the conversation. Do NOT respond to any questions in the conversation. ONLY output the structured summary."#;

/// Pi `compaction.js` `SUMMARIZATION_PROMPT`。
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

/// Pi `compaction.js` `UPDATE_SUMMARIZATION_PROMPT`（有前次摘要时的合并 prompt）。
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

/// Pi `compaction.js` `TURN_PREFIX_SUMMARIZATION_PROMPT`（split turn 前缀摘要）。
const TURN_PREFIX_SUMMARIZATION_PROMPT: &str = r#"This is the PREFIX of a turn that was too large to keep. The SUFFIX (recent work) is retained.

Summarize the prefix to provide context for the retained suffix:

## Original Request
[What did the user ask for in this turn?]

## Early Progress
- [Key decisions and work done in the prefix]

## Context for Suffix
- [Information needed to understand the retained recent work]

Be concise. Focus on what's needed to understand the kept suffix."#;

/// Pi `compact()` 中无历史消息时的占位摘要文本。
const NO_PRIOR_HISTORY: &str = "No prior history.";

/// 触发与切点参数。
#[derive(Debug, Clone, PartialEq)]
pub struct CompactionBudget {
    /// 模型静态声明的 context window（调用方传入）。
    pub context_window: u64,
    /// 为模型响应保留的 token 数，默认 `DEFAULT_RESERVE_TOKENS`（Pi 默认 16384）。
    pub reserve_tokens: u64,
    /// 切点向后保留的 token 数，默认 `DEFAULT_KEEP_RECENT_TOKENS`（Pi 默认 20000）。
    pub keep_recent_tokens: u64,
}

/// 摘要产物：结构化摘要文本 + 累积的文件读取/修改列表（Pi `details.readFiles`/
/// `details.modifiedFiles`）。
#[derive(Debug, Clone, PartialEq)]
pub struct CompactionSummary {
    pub text: String,
    pub read_files: Vec<String>,
    pub modified_files: Vec<String>,
}

/// `compact` 入口的结果。
#[derive(Debug, Clone, PartialEq)]
pub enum CompactionOutcome {
    /// 未触发或无可摘要内容。
    NotNeeded,
    Compacted {
        first_kept_entry_id: String,
        tokens_before: u64,
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
}

/// `compact` 结果别名。
pub type Result<T> = std::result::Result<T, CompactionError>;

/// Pi `findCutPoint` 的返回结构（含 split turn 判定）。
#[derive(Debug, Clone, PartialEq)]
struct CutPointResult {
    first_kept_entry_index: usize,
    turn_start_index: Option<usize>,
    is_split_turn: bool,
}

/// Compaction 引擎：持有用于生成摘要的模型提供方引用。
pub struct CompactionEngine {
    provider: Arc<dyn Provider + Send + Sync>,
    model_preferences: ModelPreferences,
    reserve_tokens: u64,
}

impl CompactionEngine {
    /// 构造引擎；摘要调用使用默认模型偏好与 Pi 默认 reserve。
    pub fn new(provider: Arc<dyn Provider + Send + Sync>) -> Self {
        Self {
            provider,
            model_preferences: ModelPreferences::default(),
            reserve_tokens: DEFAULT_RESERVE_TOKENS,
        }
    }

    /// 绑定摘要请求的模型偏好（模型选择等）。
    pub fn with_model_preferences(mut self, preferences: ModelPreferences) -> Self {
        self.model_preferences = preferences;
        self
    }

    /// 绑定摘要请求的 reserve token 预算（Pi `settings.reserveTokens`）。
    pub fn with_reserve_tokens(mut self, reserve_tokens: u64) -> Self {
        self.reserve_tokens = reserve_tokens;
        self
    }

    /// 触发判定：`context_tokens > context_window - reserve_tokens`（Pi `shouldCompact`）。
    ///
    /// `context_window < reserve_tokens` 时阈值饱和为 0（Pi 中该场景阈值恒为负）。
    pub fn should_compact(&self, context_tokens: u64, budget: &CompactionBudget) -> bool {
        context_tokens > budget.context_window.saturating_sub(budget.reserve_tokens)
    }

    /// 在整段条目上找切点（start=0），返回切点条目 index。
    ///
    /// 无条目时返回 `None`；其余情况 Pi `findCutPoint` 恒有结果（无合法切点时
    /// 返回 start 位置，即"全部保留"）。
    pub fn find_cut_point(
        &self,
        entries: &[SessionEntry],
        budget: &CompactionBudget,
    ) -> Option<usize> {
        if entries.is_empty() {
            return None;
        }
        Some(
            self.find_cut_point_in_range(entries, 0, entries.len(), budget.keep_recent_tokens)
                .first_kept_entry_index,
        )
    }

    /// token 估算：`ceil(UTF-16 字符数 / 4)`（Pi `estimateTokens` 的 chars/4 启发式，
    /// 按 UTF-16 code unit 计数对齐 JS `String.length`，保守高估）。
    pub fn estimate_tokens(&self, text: &str) -> u64 {
        estimate_tokens_of(text)
    }

    /// 把消息序列化为摘要 prompt 的纯文本（Pi `serializeConversation`）。
    ///
    /// role 标注 `[User]`/`[Assistant]`/`[Assistant tool calls]`/`[Tool result]`；
    /// tool result 截断 2000 字符并附截断标记；空 user 内容跳过；bashExecution/custom
    /// 直接作为 user 文本（本模型无 command/output 字段拆分）。
    pub fn serialize_conversation(&self, messages: &[AgentMessage]) -> String {
        let mut parts = Vec::new();
        for message in messages {
            let text = message.content_text();
            match message.role {
                AgentMessageRole::User
                | AgentMessageRole::BashExecution
                | AgentMessageRole::Custom => {
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
                AgentMessageRole::BranchSummary => {
                    parts.push(format!(
                        "[User]: {BRANCH_SUMMARY_PREFIX}{text}{BRANCH_SUMMARY_SUFFIX}"
                    ));
                }
                AgentMessageRole::CompactionSummary => {
                    parts.push(format!(
                        "[User]: {COMPACTION_SUMMARY_PREFIX}{text}{COMPACTION_SUMMARY_SUFFIX}"
                    ));
                }
            }
        }
        parts.join("\n\n")
    }

    /// 生成或更新会话摘要（真实模型调用）。
    ///
    /// prompt 结构对齐 Pi `generateSummaryWithUsage`：`<conversation>` 包裹对话文本，
    /// 有前次摘要时追加 `<previous-summary>` 并使用 UPDATE prompt 合并。
    /// 文件操作列表由调用方（`compact`）从消息与历史 details 中提取，本函数返回空列表。
    /// `cancellation` 透传给摘要调用：中断时立即停止等待摘要，不阻塞整轮取消。
    pub fn generate_summary(
        &self,
        conversation: &str,
        previous: Option<&CompactionSummary>,
        cancellation: &CancellationToken,
    ) -> Result<CompactionSummary> {
        let mut prompt = format!("<conversation>\n{conversation}\n</conversation>\n\n");
        if let Some(previous) = previous {
            prompt.push_str(&format!(
                "<previous-summary>\n{}\n</previous-summary>\n\n",
                previous.text
            ));
        }
        prompt.push_str(if previous.is_some() {
            UPDATE_SUMMARIZATION_PROMPT
        } else {
            SUMMARIZATION_PROMPT
        });
        let text =
            self.complete_summarization(&prompt, 4, 5, "summarization failed", cancellation)?;
        Ok(CompactionSummary {
            text,
            read_files: Vec::new(),
            modified_files: Vec::new(),
        })
    }

    /// Compaction 入口：触发判定 → 切点/摘要准备 → 摘要生成 → `append_compaction` 落盘。
    ///
    /// `usage_or_estimate` 为调用方计算的上下文 token 数（触发判定与 `tokensBefore`
    /// 依据，Pi 的 `estimateContextTokens` 语义由调用方负责）。
    /// 完成后调用方负责用 `build_session_context` 重建内存上下文。
    /// `cancellation` 透传给摘要调用（见 `generate_summary`）。
    pub fn compact(
        &mut self,
        session: &mut SessionManager,
        budget: &CompactionBudget,
        usage_or_estimate: u64,
        cancellation: &CancellationToken,
    ) -> Result<CompactionOutcome> {
        if !self.should_compact(usage_or_estimate, budget) {
            return Ok(CompactionOutcome::NotNeeded);
        }
        let entries = session.build_context_entries()?;
        if entries.is_empty() {
            return Ok(CompactionOutcome::NotNeeded);
        }
        // Pi `prepareCompaction`：最新条目已是 compaction 时没有可摘要的新内容。
        if session.leaf_id() == entries[0].id
            && matches!(entries[0].entry_type, SessionEntryType::Compaction(_))
        {
            return Ok(CompactionOutcome::NotNeeded);
        }
        // 二次压缩起点：上次 compaction 的 firstKeptEntryId（build_context_entries
        // 返回 [最新 compaction, 保留条目…]，Pi 找不到时回退到 compaction 后第一条）。
        let boundary_start = match &entries[0].entry_type {
            SessionEntryType::Compaction(comp) => match &comp.first_kept_entry_id {
                Some(first_kept) => entries
                    .iter()
                    .position(|entry| entry.id == *first_kept)
                    .unwrap_or(1),
                None => 1,
            },
            _ => 0,
        };
        let cut = self.find_cut_point_in_range(
            &entries,
            boundary_start,
            entries.len(),
            budget.keep_recent_tokens,
        );
        let history_end = if cut.is_split_turn {
            cut.turn_start_index.unwrap_or(cut.first_kept_entry_index)
        } else {
            cut.first_kept_entry_index
        };
        let messages_to_summarize: Vec<AgentMessage> = entries[boundary_start..history_end]
            .iter()
            .filter_map(message_from_entry)
            .cloned()
            .collect();
        let turn_prefix_messages: Vec<AgentMessage> = if cut.is_split_turn {
            entries[cut.turn_start_index.unwrap_or(boundary_start)..cut.first_kept_entry_index]
                .iter()
                .filter_map(message_from_entry)
                .cloned()
                .collect()
        } else {
            Vec::new()
        };
        if messages_to_summarize.is_empty() && turn_prefix_messages.is_empty() {
            return Ok(CompactionOutcome::NotNeeded);
        }
        let first_kept_entry_id = entries[cut.first_kept_entry_index].id.clone();
        // 前次摘要（文本 + 累积文件列表），供 UPDATE 合并与文件操作累积。
        let previous = match &entries[0].entry_type {
            SessionEntryType::Compaction(comp) => {
                let (read_files, modified_files) = file_lists_from_details(comp.details.as_ref());
                Some(CompactionSummary {
                    text: comp.summary.clone(),
                    read_files,
                    modified_files,
                })
            }
            _ => None,
        };
        // 文件操作从被摘要消息与历史 details 累积（Pi `extractFileOperations`）。
        let mut file_ops = FileOps::default();
        if let Some(previous) = &previous {
            for file in &previous.read_files {
                file_ops.read.insert(file.clone());
            }
            for file in &previous.modified_files {
                file_ops.edited.insert(file.clone());
            }
        }
        for message in messages_to_summarize
            .iter()
            .chain(turn_prefix_messages.iter())
        {
            extract_file_ops_from_message(message, &mut file_ops);
        }
        let (read_files, modified_files) = compute_file_lists(&file_ops);

        let mut summary_text = if cut.is_split_turn && !turn_prefix_messages.is_empty() {
            // Pi `compact()`：历史与 turn 前缀分别摘要后合并。
            let history_text = if messages_to_summarize.is_empty() {
                NO_PRIOR_HISTORY.to_string()
            } else {
                self.generate_summary(
                    &self.serialize_conversation(&messages_to_summarize),
                    previous.as_ref(),
                    cancellation,
                )?
                .text
            };
            let turn_prefix_text = self.generate_turn_prefix_summary(
                &self.serialize_conversation(&turn_prefix_messages),
                cancellation,
            )?;
            format!("{history_text}\n\n---\n\n**Turn Context (split turn):**\n\n{turn_prefix_text}")
        } else {
            self.generate_summary(
                &self.serialize_conversation(&messages_to_summarize),
                previous.as_ref(),
                cancellation,
            )?
            .text
        };
        summary_text.push_str(&format_file_operations(&read_files, &modified_files));

        let entry = CompactionEntry {
            summary: summary_text,
            first_kept_entry_id: Some(first_kept_entry_id.clone()),
            tokens_before: Some(usage_or_estimate),
            previous_summary: previous.as_ref().map(|summary| summary.text.clone()),
            details: Some(json!({
                "readFiles": read_files,
                "modifiedFiles": modified_files,
            })),
        };
        session.append_compaction(entry)?;
        Ok(CompactionOutcome::Compacted {
            first_kept_entry_id,
            tokens_before: usage_or_estimate,
        })
    }

    /// 切点查找（Pi `findCutPoint` 语义，见模块契约）。
    fn find_cut_point_in_range(
        &self,
        entries: &[SessionEntry],
        start_index: usize,
        end_index: usize,
        keep_recent_tokens: u64,
    ) -> CutPointResult {
        let cut_points: Vec<usize> = (start_index..end_index)
            .filter(|&index| is_cut_point_entry(&entries[index]))
            .collect();
        if cut_points.is_empty() {
            return CutPointResult {
                first_kept_entry_index: start_index,
                turn_start_index: None,
                is_split_turn: false,
            };
        }
        // 从最新回走累积估计 token，达到预算时取 >= 当前条目的最近合法切点。
        // 切点落在 toolResult 上时向后跳到下一个合法切点（tool result 跟随其 tool call）。
        let mut accumulated_tokens = 0u64;
        let mut cut_index = cut_points[0];
        for index in (start_index..end_index).rev() {
            let message_tokens = entry_token_estimate(&entries[index]);
            if message_tokens == 0 {
                continue;
            }
            accumulated_tokens += message_tokens;
            if accumulated_tokens >= keep_recent_tokens {
                if let Some(&next) = cut_points.iter().find(|&&cut| cut >= index) {
                    cut_index = next;
                }
                break;
            }
        }
        // 回扫吸收不影响上下文的相邻元数据条目（model/thinking change、custom 等）。
        while cut_index > start_index {
            let previous = &entries[cut_index - 1].entry_type;
            if matches!(previous, SessionEntryType::Compaction(_))
                || matches!(previous, SessionEntryType::Message(_))
            {
                break;
            }
            cut_index -= 1;
        }
        let starts_turn = is_turn_start_entry(&entries[cut_index]);
        let turn_start_index = if starts_turn {
            None
        } else {
            find_turn_start_index(entries, cut_index, start_index)
        };
        CutPointResult {
            first_kept_entry_index: cut_index,
            turn_start_index,
            is_split_turn: !starts_turn && turn_start_index.is_some(),
        }
    }

    /// split turn 前缀摘要（Pi `generateTurnPrefixSummary`，预算为 reserve 的 0.5 倍）。
    fn generate_turn_prefix_summary(
        &self,
        conversation: &str,
        cancellation: &CancellationToken,
    ) -> Result<String> {
        let prompt = format!(
            "<conversation>\n{conversation}\n</conversation>\n\n{TURN_PREFIX_SUMMARIZATION_PROMPT}"
        );
        self.complete_summarization(
            &prompt,
            1,
            2,
            "turn prefix summarization failed",
            cancellation,
        )
    }

    /// 单个摘要模型调用：与普通请求共用 role adaptation seam。
    ///
    /// 输出上限取 `reserve * fraction` 与调用方模型偏好上限的较小值（Pi 再与模型
    /// `maxTokens` 取小；本边界由 provider 侧校验兜底）。
    /// `cancellation` 透传给 provider：中断时不继续等待摘要。
    fn complete_summarization(
        &self,
        prompt_text: &str,
        fraction_numerator: u64,
        fraction_denominator: u64,
        error_prefix: &str,
        cancellation: &CancellationToken,
    ) -> Result<String> {
        let from_reserve =
            self.reserve_tokens.saturating_mul(fraction_numerator) / fraction_denominator;
        let cap = self.model_preferences.max_output_tokens.unwrap_or(u32::MAX) as u64;
        let mut preferences = self.model_preferences.clone();
        preferences.max_output_tokens =
            Some(u32::try_from(from_reserve.min(cap)).unwrap_or(u32::MAX));
        let mut request = ModelTurnRequest::new(
            format!("compaction-{}", Uuid::now_v7()),
            crate::agent::instruction_message(
                &self.provider.protocol_contract(),
                SUMMARIZATION_SYSTEM_PROMPT,
            )
            .into_iter()
            .chain(std::iter::once(ModelMessage::text(
                ModelRole::User,
                prompt_text,
            )))
            .collect(),
        );
        request.model_preferences = preferences;
        let response = self.provider.complete(&request, cancellation)?;
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
        response
            .assistant_message
            .map(|message| message.content)
            .ok_or_else(|| {
                CompactionError::InvalidResponse(format!(
                    "{error_prefix}: missing assistant message"
                ))
            })
    }
}

/// Pi `estimateTokens` 的 chars/4 启发式；空串为 0。
fn estimate_tokens_of(text: &str) -> u64 {
    let chars = text.encode_utf16().count() as u64;
    chars.div_ceil(4)
}

/// 单条 entry 的估计 token 数（对齐 Pi `sessionEntryToContextMessages` + `estimateTokens`：
/// compaction 条目按其 summary 文本估算，非消息条目为 0）。
fn entry_token_estimate(entry: &SessionEntry) -> u64 {
    match &entry.entry_type {
        SessionEntryType::Message(message) => estimate_tokens_of(&message.content_text()),
        SessionEntryType::Compaction(compaction) => estimate_tokens_of(&compaction.summary),
        _ => 0,
    }
}

/// 消息是否为合法切点（Pi `isCutPointMessage`：除 toolResult 外全部合法）。
fn is_cut_point_entry(entry: &SessionEntry) -> bool {
    match &entry.entry_type {
        SessionEntryType::Message(message) => !matches!(message.role, AgentMessageRole::ToolResult),
        _ => false,
    }
}

/// 消息是否开启新 turn（Pi `isTurnStartMessage`）。
fn is_turn_start_entry(entry: &SessionEntry) -> bool {
    match &entry.entry_type {
        SessionEntryType::Message(message) => matches!(
            message.role,
            AgentMessageRole::User
                | AgentMessageRole::BashExecution
                | AgentMessageRole::Custom
                | AgentMessageRole::BranchSummary
                | AgentMessageRole::CompactionSummary
        ),
        _ => false,
    }
}

/// 在 `entry_index` 及之前寻找包含该条目的 turn 的起始消息（Pi `findTurnStartIndex`）。
fn find_turn_start_index(
    entries: &[SessionEntry],
    entry_index: usize,
    start_index: usize,
) -> Option<usize> {
    (start_index..=entry_index)
        .rev()
        .find(|&index| is_turn_start_entry(&entries[index]))
}

/// 条目是否产出摘要用消息（Pi `getMessageFromEntryForCompaction`）。
fn message_from_entry(entry: &SessionEntry) -> Option<&AgentMessage> {
    match &entry.entry_type {
        SessionEntryType::Message(message) => Some(message),
        _ => None,
    }
}

/// UTF-16 code unit 数（对齐 JS `String.length`）。
fn utf16_len(text: &str) -> usize {
    text.encode_utf16().count()
}

/// 截断到 `max_chars` 字符并附 Pi 风格的截断标记；保留开头。
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

/// tool call 参数序列化为 `k=json` 列表（Pi `Object.entries(args)` 的 JSON.stringify）。
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

/// 文件操作累积集（Pi `createFileOps`）。
#[derive(Default)]
struct FileOps {
    read: BTreeSet<String>,
    written: BTreeSet<String>,
    edited: BTreeSet<String>,
}

/// 从 assistant 消息的 tool_call 块提取文件操作（Pi `extractFileOpsFromMessage`）：
/// `read`/`write`/`edit` 且 `args.path` 为字符串。
fn extract_file_ops_from_message(message: &AgentMessage, file_ops: &mut FileOps) {
    if message.role != AgentMessageRole::Assistant {
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

/// 计算最终文件列表（Pi `computeFileLists`）：modified = edited ∪ written；
/// readFiles = read − modified；均排序。
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

/// 文件列表格式化为 `<read-files>`/`<modified-files>` XML 块（Pi `formatFileOperations`）。
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

/// 从 compaction 条目 details 解析累积文件列表（Pi `CompactionDetails.readFiles`/
/// `modifiedFiles`；缺失时为空）。
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
mod tests {
    use super::*;
    use singularity_model::{ModelTurnResponse, ProviderProtocolContract};
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    fn user(text: &str) -> AgentMessage {
        AgentMessage {
            role: AgentMessageRole::User,
            content: vec![ContentBlock::Text {
                text: text.to_string(),
            }],
            provider_reasoning_replay: None,
            tool_call_id: None,
            tool_name: None,
            is_error: None,
            timestamp: None,
        }
    }

    fn assistant(text: &str) -> AgentMessage {
        AgentMessage {
            role: AgentMessageRole::Assistant,
            content: vec![ContentBlock::Text {
                text: text.to_string(),
            }],
            provider_reasoning_replay: None,
            tool_call_id: None,
            tool_name: None,
            is_error: None,
            timestamp: None,
        }
    }

    fn tool_result(call_id: &str, text: &str) -> AgentMessage {
        AgentMessage {
            role: AgentMessageRole::ToolResult,
            content: vec![ContentBlock::Text {
                text: text.to_string(),
            }],
            provider_reasoning_replay: None,
            tool_call_id: Some(call_id.to_string()),
            tool_name: Some("bash".to_string()),
            is_error: None,
            timestamp: None,
        }
    }

    fn file_call(tool_name: &str, path: &str) -> AgentMessage {
        AgentMessage {
            role: AgentMessageRole::Assistant,
            content: vec![ContentBlock::ToolCall {
                id: "call_file".to_string(),
                name: tool_name.to_string(),
                args: json!({"path": path}),
            }],
            provider_reasoning_replay: None,
            tool_call_id: None,
            tool_name: None,
            is_error: None,
            timestamp: None,
        }
    }

    fn message_entry(message: AgentMessage) -> SessionEntry {
        SessionEntry {
            id: Uuid::new_v4()
                .simple()
                .to_string()
                .chars()
                .take(8)
                .collect(),
            parent_id: String::new(),
            timestamp: None,
            entry_type: SessionEntryType::Message(message),
        }
    }

    fn entries_of(messages: Vec<AgentMessage>) -> Vec<SessionEntry> {
        messages.into_iter().map(message_entry).collect()
    }

    fn budget(window: u64, keep_recent: u64) -> CompactionBudget {
        CompactionBudget {
            context_window: window,
            reserve_tokens: DEFAULT_RESERVE_TOKENS,
            keep_recent_tokens: keep_recent,
        }
    }

    /// 记录请求并提供固定文本的 mock provider。
    #[derive(Clone)]
    struct MockProvider {
        texts: Arc<Mutex<VecDeque<String>>>,
        requests: Arc<Mutex<Vec<ModelTurnRequest>>>,
    }

    impl MockProvider {
        fn new(texts: Vec<String>) -> Self {
            Self {
                texts: Arc::new(Mutex::new(texts.into())),
                requests: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn requests(&self) -> Vec<ModelTurnRequest> {
            self.requests.lock().unwrap().clone()
        }
    }

    impl Provider for MockProvider {
        fn protocol_contract(&self) -> ProviderProtocolContract {
            ProviderProtocolContract::default()
        }

        fn complete(
            &self,
            request: &ModelTurnRequest,
            _cancellation: &CancellationToken,
        ) -> std::result::Result<ModelTurnResponse, ProviderError> {
            self.requests.lock().unwrap().push(request.clone());
            let text = self.texts.lock().unwrap().pop_front().unwrap_or_default();
            Ok(ModelTurnResponse::completed(
                request.request_id.clone(),
                "mock-response",
                text,
            ))
        }
    }

    fn mock_engine(texts: Vec<String>) -> (CompactionEngine, MockProvider) {
        let mock = MockProvider::new(texts);
        let provider: Arc<dyn Provider + Send + Sync> = Arc::new(mock.clone());
        (CompactionEngine::new(provider), mock)
    }

    /// 1. should_compact 阈值边界：刚好低于/等于/超过。
    #[test]
    fn should_compact_threshold_boundaries() {
        let (engine, _) = mock_engine(vec![]);
        let budget = budget(100_000, 20_000);
        // 阈值 = 100_000 - 16_384 = 83_616。
        assert!(!engine.should_compact(0, &budget));
        assert!(!engine.should_compact(83_615, &budget));
        assert!(!engine.should_compact(83_616, &budget), "等于阈值不触发");
        assert!(engine.should_compact(83_617, &budget), "超过阈值触发");
        assert!(engine.should_compact(100_000, &budget));
        // context_window < reserve_tokens：阈值饱和为 0。
        let tiny = CompactionBudget {
            context_window: 100,
            reserve_tokens: 16_384,
            keep_recent_tokens: 20_000,
        };
        assert!(engine.should_compact(1, &tiny));
    }

    /// 2. find_cut_point：全量切点、toolResult 跟随、split turn、keep 边界、元数据回扫。
    #[test]
    fn find_cut_point_full_history_and_tool_result_following() {
        let (engine, _) = mock_engine(vec![]);
        let messages = vec![
            user("aaaaaaaaaa"),              // 0 user（切点）
            assistant("bbbbbbbbbb"),         // 1 assistant（切点）
            tool_result("c1", "cccccccccc"), // 2 toolResult（非切点）
            user("dddddddddd"),              // 3 user（切点）
            assistant("eeeeeeeeee"),         // 4 assistant（切点）
            tool_result("c2", "ffffffffff"), // 5 toolResult（非切点）
        ];
        let entries = entries_of(messages);

        // 预算足够大：不跨阈值 → 默认保留从第一个消息起（cutPoints[0]）。
        assert_eq!(
            engine.find_cut_point(&entries, &budget(100_000, 100_000)),
            Some(0)
        );

        // 预算 10 token：回走累积在 t0（index 2）跨过 → 切点跳到其后最近的合法切点 u1（index 3）。
        // 切点绝不在 toolResult 上。
        assert_eq!(
            engine.find_cut_point(&entries, &budget(100_000, 10)),
            Some(3)
        );

        // 空条目无切点。
        assert_eq!(engine.find_cut_point(&[], &budget(100_000, 10)), None);
    }

    #[test]
    fn find_cut_point_split_turn_and_keep_boundary() {
        let (engine, _) = mock_engine(vec![]);
        // 单个超大 turn：u0/a0/t0/a1/t1 各 400 字符（100 token）。
        let messages = vec![
            user(&"u".repeat(400)),
            assistant(&"a".repeat(400)),
            tool_result("c1", &"t".repeat(400)),
            assistant(&"b".repeat(400)),
            tool_result("c2", &"q".repeat(400)),
        ];
        let entries = entries_of(messages);

        // keep=250：跨过点落在 t0（index 2）→ 切在 a1（index 3），切点位于 assistant →
        // split turn，turn 起始为 u0（index 0）。
        let cut = engine.find_cut_point_in_range(&entries, 0, entries.len(), 250);
        assert_eq!(cut.first_kept_entry_index, 3);
        assert_eq!(cut.turn_start_index, Some(0));
        assert!(cut.is_split_turn);

        // keep=100：跨过点在最新 toolResult → 无合法切点可跳 → 全部保留（cutPoints[0]）。
        let cut = engine.find_cut_point_in_range(&entries, 0, entries.len(), 100);
        assert_eq!(cut.first_kept_entry_index, 0);
        assert!(!cut.is_split_turn);

        // keep 边界：恰好等于累积值（>= 语义）与多 1 token 的差异。
        let messages = vec![
            user(&"u".repeat(400)),
            assistant(&"a".repeat(400)),
            tool_result("c1", &"t".repeat(400)),
            user(&"d".repeat(400)),
            assistant(&"e".repeat(400)),
            tool_result("c2", &"f".repeat(400)),
        ];
        let entries = entries_of(messages);
        // 回走：t1=100,a1=200,u1=300 ≥ 300 → 切在 u1（index 3）。
        assert_eq!(
            engine
                .find_cut_point_in_range(&entries, 0, entries.len(), 300)
                .first_kept_entry_index,
            3
        );
        // 400：跨过点恰好等于累积值（>= 语义）→ 切在 u1（index 3）；
        // 401：多 1 token 时跨过点落在 t0 → 切在 a0（index 1）。
        assert_eq!(
            engine
                .find_cut_point_in_range(&entries, 0, entries.len(), 400)
                .first_kept_entry_index,
            3
        );
        assert_eq!(
            engine
                .find_cut_point_in_range(&entries, 0, entries.len(), 401)
                .first_kept_entry_index,
            1
        );
    }

    #[test]
    fn find_cut_point_metadata_scan() {
        let (engine, _) = mock_engine(vec![]);
        // model_change 无上下文消息：切点从 u0（index 1）回扫吸收该元数据条目。
        let model_change = SessionEntry {
            id: "m0000001".to_string(),
            parent_id: String::new(),
            timestamp: None,
            entry_type: SessionEntryType::ModelChange {
                provider: "openai".to_string(),
                model_id: "gpt-4o".to_string(),
            },
        };
        let mut entries = vec![model_change];
        entries.extend(entries_of(vec![
            user("aaaaaaaaaa"),
            assistant("bbbbbbbbbb"),
            tool_result("c1", "cccccccccc"),
        ]));
        assert_eq!(
            engine.find_cut_point(&entries, &budget(100_000, 100_000)),
            Some(0),
            "切点应回扫到元数据条目（firstKeptEntryId 指向它）"
        );
    }

    /// 3. estimate_tokens：空串/英文/中文/长文本边界。
    #[test]
    fn estimate_tokens_boundaries() {
        let (engine, _) = mock_engine(vec![]);
        assert_eq!(engine.estimate_tokens(""), 0);
        assert_eq!(engine.estimate_tokens("a"), 1);
        assert_eq!(engine.estimate_tokens("abcd"), 1);
        assert_eq!(engine.estimate_tokens("abcde"), 2);
        assert_eq!(engine.estimate_tokens("中文测试"), 1); // 4 字符 → ceil(4/4)
        assert_eq!(engine.estimate_tokens("中文测试一"), 2); // 5 字符 → ceil(5/4)
        assert_eq!(engine.estimate_tokens(&"x".repeat(8000)), 2000);
    }

    /// 4. serialize_conversation：role 标注、tool result 截断、tool call 序列化。
    #[test]
    fn serialize_conversation_roles_and_truncation() {
        let (engine, _) = mock_engine(vec![]);
        let long_output = "x".repeat(2500);
        let messages = vec![
            user("hello"),
            assistant("hi"),
            file_call("read", "src/main.rs"),
            tool_result("c1", &long_output),
            AgentMessage {
                role: AgentMessageRole::BashExecution,
                content: vec![ContentBlock::Text {
                    text: "ran a command".to_string(),
                }],
                provider_reasoning_replay: None,
                tool_call_id: None,
                tool_name: None,
                is_error: None,
                timestamp: None,
            },
            AgentMessage {
                role: AgentMessageRole::CompactionSummary,
                content: vec![ContentBlock::Text {
                    text: "earlier summary".to_string(),
                }],
                provider_reasoning_replay: None,
                tool_call_id: None,
                tool_name: None,
                is_error: None,
                timestamp: None,
            },
            user(""),
        ];
        let text = engine.serialize_conversation(&messages);
        assert!(text.contains("[User]: hello"));
        assert!(text.contains("[Assistant]: hi"));
        assert!(text.contains(r#"[Assistant tool calls]: read(path="src/main.rs")"#));
        assert!(text.contains("[Tool result]: "));
        assert!(text.contains("[User]: ran a command"));
        assert!(text.contains(&format!(
            "{COMPACTION_SUMMARY_PREFIX}earlier summary{COMPACTION_SUMMARY_SUFFIX}"
        )));
        assert!(!text.contains("[User]: \n"), "空 user 内容应跳过");
        // 截断：保留开头 2000 字符并附精确截断标记（2500 - 2000 = 500）。
        assert!(text.contains(&"x".repeat(2000)));
        assert!(text.contains("[... 500 more characters truncated]"));
        assert!(!text.contains("xxx[..."));
    }

    /// 4b. 文件操作提取与格式化（read/write/edit、read-only 排除已修改文件、排序）。
    #[test]
    fn file_ops_extraction_and_formatting() {
        let mut ops = FileOps::default();
        extract_file_ops_from_message(&file_call("read", "z.txt"), &mut ops);
        extract_file_ops_from_message(&file_call("read", "a.txt"), &mut ops);
        extract_file_ops_from_message(&file_call("edit", "a.txt"), &mut ops);
        extract_file_ops_from_message(&file_call("write", "b.txt"), &mut ops);
        // 非 assistant / 无 path 的调用不提取。
        extract_file_ops_from_message(&user("hello"), &mut ops);
        let (read_files, modified_files) = compute_file_lists(&ops);
        // a.txt 被 edit → 属于 modified，不出现在 readFiles。
        assert_eq!(read_files, vec!["z.txt".to_string()]);
        assert_eq!(
            modified_files,
            vec!["a.txt".to_string(), "b.txt".to_string()]
        );
        let formatted = format_file_operations(&read_files, &modified_files);
        assert_eq!(
            formatted,
            "\n\n<read-files>\nz.txt\n</read-files>\n\n<modified-files>\na.txt\nb.txt\n</modified-files>"
        );
        // 历史 details 累积：readFiles/modifiedFiles 数组并入集合。
        let mut ops = FileOps::default();
        for file in ["old.txt", "z.txt"] {
            ops.read.insert(file.to_string());
        }
        ops.edited.insert("a.txt".to_string());
        let (read_files, modified_files) = compute_file_lists(&ops);
        let _ = (read_files, modified_files);
    }

    /// 5. compact 全流程（mock provider）：触发、摘要、append_compaction 落盘、
    ///    first_kept_entry_id 正确、重开后 build_context_entries 切片正确。
    #[test]
    fn compact_full_flow_and_reopen_slicing() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = SessionManager::create(dir.path(), dir.path()).unwrap();
        // 每条 7000 字符 ≈ 1750 token；keep=4000 时切点落在 u1（index 3）。
        let id_u0 = session.append_message(user(&"u".repeat(7000))).unwrap();
        session
            .append_message(file_call("read", "src/main.rs"))
            .unwrap();
        let id_t0 = session
            .append_message(tool_result("c1", &"t".repeat(7000)))
            .unwrap();
        let id_u1 = session.append_message(user(&"d".repeat(7000))).unwrap();
        let id_a1 = session
            .append_message(assistant(&"e".repeat(7000)))
            .unwrap();
        let id_t1 = session
            .append_message(tool_result("c2", &"f".repeat(7000)))
            .unwrap();

        let (mut engine, mock) = mock_engine(vec!["## Goal\nsummary of history".to_string()]);
        let budget = budget(100_000, 4_000);
        let outcome = engine
            .compact(&mut session, &budget, 90_000, &CancellationToken::new())
            .unwrap();
        assert_eq!(
            outcome,
            CompactionOutcome::Compacted {
                first_kept_entry_id: id_u1.clone(),
                tokens_before: 90_000,
            }
        );

        // 未触发 → NotNeeded。
        assert_eq!(
            engine
                .compact(&mut session, &budget, 10_000, &CancellationToken::new())
                .unwrap(),
            CompactionOutcome::NotNeeded
        );

        // 摘要请求：1 次，developer + user prompt（ProviderProtocolContract 默认支持
        // developer），含 <conversation> 与初始 prompt，无 previous。
        let requests = mock.requests();
        assert_eq!(requests.len(), 1);
        let roles: Vec<ModelRole> = requests[0]
            .messages
            .iter()
            .map(|m| m.role.clone())
            .collect();
        assert_eq!(roles, vec![ModelRole::Developer, ModelRole::User]);
        let prompt = &requests[0].messages[1].content;
        assert!(prompt.contains("<conversation>\n"));
        assert!(prompt.contains("Use this EXACT format:"));
        assert!(!prompt.contains("<previous-summary>"));
        assert!(prompt.contains("[User]: "));
        assert!(prompt.contains(r#"[Assistant tool calls]: read(path="src/main.rs")"#));

        // 磁盘 compaction 条目：summary 含文件操作块；details 记录累积文件列表。
        let content = std::fs::read_to_string(session.path()).unwrap();
        let last: Value = serde_json::from_str(content.lines().last().unwrap()).unwrap();
        assert_eq!(last["type"], "compaction");
        assert_eq!(last["firstKeptEntryId"], id_u1);
        assert_eq!(last["tokensBefore"], 90_000);
        assert!(last.get("previousSummary").is_none());
        let summary = last["summary"].as_str().unwrap();
        assert!(summary.starts_with("## Goal"));
        assert!(summary.ends_with("\n\n<read-files>\nsrc/main.rs\n</read-files>"));
        assert_eq!(last["details"]["readFiles"], json!(["src/main.rs"]));
        assert_eq!(last["details"]["modifiedFiles"], json!([]));

        // 重开：上下文 = [compaction, 从 firstKeptEntryId 起的保留条目]，旧消息被摘要取代。
        let reopened = SessionManager::open(session.path()).unwrap();
        let ctx = reopened.build_context_entries().unwrap();
        let ctx_ids: Vec<&str> = ctx.iter().map(|entry| entry.id.as_str()).collect();
        assert_eq!(ctx_ids.len(), 4);
        assert!(matches!(ctx[0].entry_type, SessionEntryType::Compaction(_)));
        assert_eq!(
            ctx_ids,
            vec![
                ctx[0].id.as_str(),
                id_u1.as_str(),
                id_a1.as_str(),
                id_t1.as_str()
            ]
        );
        assert!(!ctx_ids.contains(&id_u0.as_str()));
        assert!(!ctx_ids.contains(&id_t0.as_str()));

        // 二次压缩：起点 = 上次 first_kept_entry_id，previousSummary 传入 UPDATE 合并。
        let id_u2 = session.append_message(user(&"g".repeat(7000))).unwrap();
        let id_a2 = session
            .append_message(assistant(&"h".repeat(7000)))
            .unwrap();
        let id_t2 = session
            .append_message(tool_result("c3", &"i".repeat(7000)))
            .unwrap();
        let (mut engine, mock) = mock_engine(vec!["## Goal\nupdated summary".to_string()]);
        let outcome = engine
            .compact(&mut session, &budget, 90_000, &CancellationToken::new())
            .unwrap();
        assert_eq!(
            outcome,
            CompactionOutcome::Compacted {
                first_kept_entry_id: id_u2.clone(),
                tokens_before: 90_000,
            }
        );
        let requests = mock.requests();
        assert_eq!(requests.len(), 1);
        let prompt = &requests[0].messages[1].content;
        // previousSummary 为上次压缩的完整 summary 文本（含文件操作块）。
        let previous_summary =
            "## Goal\nsummary of history\n\n<read-files>\nsrc/main.rs\n</read-files>";
        assert!(prompt.contains(&format!(
            "<previous-summary>\n{previous_summary}\n</previous-summary>"
        )));
        assert!(prompt.contains("PRESERVE all existing information"));
        let content = std::fs::read_to_string(session.path()).unwrap();
        let last: Value = serde_json::from_str(content.lines().last().unwrap()).unwrap();
        assert_eq!(last["firstKeptEntryId"], id_u2);
        assert_eq!(last["previousSummary"], previous_summary);
        // 文件列表从历史 details 累积。
        assert_eq!(last["details"]["readFiles"], json!(["src/main.rs"]));
        let reopened = SessionManager::open(session.path()).unwrap();
        let ctx = reopened.build_context_entries().unwrap();
        let ctx_ids: Vec<&str> = ctx.iter().map(|entry| entry.id.as_str()).collect();
        assert_eq!(
            ctx_ids,
            vec![
                ctx[0].id.as_str(),
                id_u2.as_str(),
                id_a2.as_str(),
                id_t2.as_str()
            ]
        );
    }

    /// 5b. split turn：历史摘要 + turn 前缀摘要两次调用，按 Pi 模板合并。
    #[test]
    fn compact_split_turn_merges_two_summaries() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = SessionManager::create(dir.path(), dir.path()).unwrap();
        // u0/a0/t0 为完整历史 turn；u1/a1/t1 为超大 turn。
        session.append_message(user(&"u".repeat(7000))).unwrap();
        session
            .append_message(file_call("read", "split.txt"))
            .unwrap();
        session
            .append_message(tool_result("c1", &"t".repeat(7000)))
            .unwrap();
        let id_u1 = session.append_message(user(&"d".repeat(10_000))).unwrap();
        let id_a1 = session
            .append_message(assistant(&"e".repeat(10_000)))
            .unwrap();
        let id_t1 = session
            .append_message(tool_result("c2", &"f".repeat(10_000)))
            .unwrap();

        // keep=2600：跨过点在 a1 → 切在 a1（index 4）→ split；历史 = u0/a0/t0。
        let (mut engine, mock) = mock_engine(vec![
            "## Goal\nhistory".to_string(),
            "## Original Request\nprefix".to_string(),
        ]);
        let budget = budget(100_000, 2_600);
        let outcome = engine
            .compact(&mut session, &budget, 90_000, &CancellationToken::new())
            .unwrap();
        assert_eq!(
            outcome,
            CompactionOutcome::Compacted {
                first_kept_entry_id: id_a1.clone(),
                tokens_before: 90_000,
            }
        );
        let requests = mock.requests();
        assert_eq!(requests.len(), 2, "历史与 turn 前缀各一次摘要调用");
        assert!(
            requests[0].messages[1]
                .content
                .contains("Use this EXACT format:")
        );
        assert!(
            requests[1].messages[1]
                .content
                .contains("## Original Request")
        );
        let content = std::fs::read_to_string(session.path()).unwrap();
        let last: Value = serde_json::from_str(content.lines().last().unwrap()).unwrap();
        let summary = last["summary"].as_str().unwrap();
        assert!(summary.starts_with("## Goal\nhistory"));
        assert!(
            summary.contains(
                "\n\n---\n\n**Turn Context (split turn):**\n\n## Original Request\nprefix"
            )
        );
        assert!(summary.ends_with("\n\n<read-files>\nsplit.txt\n</read-files>"));
        assert_eq!(last["details"]["readFiles"], json!(["split.txt"]));

        let reopened = SessionManager::open(session.path()).unwrap();
        let ctx = reopened.build_context_entries().unwrap();
        let ctx_ids: Vec<&str> = ctx.iter().map(|entry| entry.id.as_str()).collect();
        assert_eq!(ctx_ids.len(), 3);
        assert_eq!(
            ctx_ids,
            vec![ctx[0].id.as_str(), id_a1.as_str(), id_t1.as_str()]
        );
        assert!(!ctx_ids.contains(&id_u1.as_str()));
    }

    /// 5c. compact 在无可摘要内容时返回 NotNeeded（全部保留路径）。
    #[test]
    fn compact_nothing_to_summarize() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = SessionManager::create(dir.path(), dir.path()).unwrap();
        session.append_message(user("hi")).unwrap();
        let (mut engine, mock) = mock_engine(vec![]);
        // 触发条件满足但 keep 预算极大 → 切点在起点 → 无可摘要内容。
        let cfg = budget(100_000, 1_000_000);
        assert_eq!(
            engine
                .compact(&mut session, &cfg, 90_000, &CancellationToken::new())
                .unwrap(),
            CompactionOutcome::NotNeeded
        );
        assert!(mock.requests().is_empty(), "不应发起摘要调用");
        // 再次 compact：仍然没有新内容，不产生 compaction 条目。
        let _ = engine
            .compact(&mut session, &cfg, 90_000, &CancellationToken::new())
            .unwrap();
        let content = std::fs::read_to_string(session.path()).unwrap();
        let lines: Vec<&str> = content.lines().skip(1).collect();
        assert_eq!(lines.len(), 1, "只有一条消息，无 compaction 条目");
    }

    /// 6. 摘要 prompt 常量与 Pi 结构一致（抽查关键段落与顺序）。
    #[test]
    fn summarization_prompts_match_pi_structure() {
        let sections = [
            "## Goal",
            "## Constraints & Preferences",
            "## Progress",
            "### Done",
            "### In Progress",
            "### Blocked",
            "## Key Decisions",
            "## Next Steps",
            "## Critical Context",
        ];
        for section in sections {
            assert!(SUMMARIZATION_PROMPT.contains(section), "缺少段落 {section}");
        }
        assert!(SUMMARIZATION_PROMPT.contains("Use this EXACT format:"));
        assert!(
            SUMMARIZATION_PROMPT
                .contains("Preserve exact file paths, function names, and error messages.")
        );
        // 段落顺序与 Pi 一致。
        let positions: Vec<usize> = [
            "## Goal",
            "## Progress",
            "## Key Decisions",
            "## Next Steps",
            "## Critical Context",
        ]
        .iter()
        .map(|section| SUMMARIZATION_PROMPT.find(section).unwrap())
        .collect();
        assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));

        assert!(UPDATE_SUMMARIZATION_PROMPT.contains("PRESERVE all existing information"));
        assert!(UPDATE_SUMMARIZATION_PROMPT.contains("<previous-summary>"));
        assert!(
            UPDATE_SUMMARIZATION_PROMPT.contains("move items from \"In Progress\" to \"Done\"")
        );
        assert!(UPDATE_SUMMARIZATION_PROMPT.contains("## Critical Context"));

        assert!(TURN_PREFIX_SUMMARIZATION_PROMPT.contains("## Original Request"));
        assert!(TURN_PREFIX_SUMMARIZATION_PROMPT.contains("## Early Progress"));
        assert!(TURN_PREFIX_SUMMARIZATION_PROMPT.contains("## Context for Suffix"));

        assert!(SUMMARIZATION_SYSTEM_PROMPT.contains("Do NOT continue the conversation."));
        assert!(SUMMARIZATION_SYSTEM_PROMPT.contains("ONLY output the structured summary."));
    }
}
