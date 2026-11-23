use super::message::Message;
use app_core::ToolDefinition;

#[derive(Debug, Clone, PartialEq)]
pub struct ModelTurnRequest {
    pub messages: Vec<Message>,
    pub images: std::collections::HashMap<String, String>,
    pub tools: Vec<ToolDefinition>,
    pub max_output_tokens: u32,
    pub prompt_cache_key: Option<String>,
}
