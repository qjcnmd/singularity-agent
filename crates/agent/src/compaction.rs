//! 上下文缩减策略、摘要请求的构造与结果校验。
//!
//! 摘要只替换早期历史；请求执行、取消和持久提交统一由 Agent 负责，文件指令由每轮准备负责。

use crate::request_execution::output_budget_tokens;
use crate::session::RequestDefinitions;
use crate::session::context::CompactionPrefix;
use crate::session::context::estimate_tokens_of;

use crate::agent::{AgentError, Result};
use singularity_model::{
    ModelConfigurationSnapshot, ModelMessage, ModelRole, ModelTurnRequest, ModelTurnResponse,
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
    pub(crate) first_kept_entry_id: String,
}
impl PreparedCompaction {
    /// 摘要请求沿用生成请求的指令前缀与工具定义；只替换选中的对话历史前缀。
    pub(crate) fn new(
        prefix: CompactionPrefix,
        definitions: &RequestDefinitions,
        model: &ModelConfigurationSnapshot,
    ) -> crate::agent::Result<Self> {
        let overhead_tokens = definitions.estimated_tokens();
        let mut messages = definitions.model_messages();
        messages.extend(crate::session::context::load_messages(prefix.messages, &prefix.image_directory)?);
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
            model_preferences: singularity_protocol::RequestPreferences {
                max_output_tokens: output_budget_tokens(model, input_tokens, DEFAULT_SUMMARY_MAX_TOKENS),
            },
        };
        Ok(Self {
            request,
            first_kept_entry_id: prefix.first_kept_entry_id,
        })
    }
}

/// 摘要通过校验后才由 Agent 保存并替换早期对话。
pub(crate) fn validate_summary(response: ModelTurnResponse) -> Result<String> {
    if response.is_length_truncated() {
        return Err(AgentError::InvalidSummary(
            "summary reached the output limit (incomplete checkpoint)".into(),
        ));
    }
    let text = response.assistant_message.content;
    if text.trim().is_empty() {
        return Err(AgentError::InvalidSummary("summary contains no text".into()));
    }
    Ok(text)
}
