use super::message::ModelMessage;
use super::reasoning::ProviderReasoningReplay;
use super::tool::ModelToolSchema;
use serde::{Deserialize, Serialize};

/// `AgentLoop` 为完成请求提供的可选模型参数。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelPreferences {
    pub model_name: Option<String>,
    pub max_output_tokens: Option<u32>,
}

/// 传给模型提供方的完整模型请求，包括可见 tool。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelTurnRequest {
    pub request_id: String,
    pub messages: Vec<ModelMessage>,
    pub tools: Vec<ModelToolSchema>,
    pub model_preferences: ModelPreferences,
    /// 内部 provider continuation 状态；刻意从所有公开/请求 schema 中省略，
    /// 仅由适配器消费。
    #[serde(skip)]
    pub provider_reasoning_history: Vec<ProviderReasoningReplay>,
}

impl ModelTurnRequest {
    /// 创建模型 turn 请求。
    pub fn new(request_id: impl Into<String>, messages: Vec<ModelMessage>) -> Self {
        Self {
            request_id: request_id.into(),
            messages,
            tools: Vec::new(),
            model_preferences: ModelPreferences::default(),
            provider_reasoning_history: Vec::new(),
        }
    }
}
