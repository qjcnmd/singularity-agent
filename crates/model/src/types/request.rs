use super::message::ModelMessage;
use super::reasoning::ProviderReasoningReplay;
use super::tool::{ModelToolSchema, ToolChoicePolicy};
use crate::ModelPreferences;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// 传给模型提供方的完整模型请求，包括可见 tool 和 tool 策略。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ModelTurnRequest {
    pub request_id: String,
    pub messages: Vec<ModelMessage>,
    pub tools: Vec<ModelToolSchema>,
    pub tool_choice: ToolChoicePolicy,
    pub model_preferences: ModelPreferences,
    /// Internal provider continuation state. It is deliberately omitted from
    /// all public/request schemas and is only consumed by the adapter.
    #[serde(skip)]
    #[schemars(skip)]
    pub provider_reasoning_history: Vec<ProviderReasoningReplay>,
}

impl ModelTurnRequest {
    /// 创建模型 turn 请求。
    pub fn new(request_id: impl Into<String>, messages: Vec<ModelMessage>) -> Self {
        Self {
            request_id: request_id.into(),
            messages,
            tools: Vec::new(),
            tool_choice: ToolChoicePolicy::default(),
            model_preferences: ModelPreferences::default(),
            provider_reasoning_history: Vec::new(),
        }
    }
}
