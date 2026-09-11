use super::message::ModelMessage;
use super::tool::ModelToolSchema;
use serde::{Deserialize, Serialize};

pub use singularity_protocol::RequestPreferences as ModelPreferences;

/// 传给模型提供方的完整模型请求，包括可见 tool。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelTurnRequest {
    pub request_id: String,
    pub messages: Vec<ModelMessage>,
    pub tools: Vec<ModelToolSchema>,
    pub model_preferences: ModelPreferences,
}

impl ModelTurnRequest {
    /// 创建模型 turn 请求。
    pub fn new(request_id: impl Into<String>, messages: Vec<ModelMessage>) -> Self {
        Self {
            request_id: request_id.into(),
            messages,
            tools: Vec::new(),
            model_preferences: ModelPreferences::default(),
        }
    }
}
