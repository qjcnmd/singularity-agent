//! 上下文缩减策略、摘要请求的构造与结果校验。
//!
//! 摘要只替换早期历史；请求执行、取消和持久提交统一由 Agent 负责，文件指令在压缩后会重新加载。

use crate::request_execution::output_budget_tokens;
use crate::session::context::CompactionPrefix;
use crate::session::context::estimate_tokens_of;
use crate::session::{CompactionEntry, RequestDefinitions};

use crate::agent::{AgentError, Result};
use singularity_model::{
    ModelConfigurationSnapshot, ModelMessage, ModelPreferences, ModelRole, ModelTurnRequest,
    ModelTurnResponse,
};

/// 摘要请求的目标输出上限；实际值还受模型上限与本次请求的窗口余量约束。
const DEFAULT_SUMMARY_MAX_TOKENS: u32 = 8192;

const INITIAL_SUMMARY_INSTRUCTION: &str =
    "Summarize the earlier conversation so another coding assistant can continue the user's current task.";
const UPDATE_SUMMARY_INSTRUCTION: &str = "Update the previous summary with the new conversation above. Preserve still-current goals, constraints, completed work, and decisions. Move finished work to Done, remove resolved blockers, and revise Next Steps.";
const SUMMARY_FORMAT: &str = r#"Output only a concise summary with these Markdown sections, in order:
## Goal
[The user's current objective]
## Constraints & Preferences
- [Current user requirements and preferences]
## Progress
### Done
- [x] [Completed work]
### In Progress
- [ ] [Current work]
### Blocked
- [Current blockers]
## Key Decisions
- **[Decision]**: [Reason]
## Next Steps
1. [Next concrete action]
## Critical Context
- [Facts needed to continue]

Use "(none)" for an empty section. Preserve exact paths, identifiers, commands, errors, and user wording when needed to continue. Distinguish verified results from plans. Project files remain authoritative; Harness and project instructions are loaded separately. Do not call tools or continue the conversation."#;

/// 摘要请求和它要替换的历史边界；响应只有通过校验才能生成落盘条目。
pub(crate) struct PreparedCompaction {
    pub(crate) request: ModelTurnRequest,
    first_kept_entry_id: String,
}
impl PreparedCompaction {
    /// 摘要请求沿用生成请求的指令前缀与工具定义；只替换选中的对话历史前缀。
    pub(crate) fn new(
        prefix: CompactionPrefix,
        definitions: &RequestDefinitions,
        model: &ModelConfigurationSnapshot,
    ) -> crate::agent::Result<Self> {
        let overhead_tokens = definitions.estimated_tokens();
        let mut instructions = definitions.model_messages()?;
        instructions
            .extend(crate::session::context::load_messages(prefix.messages, &prefix.image_directory)?);
        let mut messages = instructions;
        let instruction = match prefix.previous_summary {
            Some(previous) => format!(
                "<previous-summary>\n{previous}\n</previous-summary>\n\n{UPDATE_SUMMARY_INSTRUCTION}\n\n{SUMMARY_FORMAT}"
            ),
            None => format!("{INITIAL_SUMMARY_INSTRUCTION}\n\n{SUMMARY_FORMAT}"),
        };
        let input_tokens = overhead_tokens
            .saturating_add(prefix.estimated_tokens)
            .saturating_add(estimate_tokens_of(&instruction) + 8);
        messages.push(ModelMessage::text(ModelRole::User, instruction));
        let request = ModelTurnRequest {
            messages,
            tools: definitions.tools.clone(),
            model_preferences: ModelPreferences {
                max_output_tokens: Some(output_budget_tokens(
                    model,
                    input_tokens,
                    DEFAULT_SUMMARY_MAX_TOKENS,
                )),
            },
        };
        Ok(Self {
            request,
            first_kept_entry_id: prefix.first_kept_entry_id,
        })
    }

    pub(crate) fn into_entry(self, response: ModelTurnResponse) -> Result<CompactionEntry> {
        if response.is_length_truncated() {
            return Err(AgentError::InvalidSummary(
                "summary reached the output limit (incomplete checkpoint)".into(),
            ));
        }
        let text = response.assistant_message.content;
        if text.trim().is_empty() {
            return Err(AgentError::InvalidSummary("summary contains no text".into()));
        }
        // 摘要请求的用量由统一请求账本记录（会话累计与展示都从那里读），这里只保留正文和保留锚点。
        Ok(CompactionEntry {
            summary: text,
            first_kept_entry_id: self.first_kept_entry_id,
        })
    }
}

/// 正文剪枝的下限字符数：不到这个长度就不剪。
const PRUNE_MIN_CHARS: usize = 8192;
/// 剪枝后分别保留的头部、尾部字符数。
const PRUNE_KEEP_HEAD_CHARS: usize = 4096;
const PRUNE_KEEP_TAIL_CHARS: usize = 1024;

/// 文本超过 [`PRUNE_MIN_CHARS`] 个 Unicode 字符时，保留头尾并标记省略部分。
pub(crate) fn prune_tool_text(text: &str) -> Option<String> {
    let total = text.chars().count();
    if total <= PRUNE_MIN_CHARS {
        return None;
    }
    let mut pruned: String = text.chars().take(PRUNE_KEEP_HEAD_CHARS).collect();
    pruned.push_str("\n\n[... tool result middle pruned ...]\n\n");
    pruned.extend(text.chars().skip(total - PRUNE_KEEP_TAIL_CHARS));
    Some(pruned)
}
