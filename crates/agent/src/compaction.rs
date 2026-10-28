//! 摘要请求的构造和结果检查。
//!
//! 摘要只替换早期历史；发送、停止和保存由 Agent 负责，文件指令在每轮准备时读取。

use crate::agent::request::RequestDefinitions;
use crate::request_execution::output_budget_tokens;
pub use crate::session::context::CompactionPrefix;
use singularity_core::estimate_tokens_of;

use crate::agent::{AgentError, Result};
use singularity_model::{Message, ModelConfigurationSnapshot, ModelTurnRequest, ModelTurnResponse};

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

/// 摘要请求，以及摘要生效后保留的第一条记录。
pub(crate) struct PreparedCompaction {
    pub(crate) request: ModelTurnRequest,
    pub(crate) first_kept_entry_id: String,
}
impl PreparedCompaction {
    /// 摘要请求沿用普通请求的指令和工具定义，后面接要总结的早期对话和总结要求。
    pub(crate) fn new(
        prefix: CompactionPrefix,
        definitions: RequestDefinitions,
        model: &ModelConfigurationSnapshot,
    ) -> crate::agent::Result<Self> {
        let overhead_tokens = definitions.estimated_tokens;
        let mut messages = definitions.messages;
        messages.extend(prefix.messages);
        let images = crate::session::context::load_images(&mut messages, &prefix.image_directory)?;
        let task = if prefix.updating {
            UPDATE_SUMMARY_INSTRUCTION
        } else {
            INITIAL_SUMMARY_INSTRUCTION
        };
        let instruction = format!(
            "{task}

{SUMMARY_FORMAT}"
        );
        let input_tokens = overhead_tokens
            .saturating_add(prefix.estimated_tokens)
            .saturating_add(estimate_tokens_of(&instruction) + 8);
        messages.push(Message::user(instruction));
        let request = ModelTurnRequest {
            messages,
            images,
            tools: definitions.tools,
            max_output_tokens: output_budget_tokens(model, input_tokens, DEFAULT_SUMMARY_MAX_TOKENS),
        };
        Ok(Self {
            request,
            first_kept_entry_id: prefix.first_kept_entry_id,
        })
    }
}

/// 检查摘要：没有被截断且正文非空才能使用。
pub(crate) fn validate_summary(response: ModelTurnResponse) -> Result<String> {
    if response.is_length_truncated() {
        return Err(AgentError::InvalidSummary(
            "summary reached the output limit (incomplete checkpoint)".into(),
        ));
    }
    let text = response.assistant_message.content_text();
    if text.trim().is_empty() {
        return Err(AgentError::InvalidSummary("summary contains no text".into()));
    }
    Ok(text)
}
