//! Prompt and tool-definition snapshots for the trajectory; conversation content is not indexed.
use super::{LedgerRecord, Result, SessionEntry, SessionError};
use serde::{Deserialize, Serialize};
use singularity_model::{ModelRole, ModelTurnRequest};
use singularity_protocol::{ModelRequestSnapshot, RequestMessage, RequestPreferences, RequestTool};
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestDefinitions {
    pub messages: Vec<RequestMessage>,
    pub tools: Vec<RequestTool>,
}

impl RequestDefinitions {
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

#[derive(Default)]
pub(super) struct RequestIndex {
    definitions: HashMap<String, usize>,
    requests: HashMap<String, usize>,
    latest: Option<usize>,
}

impl RequestIndex {
    pub(super) fn from_entries(entries: &[SessionEntry]) -> Self {
        let mut index = Self::default();
        for (position, entry) in entries.iter().enumerate() {
            index.observe(entry, position);
        }
        index
    }
    pub(super) fn observe(&mut self, entry: &SessionEntry, position: usize) {
        if let SessionEntry::Record { id, record, .. } = entry {
            match record {
                LedgerRecord::RequestDefinitions { .. } => {
                    self.definitions.insert(id.clone(), position);
                    self.latest = Some(position);
                }
                LedgerRecord::ModelRequest {
                    observation,
                    context: Some(_),
                    ..
                } => {
                    self.requests
                        .insert(observation.request_id.clone(), position);
                }
                _ => {}
            }
        }
    }
    pub(super) fn find(
        &self,
        entries: &[SessionEntry],
        definitions: &RequestDefinitions,
    ) -> Option<String> {
        let entry = &entries[self.latest?];
        matches!(entry, SessionEntry::Record { record: LedgerRecord::RequestDefinitions { definitions: previous }, .. } if previous == definitions).then(|| entry.id().to_string())
    }
    pub(super) fn validate(&self, context: &RequestContext) -> Result<()> {
        if self.definitions.contains_key(&context.definitions) {
            Ok(())
        } else {
            Err(SessionError::InvalidStructure(format!(
                "request references missing definitions {}",
                context.definitions
            )))
        }
    }
    pub(super) fn head(
        &self,
        entries: &[SessionEntry],
        id: &str,
    ) -> Result<Box<ModelRequestSnapshot>> {
        let context = self
            .requests
            .get(id)
            .and_then(|p| match &entries[*p] {
                SessionEntry::Record {
                    record: LedgerRecord::ModelRequest { context, .. },
                    ..
                } => context.as_deref(),
                _ => None,
            })
            .ok_or_else(|| {
                SessionError::InvalidStructure(format!("request header not found: {id}"))
            })?;
        self.validate(context)?;
        let SessionEntry::Record {
            record: LedgerRecord::RequestDefinitions { definitions },
            ..
        } = &entries[self.definitions[&context.definitions]]
        else {
            unreachable!()
        };
        Ok(Box::new(ModelRequestSnapshot {
            request_id: context.request_id.clone(),
            messages: definitions.messages.clone(),
            tools: definitions.tools.clone(),
            model_preferences: context.model_preferences.clone(),
        }))
    }
}
