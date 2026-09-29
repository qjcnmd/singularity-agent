use super::message::ModelMessage;
use super::tool::ModelToolSchema;

pub use singularity_protocol::RequestPreferences as ModelPreferences;

/// Provider 无关的模型输入；执行层为每次发送分配 attempt 身份。
#[derive(Debug, Clone, PartialEq)]
pub struct ModelTurnRequest {
    pub messages: Vec<ModelMessage>,
    pub tools: Vec<ModelToolSchema>,
    pub model_preferences: ModelPreferences,
}
