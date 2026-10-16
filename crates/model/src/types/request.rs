use super::message::ModelMessage;
use super::tool::ModelToolSchema;

/// 本次模型调用的输出限制，不包含桌面协议或保存格式。
#[derive(Debug, Clone, PartialEq)]
pub struct ModelPreferences {
    pub max_output_tokens: u32,
}

/// Provider 无关的模型输入；执行层为每次发送分配 attempt 身份。
#[derive(Debug, Clone, PartialEq)]
pub struct ModelTurnRequest {
    pub messages: Vec<ModelMessage>,
    pub tools: Vec<ModelToolSchema>,
    pub model_preferences: ModelPreferences,
}
