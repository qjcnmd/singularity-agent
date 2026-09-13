//! Prompt and tool-definition snapshots for the trajectory; conversation content is not indexed.
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
        request_id: &str,
        model_preferences: &RequestPreferences,
    ) -> Box<ModelRequestSnapshot> {
        Box::new(ModelRequestSnapshot {
            request_id: request_id.to_string(),
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestContext {
    pub request_id: String,
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
            self.latest_definitions = Some(position);
        }
    }

    pub(super) fn find_definitions(&self, definitions: &RequestDefinitions) -> Option<String> {
        let entry = &self.entries[self.latest_definitions?];
        matches!(entry, SessionEntry::Record { record: LedgerRecord::RequestDefinitions { definitions: previous }, .. } if previous == definitions).then(|| entry.id().to_string())
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

    /// Expand the prompt and tools referenced by a request record, without conversation content.
    pub fn request_head(&self, context: &RequestContext) -> Result<Box<ModelRequestSnapshot>> {
        self.validate_request_context(context)?;
        let SessionEntry::Record {
            record: LedgerRecord::RequestDefinitions { definitions },
            ..
        } = &self.entries[self.definitions[&context.definitions]]
        else {
            unreachable!()
        };
        Ok(definitions.snapshot(&context.request_id, &context.model_preferences))
    }
}
