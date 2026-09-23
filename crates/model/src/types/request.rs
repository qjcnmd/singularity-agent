use super::message::ModelMessage;
use super::tool::ModelToolSchema;
use serde::{Deserialize, Serialize};

pub use singularity_protocol::RequestPreferences as ModelPreferences;

/// Provider 无关的模型输入；执行层为每次发送分配 attempt 身份。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelTurnRequest {
    pub messages: Vec<ModelMessage>,
    pub tools: Vec<ModelToolSchema>,
    pub model_preferences: ModelPreferences,
}

impl ModelTurnRequest {
    /// 构造一次模型轮次输入。
    pub fn new(messages: Vec<ModelMessage>) -> Self {
        Self {
            messages,
            tools: Vec::new(),
            model_preferences: ModelPreferences::default(),
        }
    }
}
