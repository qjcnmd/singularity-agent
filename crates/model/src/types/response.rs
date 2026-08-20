use super::message::{ModelMessage, ModelRole};
use super::reasoning::ProviderReasoningReplay;
use super::tool::ModelToolCall;
use super::usage::{ModelUsage, ModelValidationResult};
use crate::error::ModelError;
use crate::provider::ProviderAttemptMetadata;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Typed terminal reason normalized across Chat Completions and Responses.
///
/// The wire-compatible `finish_reason` string remains serialized for v1
/// callers; this enum is the only control-flow interpretation of that field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ModelStopReason {
    Stop,
    Length,
}

/// 模型提供方 turn 产生了有效完成，还是未通过校验。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ModelTurnStatus {
    Success,
    Failed,
    Invalid,
}

/// 模型提供方完成结果及其配对的已解析 tool call、用量、校验和错误状态。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ModelTurnResponse {
    pub request_id: String,
    pub response_id: String,
    pub status: ModelTurnStatus,
    pub assistant_message: Option<ModelMessage>,
    pub tool_calls: Vec<ModelToolCall>,
    pub usage: ModelUsage,
    pub finish_reason: Option<String>,
    pub validation: Option<ModelValidationResult>,
    pub error: Option<ModelError>,
    pub provider_name: Option<String>,
    pub model_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_attempt_metadata: Option<ProviderAttemptMetadata>,
    /// 内部 opaque reasoning continuation state；never serialized to the
    /// app-server or trace/evidence projections.
    #[serde(skip)]
    #[schemars(skip)]
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
            tool_calls: Vec::new(),
            usage: ModelUsage::default(),
            finish_reason: None,
            validation: None,
            error: None,
            provider_name: None,
            model_name: None,
            provider_attempt_metadata: None,
            provider_reasoning_history: Vec::new(),
        }
    }

    /// Interpret the provider finish reason without exposing string matching to callers.
    pub fn stop_reason(&self) -> Option<ModelStopReason> {
        match self.finish_reason.as_deref() {
            Some("length") => Some(ModelStopReason::Length),
            Some("stop" | "tool_calls" | "function_call") => Some(ModelStopReason::Stop),
            _ => None,
        }
    }

    /// Return true when the provider stopped because its output budget was reached.
    pub fn is_length_truncated(&self) -> bool {
        self.stop_reason() == Some(ModelStopReason::Length)
    }
}
