use super::message::{ModelMessage, ModelRole};
use super::reasoning::ProviderReasoningReplay;
use super::tool::ModelToolCall;
use super::usage::ModelUsage;
use crate::error::ModelError;
use serde::{Deserialize, Serialize};

/// 跨 Chat Completions 与 Responses 归一化的类型化终态原因。
///
/// wire 兼容的 `finish_reason` 字符串仍为 v1 调用方序列化；此枚举是该
/// 字段的唯一控制流解释。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelStopReason {
    Stop,
    Length,
}

/// 模型提供方 turn 产生了有效完成，还是未通过校验。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelTurnStatus {
    Success,
    Failed,
    Invalid,
}

/// 模型提供方完成结果及其配对的已解析 tool call、用量、校验和错误状态。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelTurnResponse {
    pub request_id: String,
    pub response_id: String,
    pub status: ModelTurnStatus,
    pub assistant_message: Option<ModelMessage>,
    pub usage: ModelUsage,
    pub finish_reason: Option<String>,
    pub error: Option<ModelError>,
    pub provider_name: Option<String>,
    pub model_name: Option<String>,
    /// 内部 opaque reasoning continuation 状态；不序列化到 app-server
    /// 或 trace/evidence 投影。
    #[serde(skip)]
    pub provider_reasoning_history: Vec<ProviderReasoningReplay>,
}

impl ModelTurnResponse {
    /// 构造已完成的模型响应。
    pub fn completed(
        request_id: impl Into<String>,
        response_id: impl Into<String>,
        content: impl Into<String>,
    ) -> Self {
        Self {
            request_id: request_id.into(),
            response_id: response_id.into(),
            status: ModelTurnStatus::Success,
            assistant_message: Some(ModelMessage::text(ModelRole::Assistant, content)),
            usage: ModelUsage::default(),
            finish_reason: None,
            error: None,
            provider_name: None,
            model_name: None,
            provider_reasoning_history: Vec::new(),
        }
    }

    /// 响应携带的已解析 tool calls；唯一存储于 assistant message 内。
    pub fn tool_calls(&self) -> &[ModelToolCall] {
        self.assistant_message
            .as_ref()
            .map(|message| message.tool_calls.as_slice())
            .unwrap_or(&[])
    }

    /// 解释 provider finish reason，不向调用方暴露字符串匹配。
    pub fn stop_reason(&self) -> Option<ModelStopReason> {
        match self.finish_reason.as_deref() {
            Some("length") => Some(ModelStopReason::Length),
            Some("stop" | "tool_calls" | "function_call") => Some(ModelStopReason::Stop),
            _ => None,
        }
    }

    /// provider 是否因输出预算耗尽而停止。
    pub fn is_length_truncated(&self) -> bool {
        self.stop_reason() == Some(ModelStopReason::Length)
    }
}
