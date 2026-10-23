use super::ModelUsage;
use super::message::Message;
use crate::ProviderError;

/// 把 Chat Completions 与 Responses 两种协议统一后的停止原因。
#[derive(Debug, Clone, PartialEq)]
pub enum ModelStopReason {
    Stop,
    Length,
    Failed(ProviderError),
}

/// 提供方返回的完成结果，连同已解析的工具调用和用量。
///
/// 失败和取消也返回已收到的正文；调用方按停止原因决定保存、重试或结束。
#[derive(Debug, Clone, PartialEq)]
pub struct ModelTurnResponse {
    pub assistant_message: Message,
    /// 输入与输出计数都有效上报时才有值；未知用量不当成零。
    pub usage: Option<ModelUsage>,
    pub stop_reason: ModelStopReason,
}

impl ModelTurnResponse {
    /// 请求失败时保留已生成的消息，用量保持未知。
    pub fn failed(error: ProviderError, assistant_message: Message) -> Self {
        Self {
            assistant_message,
            usage: None,
            stop_reason: ModelStopReason::Failed(error),
        }
    }

    /// 失败原因；取消使用提供方的 Cancelled 分类。
    pub fn error(&self) -> Option<&ProviderError> {
        match &self.stop_reason {
            ModelStopReason::Failed(error) => Some(error),
            ModelStopReason::Stop | ModelStopReason::Length => None,
        }
    }

    /// 提供方是否因为输出额度用尽而停下。
    pub fn is_length_truncated(&self) -> bool {
        self.stop_reason == ModelStopReason::Length
    }
}
