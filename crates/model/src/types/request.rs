use super::message::Message;
use singularity_core::ToolDefinition;

/// 发给提供方的一次模型请求，包含上下文消息、已加载的图片 data URL、工具定义和输出上限。
#[derive(Debug, Clone, PartialEq)]
pub struct ModelTurnRequest {
    pub messages: Vec<Message>,
    /// 已读取的图片 data URL，按消息中的附件 ID 查找。
    pub images: std::collections::HashMap<String, String>,
    pub tools: Vec<ToolDefinition>,
    pub max_output_tokens: u32,
}
