//! 会话消息与内容块数据模型。
//!
//! 支持富文本内容块（纯文本 `Text`、思考链 `Thinking`、工具调用 `ToolCall`）
//! 以及工具执行结果 `ToolResult`，确保单次模型交互的完整语义（含推理过程与多工具调用）
//! 能够精确持久化与协议重放。

/// 交由模型提供方执行的模型层消息类型别名。
pub type LlmMessage = singularity_model::ModelMessage;

use singularity_model::{ModelTurnResponse, ProviderReasoningReplay};

use crate::tools::ToolExecution;

/// 会话消息角色枚举。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AgentMessageRole {
    /// 用户输入消息。
    User,
    /// 模型助手响应消息。
    Assistant,
    /// 工具执行结果回填消息。
    ToolResult,
}

/// 消息体内的结构化内容块。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    /// 纯文本内容块（`{"type":"text","text":...}`）。
    Text { text: String },
    /// 思考/推理链内容块（`{"type":"thinking","thinking":...,"signature":...}`）。
    Thinking {
        thinking: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    /// 工具调用描述块（`{"type":"tool_call","id":...,"name":...,"args":...}`）。
    ToolCall {
        id: String,
        name: String,
        args: serde_json::Value,
    },
}

/// 核心会话消息数据结构。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentMessage {
    pub role: AgentMessageRole,
    pub content: Vec<ContentBlock>,
    /// 模型提供方私有推理状态（用于支持 Responses 等协议的推理连续性重放）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_reasoning_replay: Option<ProviderReasoningReplay>,
    /// 对应的工具调用 ID（仅 ToolResult 角色消息使用）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// 对应的工具名称（仅 ToolResult 角色消息使用）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    /// 工具执行是否失败标志。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
    /// Unix 毫秒时间戳。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<u64>,
}

impl AgentMessage {
    /// 构造纯文本内容消息。
    pub fn text(role: AgentMessageRole, content: impl Into<String>) -> Self {
        Self {
            role,
            content: vec![ContentBlock::Text {
                text: content.into(),
            }],
            provider_reasoning_replay: None,
            tool_call_id: None,
            tool_name: None,
            is_error: None,
            timestamp: None,
        }
    }

    /// 提取并拼接消息内部所有纯文本块的内容视图。
    pub fn content_text(&self) -> String {
        let mut text = String::new();
        for block in &self.content {
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
    pub fn tool_calls(&self) -> Vec<&ContentBlock> {
        self.content
            .iter()
            .filter(|block| matches!(block, ContentBlock::ToolCall { .. }))
            .collect()
    }

    /// 判断消息是否包含至少一个工具调用块。
    pub fn has_tool_calls(&self) -> bool {
        self.content
            .iter()
            .any(|block| matches!(block, ContentBlock::ToolCall { .. }))
    }

    /// 获取消息包含的所有思考推理块引用。
    pub fn thinking_blocks(&self) -> Vec<&ContentBlock> {
        self.content
            .iter()
            .filter(|block| matches!(block, ContentBlock::Thinking { .. }))
            .collect()
    }
}

/// 压缩摘要节点进入模型上下文时的说明前缀。
pub const COMPACTION_SUMMARY_PREFIX: &str = "The conversation history before this point was compacted into the following summary:\n\n<summary>\n";
/// 压缩摘要节点进入模型上下文时的闭合后缀。
pub const COMPACTION_SUMMARY_SUFFIX: &str = "\n</summary>";

pub(crate) fn user_message(text: &str) -> AgentMessage {
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

/// 一次模型响应投影为一条 assistant 消息（v4 内容块）：
/// thinking 块（N2，随会话持久化）→ 文本块 → 全部 tool_call 块。
pub(crate) fn assistant_response_message(response: &ModelTurnResponse) -> AgentMessage {
    let mut content = Vec::new();
    if let Some(thinking) = reasoning_text_from_replay(&response.provider_reasoning_history) {
        content.push(ContentBlock::Thinking {
            thinking,
            signature: None,
        });
    }
    let assistant_text = response
        .assistant_message
        .as_ref()
        .map(|message| message.content.clone())
        .unwrap_or_default();
    if !assistant_text.is_empty() {
        content.push(ContentBlock::Text {
            text: assistant_text,
        });
    }
    for call in &response.tool_calls {
        content.push(ContentBlock::ToolCall {
            id: call.tool_call_id.clone(),
            name: call.tool_name.clone(),
            args: call.arguments.clone(),
        });
    }
    AgentMessage {
        role: AgentMessageRole::Assistant,
        content,
        provider_reasoning_replay: response.provider_reasoning_history.first().cloned(),
        tool_call_id: None,
        tool_name: None,
        is_error: None,
        timestamp: None,
    }
}

/// 从响应携带的 provider reasoning replay 提取可展示的推理文本。
/// 只有 Chat/DeepSeek 明确返回的 `reasoning_content` 属于公开 thinking；
/// Responses replay 是 provider-private opaque state，即使其中包含 summary
/// 文本也不得复制到公开 Session/history。
pub(crate) fn reasoning_text_from_replay(replay: &[ProviderReasoningReplay]) -> Option<String> {
    let first = replay.first()?;
    match first {
        ProviderReasoningReplay::Chat {
            reasoning_content, ..
        } if !reasoning_content.is_empty() => Some(reasoning_content.clone()),
        _ => None,
    }
}

pub(crate) fn tool_result_message(
    tool_call_id: &str,
    tool_name: &str,
    execution: &ToolExecution,
) -> AgentMessage {
    AgentMessage {
        role: AgentMessageRole::ToolResult,
        content: vec![ContentBlock::Text {
            text: execution.content.clone(),
        }],
        provider_reasoning_replay: None,
        tool_call_id: Some(tool_call_id.to_string()),
        tool_name: Some(tool_name.to_string()),
        is_error: Some(execution.is_error),
        timestamp: None,
    }
}
