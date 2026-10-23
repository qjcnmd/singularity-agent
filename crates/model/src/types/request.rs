use super::message::Message;
use super::tool::ModelToolSchema;

/// 与协议无关的一次模型请求；每次发送尝试的 ID 由请求执行代码分配。
#[derive(Debug, Clone, PartialEq)]
pub struct ModelTurnRequest {
    pub messages: Vec<Message>,
    /// 已读取的图片 data URL，按消息中的附件 ID 查找。
    pub images: std::collections::HashMap<String, String>,
    pub tools: Vec<ModelToolSchema>,
    pub max_output_tokens: u32,
}
