use super::reasoning::ProviderReasoningReplay;
use super::tool::ModelToolCall;
use serde::{Deserialize, Serialize};

/// 模型对话历史里可以出现的角色。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelRole {
    System,
    Developer,
    User,
    Assistant,
    Tool,
}

/// 发往模型提供方的一条消息，含继续本轮对话所需的工具调用信息。
#[derive(Debug, Clone, PartialEq)]
pub struct ModelMessage {
    pub role: ModelRole,
    pub content: String,
    /// 顺序对应图片说明的内联 data URL，由 Agent 从已校验快照生成。
    pub images: Vec<String>,
    pub tool_call_id: Option<String>,
    pub tool_calls: Vec<ModelToolCall>,
    /// 原始 assistant 消息的私有续接数据，只有模型适配器会读它。
    /// 公开的请求详情里没有这个字段；要落盘时由 Session 的 assistant 消息负责。
    pub provider_reasoning_replay: Option<ProviderReasoningReplay>,
}

impl ModelMessage {
    /// 构造一条纯文本消息。
    pub fn text(role: ModelRole, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
            images: Vec::new(),
            tool_call_id: None,
            tool_calls: Vec::new(),
            provider_reasoning_replay: None,
        }
    }
}
