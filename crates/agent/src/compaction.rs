//! 会话上下文自动压缩引擎（Context Compaction）。
//!
//! 在长程多轮会话中，当累计的上下文 Token 数接近模型上下文窗口上限时，
//! 压缩引擎会自动提取历史对话的结构化摘要，修剪早期详细历史并保留最新对话上下文。
//!
//! 核心流程：
//! 1. **触发判定**（`should_compact`）：依据当前 Token 数、模型上下文窗口与保留缓冲区预算判定是否触发。
//! 2. **切点查找**（`find_cut_point`）：从最新消息向后回溯，保留 `keep_recent_tokens` 预算内的最新消息；
//!    保证切点绝不切在工具结果（`tool_result`）中间，避免破坏模型工具调用配对结构；超长轮次支持 split turn 前缀摘要。
//! 3. **结构化摘要生成**（`generate_summary`）：调用模型提供方生成结构化摘要，若存在前次摘要则执行增量合并（UPDATE 模式），
//!    同时自动累积会话中读取与修改的文件列表（`<read-files>` 与 `<modified-files>`）。
//! 4. **持久化落盘**（`SessionManager::append_compaction`）：将生成的 `CompactionEntry` 写入会话文件，
//!    后续上下文构建（`build_session_context`）即可基于最新压缩节点快速重建。

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

use crate::message::{AgentMessage, AgentMessageRole, ContentBlock};
use crate::session::{
    CompactionEntry, SessionEntry, SessionEntryType, SessionError, SessionManager,
};

/// 默认保留给模型输出与系统指令的缓冲区 Token 数（16384）。
pub const DEFAULT_RESERVE_TOKENS: u64 = 16384;
/// 默认从切点向后保留的最近上下文 Token 预算（20000）。
pub const DEFAULT_KEEP_RECENT_TOKENS: u64 = 20000;
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

/// 上下文压缩的预算与触发参数配置。
#[derive(Debug, Clone, PartialEq)]
pub struct CompactionBudget {
    /// 模型静态声明的上下文窗口上限（Token 数）。
    pub context_window: u64,
    /// 预留给模型生成及系统消息的安全缓冲区 Token 数，默认 `DEFAULT_RESERVE_TOKENS`。
    pub reserve_tokens: u64,
    /// 从切点向后保留的最新上下文 Token 数，默认 `DEFAULT_KEEP_RECENT_TOKENS`。
    pub keep_recent_tokens: u64,
}

/// 摘要产物：结构化摘要文本以及跨轮次累积读取与修改的文件列表。
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

/// 切点查找的内部计算结果。
#[derive(Debug, Clone, PartialEq)]
struct CutPointResult {
    /// 保留区域的首个条目索引。
    first_kept_entry_index: usize,
    /// 若切点命中超长单轮内部，则记录该轮起始消息的索引。
    turn_start_index: Option<usize>,
    /// 是否为超长单轮被切开（Split Turn）。
    is_split_turn: bool,
}

/// 上下文压缩引擎：负责判定触发时机、查找安全切点并调用模型生成结构化摘要。
pub struct CompactionEngine {
    provider: Arc<dyn Provider + Send + Sync>,
    model_preferences: ModelPreferences,
    reserve_tokens: u64,
}

impl CompactionEngine {
    /// 创建压缩引擎实例，默认使用标准模型偏好与默认保留 Token 预算。
    pub fn new(provider: Arc<dyn Provider + Send + Sync>) -> Self {
        Self {
            provider,
            model_preferences: ModelPreferences::default(),
            reserve_tokens: DEFAULT_RESERVE_TOKENS,
        }
    }

    /// 绑定摘要请求的模型偏好配置（如模型名称、温度等）。
    pub fn with_model_preferences(mut self, preferences: ModelPreferences) -> Self {
        self.model_preferences = preferences;
        self
    }

    /// 绑定摘要生成时预留的安全 Token 预算。
    pub fn with_reserve_tokens(mut self, reserve_tokens: u64) -> Self {
        self.reserve_tokens = reserve_tokens;
        self
    }

    /// 判定是否应当触发压缩：当当前上下文 Token 数超过 `context_window - reserve_tokens` 时触发。
    pub fn should_compact(&self, context_tokens: u64, budget: &CompactionBudget) -> bool {
        context_tokens > budget.context_window.saturating_sub(budget.reserve_tokens)
    }

    /// 在给定的会话条目列表中查找安全切点，返回保留区域起始条目的索引。
    /// 若条目为空则返回 `None`。
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
            match message.role {
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
    /// 对话内容使用 `<conversation>` 标签包裹；若存在历史摘要，则放入 `<previous-summary>` 标签中
    /// 并使用 UPDATE 模板引导模型进行增量合并。支持通过取消信号提前终止。
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

    /// 执行会话压缩全流程：触发检查 -> 计算切点 -> 生成摘要与提取文件操作 -> 写入 CompactionEntry。
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
        // 若最新条目已是压缩节点，则说明尚无新的未压缩内容。
        if session.leaf_id() == entries[0].id
            && matches!(entries[0].entry_type, SessionEntryType::Compaction(_))
        {
            return Ok(CompactionOutcome::NotNeeded);
        }
        // 二次压缩起点：定位前次压缩节点记录的 first_kept_entry_id。
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
        // 获取前次摘要（包含文本与累积文件清单），用于增量更新与文件合并。
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
        // 从被压缩消息和历史记录中累积文件读取与修改清单。
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
            // Split Turn 场景：历史记录与超长轮前缀分别摘要后进行组合。
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

    /// 在指定范围内查找安全切点。
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
        // 从最新条目向后回溯累加 Token 估算值，达到保留预算时选择 >= 当前条目的最近合法切点。
        // 切点绝不切在 ToolResult 上（ToolResult 必须紧随其 ToolCall 保持在同一侧）。
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
        // 向前回溯吸收不影响会话语义的相邻元数据条目。
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

    /// 生成超长单轮（Split Turn）前缀摘要。
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

    /// 执行摘要模型的具体补全调用，处理安全预算与错误映射。
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

/// 基于 UTF-16 字符数的启发式 Token 估算函数（`ceil(chars / 4)`）。
fn estimate_tokens_of(text: &str) -> u64 {
    let chars = text.encode_utf16().count() as u64;
    chars.div_ceil(4)
}

/// 估算单条会话条目贡献的 Token 数量。
fn entry_token_estimate(entry: &SessionEntry) -> u64 {
    match &entry.entry_type {
        SessionEntryType::Message(message) => estimate_tokens_of(&message.content_text()),
        SessionEntryType::Compaction(compaction) => estimate_tokens_of(&compaction.summary),
        _ => 0,
    }
}

/// 判断某条目是否为合法的压缩切点（除 ToolResult 之外的消息均可作为切点）。
fn is_cut_point_entry(entry: &SessionEntry) -> bool {
    match &entry.entry_type {
        SessionEntryType::Message(message) => !matches!(message.role, AgentMessageRole::ToolResult),
        _ => false,
    }
}

/// 判断某条目是否为新轮次的起始（User 角色消息）。
fn is_turn_start_entry(entry: &SessionEntry) -> bool {
    match &entry.entry_type {
        SessionEntryType::Message(message) => matches!(message.role, AgentMessageRole::User),
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
    match &entry.entry_type {
        SessionEntryType::Message(message) => Some(message),
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
