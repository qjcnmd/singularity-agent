//! 轨迹里提示词与工具定义的快照；对话内容不在这里建索引。
use super::manager::SessionData;
use super::{LedgerRecord, Result, SessionEntry, SessionError};
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

    pub(super) fn from_request(request: &ModelTurnRequest) -> Self {
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

impl RequestContext {
    pub(super) fn new(definitions: String, model_preferences: RequestPreferences) -> Self {
        Self {
            definitions,
            model_preferences,
        }
    }
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

    /// 定义索引里存着全部旧定义：相同的定义再次出现时复用已有记录，而不是只看最近一份。
    pub(super) fn find_definitions(&self, definitions: &RequestDefinitions) -> Option<String> {
        self.definitions
            .iter()
            .find(|(_, position)| {
                matches!(
                    &self.entries[**position],
                    SessionEntry::Record {
                        record: LedgerRecord::RequestDefinitions { definitions: previous },
                        ..
                    } if previous == definitions
                )
            })
            .map(|(id, _)| id.clone())
    }

    pub(super) fn validate_request_context(&self, context: &RequestContext) -> Result<()> {
        if self.definitions.contains_key(&context.definitions) {
            Ok(())
        } else {
            Err(SessionError::InvalidStructure(format!(
                "request references missing definitions {}",
                context.definitions
            )))
        }
    }

    /// 展开请求记录引用的提示词与工具；不涉及对话内容。
    pub fn request_head(&self, context: &RequestContext) -> Result<Box<ModelRequestSnapshot>> {
        self.validate_request_context(context)?;
        let SessionEntry::Record {
            record: LedgerRecord::RequestDefinitions { definitions },
            ..
        } = &self.entries[self.definitions[&context.definitions]]
        else {
            unreachable!()
        };
        Ok(definitions.snapshot(&context.definitions, &context.model_preferences))
    }
}
