//! 会话消息与内容块数据模型。
//!
//! 支持富文本内容块（纯文本 Text、思考链 Thinking、工具调用 ToolCall）
//! 以及工具执行结果 ToolResult，确保单次模型交互的完整语义（含推理过程与多工具调用）
//! 能够精确持久化与协议重放。
//!
//! AgentMessage 以角色为标签的枚举承载消息体：每个角色只携带其合法字段，
//! 编译器拒绝「user 消息带 toolCallId」一类非法组合；序列化 wire 形状与历史
//! 平铺格式逐字节一致（tag = "role" + 变体级 camelCase，键序由 serde_json
//! Map 排序稳定），session 层的 JSONL 字节夹具固化该契约。

use singularity_model::{
    ModelStopReason, ModelToolCall, ModelTurnResponse, ProviderReasoningReplay,
};

use crate::tools::ToolExecution;

/// 消息体内的结构化内容块。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ContentBlock {
    /// 纯文本内容块（{"type":"text","text":...}）。
    Text { text: String },
    /// 思考/推理链内容块（{"type":"thinking","thinking":...,"signature":...}）。
    Thinking {
        thinking: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    /// 工具调用描述块（{"type":"tool_call","id":...,"name":...,"args":...}）。
    ToolCall {
        id: String,
        name: String,
        args: serde_json::Value,
    },
}

impl ContentBlock {
    pub(crate) fn from_model_tool_call(call: &ModelToolCall) -> Self {
        Self::ToolCall {
            id: call.tool_call_id.clone(),
            name: call.tool_name.clone(),
            args: call.arguments.clone(),
        }
    }

    pub(crate) fn to_model_tool_call(&self) -> Option<ModelToolCall> {
        let Self::ToolCall { id, name, args } = self else {
            return None;
        };
        if id.trim().is_empty() || name.trim().is_empty() {
            return None;
        }
        Some(ModelToolCall {
            tool_call_id: id.clone(),
            tool_name: name.clone(),
            arguments: args.clone(),
            raw_arguments: serde_json::to_string(args).unwrap_or_default(),
            validation_errors: Vec::new(),
        })
    }
}

/// 核心会话消息数据结构：以角色为标签的枚举，每个角色只携带其合法字段。
///
/// wire 形状与历史平铺格式逐字节一致（role 为内部 tag）：序列化输出
/// {"content":...,"role":"user"} / {"role":"assistant",...,"stopReason":...}
/// / {"role":"toolResult",...,"toolCallId":...,"toolName":...,"isError":...}。
/// deny_unknown_fields 使消息内未知字段写入即拒绝。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "role", rename_all = "camelCase", deny_unknown_fields)]
pub enum AgentMessage {
    #[serde(rename_all = "camelCase")]
    User { content: Vec<ContentBlock> },
    #[serde(rename_all = "camelCase")]
    Assistant {
        content: Vec<ContentBlock>,
        /// Provider 给出的 assistant 停止原因。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stop_reason: Option<ModelStopReason>,
        /// 模型提供方私有推理状态（用于支持 Responses 等协议的推理连续性重放）。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_reasoning_replay: Option<ProviderReasoningReplay>,
    },
    #[serde(rename_all = "camelCase")]
    ToolResult {
        content: Vec<ContentBlock>,
        /// 对应的工具调用 ID。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_call_id: Option<String>,
        /// 对应的工具名称。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_name: Option<String>,
        /// 工具执行是否失败标志。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
        /// Observed tool execution time; absent for unknown or unexecuted outcomes.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
        /// File changes for display; not included in content sent to the model.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        diff: Option<String>,
    },
}

impl AgentMessage {
    /// 消息内容的切片视图。
    pub fn content(&self) -> &[ContentBlock] {
        match self {
            Self::User { content }
            | Self::Assistant { content, .. }
            | Self::ToolResult { content, .. } => content,
        }
    }

    /// 提取并拼接消息内部所有纯文本块的内容视图。
    pub fn content_text(&self) -> String {
        let mut text = String::new();
        for block in self.content() {
            if let ContentBlock::Text { text: part } = block {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(part);
            }
        }
        text
    }

    /// 获取消息包含的所有工具调用块引用。
    pub fn tool_calls(&self) -> impl Iterator<Item = &ContentBlock> {
        self.content()
            .iter()
            .filter(|block| matches!(block, ContentBlock::ToolCall { .. }))
    }

    /// 获取消息包含的所有思考推理块引用。
    pub fn thinking_blocks(&self) -> impl Iterator<Item = &ContentBlock> + '_ {
        self.content()
            .iter()
            .filter(|block| matches!(block, ContentBlock::Thinking { .. }))
    }

    /// 对应工具调用 ID；仅 toolResult 消息携带。
    pub fn tool_call_id(&self) -> Option<&String> {
        match self {
            Self::ToolResult { tool_call_id, .. } => tool_call_id.as_ref(),
            _ => None,
        }
    }

    /// provider 推理重放；仅 assistant 消息携带。
    pub fn provider_reasoning_replay(&self) -> Option<&ProviderReasoningReplay> {
        match self {
            Self::Assistant {
                provider_reasoning_replay,
                ..
            } => provider_reasoning_replay.as_ref(),
            _ => None,
        }
    }
}

/// 压缩摘要节点进入模型上下文时的说明前缀。
pub const COMPACTION_SUMMARY_PREFIX: &str = "This checkpoint summarizes earlier conversation history. Treat it as established background and continue directly from the messages that follow without acknowledging the checkpoint.\n\n<compacted-summary>\n";
/// 压缩摘要节点进入模型上下文时的闭合后缀。
pub const COMPACTION_SUMMARY_SUFFIX: &str = "\n</compacted-summary>";

pub(crate) fn user_message(text: &str) -> AgentMessage {
    AgentMessage::User {
        content: vec![ContentBlock::Text {
            text: text.to_string(),
        }],
    }
}

/// 一次模型响应投影为一条 assistant 消息（v4 内容块）：
/// thinking 块（随会话持久化）→ 文本块 → 全部 tool_call 块。
pub(crate) fn assistant_response_message(response: &ModelTurnResponse) -> AgentMessage {
    let mut content = Vec::new();
    if !response.thinking.is_empty() {
        content.push(ContentBlock::Thinking {
            thinking: response.thinking.clone(),
            signature: None,
        });
    }
    let assistant_text = response.assistant_message.content.clone();
    if !assistant_text.is_empty() {
        content.push(ContentBlock::Text {
            text: assistant_text,
        });
    }
    for call in response.tool_calls() {
        content.push(ContentBlock::from_model_tool_call(call));
    }
    AgentMessage::Assistant {
        content,
        stop_reason: response.stop_reason,
        provider_reasoning_replay: response.assistant_message.provider_reasoning_replay.clone(),
    }
}

pub(crate) fn tool_result_message(
    tool_call_id: &str,
    tool_name: &str,
    execution: &ToolExecution,
) -> AgentMessage {
    AgentMessage::ToolResult {
        content: vec![ContentBlock::Text {
            text: execution.content.clone(),
        }],
        tool_call_id: Some(tool_call_id.to_string()),
        tool_name: Some(tool_name.to_string()),
        is_error: Some(execution.is_error),
        duration_ms: execution.duration_ms,
        diff: execution.diff.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completed_reply_keeps_displayed_thinking_without_replay() {
        let mut response = ModelTurnResponse::completed("answer");
        response.thinking = "visible thinking".into();
        let message = assistant_response_message(&response);
        assert!(
            matches!(&message.content()[0], ContentBlock::Thinking { thinking, .. } if thinking == "visible thinking")
        );
        assert!(matches!(&message.content()[1], ContentBlock::Text { text } if text == "answer"));
        assert!(message.provider_reasoning_replay().is_none());
    }
}
