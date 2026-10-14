//! 轨迹里提示词与工具定义的快照及引用解析。
use super::manager::SessionData;
use super::{LedgerRecord, SessionEntry};
use serde::{Deserialize, Serialize};
use singularity_model::{ModelMessage, ModelPreferences, ModelRole, ModelToolSchema};
use singularity_protocol::{
    ModelRequestDefinitions, ModelRequestReference, ModelRequestSnapshot, RequestMessage, RequestPreferences,
    RequestTool,
};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestDefinitions {
    messages: Vec<SavedInstruction>,
    tools: Vec<SavedTool>,
}

// 保持已有 JSONL 字段名；桌面 DTO 或 Provider 输入的变化不改变保存格式。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SavedInstruction {
    role: ModelRole,
    content: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SavedTool {
    name: String,
    description: String,
    parameters_schema: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SavedPreferences {
    max_output_tokens: Option<u32>,
}

impl RequestDefinitions {
    pub(super) fn snapshot(
        &self,
        definitions_id: &str,
        model_preferences: &ModelPreferences,
    ) -> Box<ModelRequestSnapshot> {
        Box::new(ModelRequestSnapshot {
            definitions_id: definitions_id.to_string(),
            messages: self.public_messages(),
            tools: self.public_tools(),
            model_preferences: RequestPreferences {
                max_output_tokens: model_preferences.max_output_tokens,
            },
        })
    }

    pub(crate) fn new(messages: &[ModelMessage], tools: Vec<ModelToolSchema>) -> Self {
        Self {
            messages: messages
                .iter()
                .map(|message| SavedInstruction {
                    role: message.role,
                    content: message.content.clone(),
                })
                .collect(),
            tools: tools
                .into_iter()
                .map(|tool| SavedTool {
                    name: tool.name,
                    description: tool.description,
                    parameters_schema: tool.parameters_schema,
                })
                .collect(),
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

    /// 从保存的定义恢复压缩请求的工具输入。
    pub(crate) fn model_tools(&self) -> Vec<ModelToolSchema> {
        self.tools
            .iter()
            .map(|tool| ModelToolSchema {
                name: tool.name.clone(),
                description: tool.description.clone(),
                parameters_schema: tool.parameters_schema.clone(),
            })
            .collect()
    }

    fn public_messages(&self) -> Vec<RequestMessage> {
        self.messages
            .iter()
            .map(|message| RequestMessage {
                role: message.role,
                content: message.content.clone(),
            })
            .collect()
    }

    fn public_tools(&self) -> Vec<RequestTool> {
        self.tools
            .iter()
            .map(|tool| RequestTool {
                name: tool.name.clone(),
                description: tool.description.clone(),
                parameters_schema: tool.parameters_schema.clone(),
            })
            .collect()
    }
}

/// 一次请求引用到的定义，以及这次请求的偏好。请求身份只由外层
/// `RequestObservation` 保存，读 header 的调用方因此只需维持一处一致。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestContext {
    pub definitions: String,
    model_preferences: SavedPreferences,
}

impl RequestContext {
    pub(super) fn new(definitions: String, preferences: ModelPreferences) -> Self {
        Self {
            definitions,
            model_preferences: SavedPreferences {
                max_output_tokens: preferences.max_output_tokens,
            },
        }
    }

    /// 将保存的请求偏好转换为桌面轨迹引用。
    pub fn public_reference(&self) -> ModelRequestReference {
        ModelRequestReference {
            definitions_id: self.definitions.clone(),
            model_preferences: RequestPreferences {
                max_output_tokens: self.model_preferences.max_output_tokens,
            },
        }
    }
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

    /// 读取已保存定义的公开快照；调用方可让同页请求共用它。
    pub fn request_definitions(&self, definitions_id: &str) -> ModelRequestDefinitions {
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
                } if id == definitions_id => Some(definitions),
                _ => None,
            })
            .expect("request references an earlier definition record");
        ModelRequestDefinitions {
            messages: definitions.public_messages(),
            tools: definitions.public_tools(),
        }
    }
}
