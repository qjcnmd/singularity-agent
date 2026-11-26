use protocol::TurnStatus;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use model::Message;
const CURRENT_SESSION_VERSION: u32 = 1;
#[derive(Debug, Error)]
pub enum SessionError {
    #[error("session io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("session json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("session header is invalid: {0}")]
    InvalidHeader(String),
    #[error("session line {line} is invalid: {source}")]
    InvalidEntry {
        line: usize,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    #[error("{message}：{source}")]
    Context {
        message: String,
        #[source]
        source: Box<SessionError>,
    },
    #[error("{0}")]
    InvalidSession(String),
}

pub(super) type Result<T> = std::result::Result<T, SessionError>;
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompactionEntry {
    pub summary: String,
    #[serde(rename = "firstKeptEntryId", default, skip_serializing_if = "Option::is_none")]
    pub first_kept_entry_id: Option<String>,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "metadataType", rename_all = "snake_case", deny_unknown_fields)]
pub enum SessionMetadata {
    ThreadSettings { selection: protocol::ModelSelection },
    DisabledModules { ids: Vec<String> },
    ThreadName { name: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum MessageDisplay {
    #[serde(rename_all = "camelCase")]
    User {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        display_text: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        files: Vec<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        annotations: Vec<protocol::MessageAnnotation>,
    },
    #[serde(rename_all = "camelCase")]
    Tool {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        background_task_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        read_source: Option<protocol::ReadSource>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        file_created: Option<bool>,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "recordType", rename_all = "snake_case", deny_unknown_fields)]
pub enum LedgerRecord {
    BackgroundTaskUpdated {
        task: protocol::BackgroundTask,
    },
    ModelRequest {
        observation: protocol::RequestObservation,
    },
    OperationStarted,
    OperationFinished {
        #[serde(rename = "turnId")]
        turn_id: String,
        outcome: TurnStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<protocol::TurnErrorDetail>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        user_stopped: bool,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum SessionEntry {
    Message {
        id: String,
        timestamp: String,
        message: Message,
        #[serde(skip_serializing_if = "Option::is_none")]
        observation: Option<protocol::RequestObservation>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        display: Option<MessageDisplay>,
    },
    Compaction {
        id: String,
        timestamp: String,
        compaction: CompactionEntry,
    },
    Metadata {
        id: String,
        timestamp: String,
        metadata: SessionMetadata,
    },
    Record {
        id: String,
        timestamp: String,
        record: LedgerRecord,
    },
}

impl SessionEntry {
    pub fn request_observation(&self) -> Option<&protocol::RequestObservation> {
        match self {
            Self::Message { observation, .. } => observation.as_ref(),
            Self::Record {
                record: LedgerRecord::ModelRequest { observation },
                ..
            } => Some(observation),
            _ => None,
        }
    }

    pub fn timestamp(&self) -> &str {
        match self {
            Self::Message { timestamp, .. }
            | Self::Compaction { timestamp, .. }
            | Self::Metadata { timestamp, .. }
            | Self::Record { timestamp, .. } => timestamp,
        }
    }

    pub fn id(&self) -> &str {
        match self {
            Self::Message { id, .. }
            | Self::Compaction { id, .. }
            | Self::Metadata { id, .. }
            | Self::Record { id, .. } => id,
        }
    }
}

pub fn text_item_id(entry_id: &str, index: usize) -> String {
    format!("{entry_id}:text:{index}")
}

pub fn thinking_item_id(entry_id: &str, index: usize) -> String {
    format!("{entry_id}:thinking:{index}")
}

pub(crate) fn tool_item_id(assistant_entry_id: &str, call_index: usize) -> String {
    format!("{assistant_entry_id}:tool:{call_index}")
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SessionHeader {
    #[serde(rename = "type")]
    pub(super) kind: String,
    pub(super) version: u32,
    pub(super) id: String,
    pub(super) timestamp: String,
    pub(super) cwd: String,
}

const SESSION_HEADER_TYPE: &str = "session";

impl SessionHeader {
    pub(super) fn new(session_id: String, cwd: String, timestamp: String) -> Self {
        Self {
            kind: SESSION_HEADER_TYPE.to_string(),
            version: CURRENT_SESSION_VERSION,
            id: session_id,
            timestamp,
            cwd,
        }
    }

    pub(super) fn validate(self) -> Result<Self> {
        if self.kind != SESSION_HEADER_TYPE {
            return Err(SessionError::InvalidHeader("first entry is not a session header".into()));
        }
        Uuid::parse_str(&self.id).map_err(|_| {
            SessionError::InvalidHeader(format!("header id must be a valid UUID: {}", self.id))
        })?;
        if self.version != CURRENT_SESSION_VERSION {
            return Err(SessionError::InvalidHeader(format!(
                "unsupported session version; expected {CURRENT_SESSION_VERSION}"
            )));
        }
        if self.timestamp.trim().is_empty() {
            return Err(SessionError::InvalidHeader("header timestamp is required".into()));
        }
        Ok(self)
    }

    pub(super) fn canonical_cwd(&self) -> Result<std::path::PathBuf> {
        app_core::CanonicalWorkspacePath::from_saved(&self.cwd)
            .map_err(SessionError::InvalidHeader)
            .map(|cwd| cwd.as_path().to_path_buf())
    }
}
