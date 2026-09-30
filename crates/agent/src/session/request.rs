//! 轨迹里提示词与工具定义的快照；对话内容不在这里建索引。
use super::manager::SessionData;
use super::{LedgerRecord, SessionEntry};
use serde::{Deserialize, Serialize};
use singularity_model::{ModelRole, ModelTurnRequest};
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

    pub(crate) fn from_request(request: &ModelTurnRequest) -> Self {
        Self {
            messages: request
                .messages
                .iter()
                .filter_map(|m| {
                    let role = match m.role {
                        ModelRole::System => "system",
                        ModelRole::Developer => "developer",
                        // 对话消息（用户/助手/工具）不属于定义快照。
                        _ => return None,
                    };
                    Some(RequestMessage {
                        role: role.into(),
                        content: m.content.clone(),
                    })
                })
                .collect(),
            tools: request.tools.clone(),
        }
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
    pub(super) fn observe_definitions(&mut self, position: usize) {
        if let SessionEntry::Record {
            id,
            record: LedgerRecord::RequestDefinitions { .. },
            ..
        } = &self.entries[position]
        {
            self.definitions.insert(id.clone(), position);
        }
    }

    /// 连续请求复用最近一份定义；内容变化时追加新定义。
    pub(super) fn find_definitions(&self, definitions: &RequestDefinitions) -> Option<String> {
        let position = *self.definitions.values().max()?;
        let SessionEntry::Record {
            id,
            record:
                LedgerRecord::RequestDefinitions {
                    definitions: previous,
                },
            ..
        } = &self.entries[position]
        else {
            unreachable!("definition index references its ledger record")
        };
        (previous == definitions).then(|| id.clone())
    }

    /// 展开请求记录引用的提示词与工具；不涉及对话内容。
    pub fn request_head(&self, context: &RequestContext) -> Box<ModelRequestSnapshot> {
        // 定义先于引用写入；追加失败后写者不再接受后续记录。
        let position = self.definitions[&context.definitions];
        let SessionEntry::Record {
            record: LedgerRecord::RequestDefinitions { definitions },
            ..
        } = &self.entries[position]
        else {
            unreachable!()
        };
        definitions.snapshot(&context.definitions, &context.model_preferences)
    }
}
