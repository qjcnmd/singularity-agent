//! 上下文缩减策略、摘要请求的构造与结果校验。
//!
//! 摘要只替换早期历史；请求执行、取消和持久提交统一由 Agent 负责，文件指令在压缩后会重新加载。

use crate::message::ContentBlock;
use crate::session::CompactionEntry;
use crate::session::context::CompactionPrefix;

use crate::agent::{AgentError, Result};
use singularity_model::{
    ModelConfigurationSnapshot, ModelMessage, ModelPreferences, ModelRole, ModelToolSchema,
    ModelTurnRequest, ModelTurnResponse,
};

/// 摘要请求允许的最大输出 Token 数；实际值还要受当前模型输出上限的约束。
const DEFAULT_SUMMARY_MAX_TOKENS: u32 = 8192;

const INITIAL_SUMMARY_INSTRUCTION: &str = "Summarize the earlier conversation so another coding assistant can continue the user's current task.";
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

#[derive(Debug, Clone, PartialEq)]
pub enum CompactionOutcome {
    /// 没有触发压缩，或者没有可摘要的内容。
    NotNeeded,
    /// 摘要或工具输出剪枝的结果已经落盘。
    Reduced,
}

/// 摘要请求和它要替换的历史边界；响应只有通过校验才能生成落盘条目。
pub(crate) struct PreparedCompaction {
    pub(crate) request: ModelTurnRequest,
    first_kept_entry_id: String,
}
impl PreparedCompaction {
    /// 摘要请求沿用生成请求的指令前缀与工具定义；只替换选中的对话历史前缀。
    pub(crate) fn new(
        prefix: CompactionPrefix,
        instructions: &[ModelMessage],
        tools: &[ModelToolSchema],
        model: &ModelConfigurationSnapshot,
    ) -> Self {
        let mut messages = Vec::with_capacity(instructions.len() + prefix.messages.len() + 1);
        messages.extend_from_slice(instructions);
        messages.extend(prefix.messages);
        let instruction = match prefix.previous_summary {
            Some(previous) => format!(
                "<previous-summary>\n{previous}\n</previous-summary>\n\n{UPDATE_SUMMARY_INSTRUCTION}\n\n{SUMMARY_FORMAT}"
            ),
            None => format!("{INITIAL_SUMMARY_INSTRUCTION}\n\n{SUMMARY_FORMAT}"),
        };
        messages.push(ModelMessage::text(ModelRole::User, instruction));
        let request = ModelTurnRequest {
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

/// 不调用模型的剪枝：文本超过 [`PRUNE_MIN_CHARS`] 个 Unicode 字符时，只留下头部和尾部，中间换成省略标记；只动文本块，其他内容块及其相对顺序保持不变。
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
    // 头尾预算是所有文本块合起来算的：字符计数和省略标记的推进必须写在同一段顺序代码里，没法拆成每块独立的过滤或映射。
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
