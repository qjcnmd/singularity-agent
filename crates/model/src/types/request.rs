use super::message::ModelMessage;
use super::tool::ModelToolSchema;
use serde::{Deserialize, Serialize};

pub use singularity_protocol::RequestPreferences as ModelPreferences;

/// 一次完整的模型请求，含本次对模型可见的工具。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelTurnRequest {
    pub request_id: String,
    pub messages: Vec<ModelMessage>,
    pub tools: Vec<ModelToolSchema>,
    pub model_preferences: ModelPreferences,
}

impl ModelTurnRequest {
    /// 构造一次模型轮次请求。
    pub fn new(request_id: impl Into<String>, messages: Vec<ModelMessage>) -> Self {
        Self {
            request_id: request_id.into(),
            messages,
            tools: Vec::new(),
            model_preferences: ModelPreferences::default(),
        }
    }
}
