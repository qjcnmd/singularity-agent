use super::message::{ModelMessage, ModelRole};
use super::tool::ModelToolCall;
use super::usage::ModelUsage;
use serde::{Deserialize, Serialize};

/// 跨 Chat Completions 与 Responses 归一化的类型化终态原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelStopReason {
    Stop,
    Length,
}

/// 模型提供方完成结果及其配对的已解析 tool call 与用量。
///
/// 校验失败不在此类型内表达：不可恢复的失败以 crate::error::ProviderError
/// 从 provider 边界返回；可恢复的畸形参数交由工具派发层产出模型可见结果。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelTurnResponse {
    pub assistant_message: ModelMessage,
    /// Provider explicitly returned displayable thinking text or reasoning summary.
    /// Independent of opaque continuation data and whether tools were called.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub thinking: String,
    pub usage: ModelUsage,
    pub stop_reason: Option<ModelStopReason>,
}

impl ModelTurnResponse {
    /// 构造已完成的模型响应。
    pub fn completed(content: impl Into<String>) -> Self {
        Self {
            assistant_message: ModelMessage::text(ModelRole::Assistant, content),
            thinking: String::new(),
            usage: ModelUsage::default(),
            stop_reason: None,
        }
    }

    /// 响应携带的已解析 tool calls；唯一存储于 assistant message 内。
    pub fn tool_calls(&self) -> &[ModelToolCall] {
        &self.assistant_message.tool_calls
    }

    /// provider 是否因输出预算耗尽而停止。
    pub fn is_length_truncated(&self) -> bool {
        self.stop_reason == Some(ModelStopReason::Length)
    }
}
