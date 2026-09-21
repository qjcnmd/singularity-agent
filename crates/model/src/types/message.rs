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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelMessage {
    pub role: ModelRole,
    pub content: String,
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ModelToolCall>,
    /// 原始 assistant 消息的私有续接数据，只有模型适配器会读它。
    /// 公开的请求详情里没有这个字段；要落盘时由 Session 的 assistant 消息负责。
    #[serde(skip)]
    pub provider_reasoning_replay: Option<ProviderReasoningReplay>,
}

impl ModelMessage {
    /// 构造一条纯文本消息。
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
