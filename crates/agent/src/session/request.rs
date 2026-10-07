//! 轨迹里提示词与工具定义的快照及引用解析。
use super::manager::SessionData;
use super::{LedgerRecord, SessionEntry};
use serde::{Deserialize, Serialize};
use singularity_model::{ModelMessage, ModelToolSchema};
use singularity_protocol::{ModelRequestSnapshot, RequestMessage, RequestPreferences, RequestTool};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestDefinitions {
    pub messages: Vec<RequestMessage>,
    pub tools: Vec<RequestTool>,
}

impl RequestDefinitions {
    pub(super) fn snapshot(
        &self,
        definitions_id: &str,
        model_preferences: &RequestPreferences,
    ) -> Box<ModelRequestSnapshot> {
        Box::new(ModelRequestSnapshot {
            definitions_id: definitions_id.to_string(),
            messages: self.messages.clone(),
            tools: self.tools.clone(),
            model_preferences: model_preferences.clone(),
        })
    }

    pub(crate) fn new(messages: &[ModelMessage], tools: Vec<ModelToolSchema>) -> Self {
        Self {
            messages: messages
                .iter()
                .map(|message| RequestMessage {
                    role: message.role,
                    content: message.content.clone(),
                })
                .collect(),
            tools,
        }
    }

    /// 指令和工具定义共同占用的输入预算。
    pub(crate) fn estimated_tokens(&self) -> u64 {
        let instructions = self
            .messages
            .iter()
            .map(|message| super::context::estimate_tokens_of(&message.content) + 4)
            .sum::<u64>();
        let tools = if self.tools.is_empty() {
            0
        } else {
            super::context::estimate_tokens_of(
                &serde_json::to_string(&self.tools).expect("tool schemas are serializable"),
            ) + 4
        };
        instructions + tools
    }

    /// 从已解析的定义恢复完整指令前缀。
    pub(crate) fn model_messages(&self) -> Vec<ModelMessage> {
        self.messages
            .iter()
            .map(|message| ModelMessage::text(message.role, &message.content))
            .collect()
    }
}

/// 一次请求引用到的定义，以及这次请求的偏好。请求身份只由外层
/// `RequestObservation` 保存，读 header 的调用方因此只需维持一处一致。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestContext {
    pub definitions: String,
    pub model_preferences: RequestPreferences,
}

impl SessionData {
    /// 最近一次请求实际使用的指令与工具，供独立压缩复用。
    pub(crate) fn latest_request_definitions(&self) -> Option<(&str, &RequestDefinitions)> {
        self.entries.iter().rev().find_map(|entry| match entry {
            SessionEntry::Record {
                id,
                record: LedgerRecord::RequestDefinitions { definitions },
                ..
            } => Some((id.as_str(), definitions)),
            _ => None,
        })
    }

    /// 展开请求记录引用的提示词与工具；不涉及对话内容。
    pub fn request_head(&self, context: &RequestContext) -> Box<ModelRequestSnapshot> {
        // 定义先于引用写入；追加失败后写者不再接受后续记录。
        let definitions = self
            .entries
            .iter()
            .rev()
            .find_map(|entry| match entry {
                SessionEntry::Record {
                    id,
                    record: LedgerRecord::RequestDefinitions { definitions },
                    ..
                } if id == &context.definitions => Some(definitions),
                _ => None,
            })
            .expect("request references an earlier definition record");
        definitions.snapshot(&context.definitions, &context.model_preferences)
    }
}
