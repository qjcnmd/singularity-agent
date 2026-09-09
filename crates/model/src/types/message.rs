use super::reasoning::ProviderReasoningReplay;
use super::tool::ModelToolCall;
use serde::{Deserialize, Serialize};

/// 面向模型的对话历史支持的角色。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelRole {
    System,
    Developer,
    User,
    Assistant,
    Tool,
}

/// 面向模型提供方的消息，包括继续 turn 所需的 tool call 元数据。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelMessage {
    pub role: ModelRole,
    pub content: String,
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ModelToolCall>,
    /// 原始 assistant 消息的私有协议续接；只由模型适配器消费。
    /// 公开请求详情不包含此字段，持久化由 Session 的 assistant 消息负责。
    #[serde(skip)]
    pub provider_reasoning_replay: Option<ProviderReasoningReplay>,
}

impl ModelMessage {
    /// 创建普通文本消息。
    pub fn text(role: ModelRole, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
            tool_call_id: None,
            tool_calls: Vec::new(),
            provider_reasoning_replay: None,
        }
    }
}
