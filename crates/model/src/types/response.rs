use super::message::ModelMessage;
use super::usage::ModelUsage;

/// 把 Chat Completions 与 Responses 两种协议统一后的停止原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelStopReason {
    Stop,
    Length,
}

/// 提供方返回的完成结果，连同已解析的工具调用和用量。
///
/// 校验失败不在这里表达：无法恢复的失败以 crate::error::ProviderError 从
/// provider 边界返回；可恢复的参数畸形由工具派发层转成模型可见的结果。
#[derive(Debug, Clone, PartialEq)]
pub struct ModelTurnResponse {
    pub assistant_message: ModelMessage,
    /// 提供方明确返回、可以展示的思考文本或推理摘要，与不透明的续接数据和本轮是否发生工具调用无关。
    pub thinking: String,
    pub usage: ModelUsage,
    pub stop_reason: ModelStopReason,
}

impl ModelTurnResponse {
    /// 提供方是否因为输出额度用尽而停下。
    pub fn is_length_truncated(&self) -> bool {
        self.stop_reason == ModelStopReason::Length
    }
}
