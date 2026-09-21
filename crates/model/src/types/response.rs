use super::message::{ModelMessage, ModelRole};
use super::tool::ModelToolCall;
use super::usage::ModelUsage;
use serde::{Deserialize, Serialize};

/// 把 Chat Completions 与 Responses 两种协议统一后的停止原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelStopReason {
    Stop,
    Length,
}

/// 提供方返回的完成结果，连同已解析的工具调用和用量。
///
/// 校验失败不在这里表达：无法恢复的失败以 crate::error::ProviderError 从
/// provider 边界返回；可恢复的参数畸形由工具派发层转成模型可见的结果。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelTurnResponse {
    pub assistant_message: ModelMessage,
    /// 提供方明确返回的、可以展示的思考文本或推理摘要；与不透明的续接数据、以及本轮是否
    /// 发生工具调用都无关。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub thinking: String,
    pub usage: ModelUsage,
    pub stop_reason: Option<ModelStopReason>,
}

impl ModelTurnResponse {
    /// 构造一条已完成的模型响应。
    pub fn completed(content: impl Into<String>) -> Self {
        Self {
            assistant_message: ModelMessage::text(ModelRole::Assistant, content),
            thinking: String::new(),
            usage: ModelUsage::default(),
            stop_reason: None,
        }
    }

    /// 本次响应已解析的工具调用；它只存放在 assistant 消息里。
    pub fn tool_calls(&self) -> &[ModelToolCall] {
        &self.assistant_message.tool_calls
    }

    /// 提供方是否因为输出额度用尽而停下。
    pub fn is_length_truncated(&self) -> bool {
        self.stop_reason == Some(ModelStopReason::Length)
    }
}
