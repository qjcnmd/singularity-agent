//! 轨迹的提示词与工具定义快照；对话内容不建立索引。
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
        model_preferences: &RequestPreferences,
    ) -> Box<ModelRequestSnapshot> {
        Box::new(ModelRequestSnapshot {
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

/// 一次请求引用的定义与本次请求偏好。请求身份只由外层
/// `RequestObservation` 承载：这里不再重复保存同一个 request id，读取 header
/// 的调用者因此只需维持一处一致。
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

    /// 定义索引已保存全部旧定义：相同定义再次出现时复用已有记录，
    /// 不只看最近一份。
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

    /// 展开请求记录引用的提示词与工具，不涉及对话内容。
    pub fn request_head(&self, context: &RequestContext) -> Result<Box<ModelRequestSnapshot>> {
        self.validate_request_context(context)?;
        let SessionEntry::Record {
            record: LedgerRecord::RequestDefinitions { definitions },
            ..
        } = &self.entries[self.definitions[&context.definitions]]
        else {
            unreachable!()
        };
        Ok(definitions.snapshot(&context.model_preferences))
    }
}
