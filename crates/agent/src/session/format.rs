//! Session JSONL schema, strict validation, and public format types.

use std::collections::HashSet;

use serde::ser::Error as _;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use singularity_model::ModelUsage;
use thiserror::Error;
use uuid::Uuid;

use crate::message::AgentMessage;
/// 唯一支持的当前会话格式版本。它是一次干净格式重置，不兼容历史 v1-v4 语义。
pub const CURRENT_SESSION_VERSION: u32 = 1;
/// 会话读写错误。
#[derive(Debug, Error)]
pub enum SessionError {
    #[error("session io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("session json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("session header is invalid: {0}")]
    InvalidHeader(String),
    #[error("session line {line} is malformed: {cause}")]
    MalformedLine { line: usize, cause: String },
    #[error("session entry at line {line} is invalid: {cause}")]
    InvalidEntry { line: usize, cause: String },
    #[error("session entry id is duplicated: {0}")]
    DuplicateId(String),
    #[error("session entry structure is invalid: {0}")]
    InvalidStructure(String),
    #[error("session repair failed: {0}")]
    Repair(String),
    #[error("session append exceeds {kind} limit {limit}; attempted value is {actual}")]
    AppendLimitExceeded {
        kind: &'static str,
        limit: u64,
        actual: u64,
    },
    #[error("{0}")]
    InvalidSession(String),
    #[error("entry {0} not found")]
    EntryNotFound(String),
}

/// 会话操作结果。
pub type Result<T> = std::result::Result<T, SessionError>;
/// compaction 条目 payload。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompactionEntry {
    pub summary: String,
    #[serde(
        rename = "firstKeptEntryId",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub first_kept_entry_id: Option<String>,
    #[serde(
        rename = "tokensBefore",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub tokens_before: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<ModelUsage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}
/// JSONL 中不参与模型上下文的持久化 metadata 类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionMetadataKind {
    TurnStarted,
    TurnCompleted,
    TurnFailed,
    TurnInterrupted,
    ThreadSettings,
    ThreadName,
    Usage,
}

impl SessionMetadataKind {
    pub fn matches_turn_terminal(self) -> bool {
        matches!(
            self,
            Self::TurnCompleted | Self::TurnFailed | Self::TurnInterrupted
        )
    }
}

/// 一条可恢复的 session metadata；variant 直接携带其合法 payload。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "metadataType", rename_all = "snake_case", deny_unknown_fields)]
pub enum SessionMetadata {
    TurnStarted {
        #[serde(rename = "turnId")]
        turn_id: String,
    },
    TurnCompleted {
        #[serde(rename = "turnId")]
        turn_id: String,
    },
    TurnFailed {
        #[serde(rename = "turnId")]
        turn_id: String,
        error: String,
    },
    TurnInterrupted {
        #[serde(rename = "turnId")]
        turn_id: String,
        reason: String,
        synthetic: bool,
    },
    ThreadSettings {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider: Option<String>,
        model: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reasoning: Option<String>,
    },
    ThreadName {
        name: String,
    },
    Usage {
        #[serde(rename = "turnId")]
        turn_id: String,
        usage: Value,
    },
}

impl SessionMetadata {
    pub fn kind(&self) -> SessionMetadataKind {
        match self {
            Self::TurnStarted { .. } => SessionMetadataKind::TurnStarted,
            Self::TurnCompleted { .. } => SessionMetadataKind::TurnCompleted,
            Self::TurnFailed { .. } => SessionMetadataKind::TurnFailed,
            Self::TurnInterrupted { .. } => SessionMetadataKind::TurnInterrupted,
            Self::ThreadSettings { .. } => SessionMetadataKind::ThreadSettings,
            Self::ThreadName { .. } => SessionMetadataKind::ThreadName,
            Self::Usage { .. } => SessionMetadataKind::Usage,
        }
    }

    pub fn turn_id(&self) -> Option<&str> {
        match self {
            Self::TurnStarted { turn_id }
            | Self::TurnCompleted { turn_id }
            | Self::TurnFailed { turn_id, .. }
            | Self::TurnInterrupted { turn_id, .. }
            | Self::Usage { turn_id, .. } => Some(turn_id),
            Self::ThreadSettings { .. } | Self::ThreadName { .. } => None,
        }
    }

    pub fn synthetic(&self) -> bool {
        matches!(
            self,
            Self::TurnInterrupted {
                synthetic: true,
                ..
            }
        )
    }

    pub fn turn_started(turn_id: impl Into<String>) -> Self {
        Self::TurnStarted {
            turn_id: turn_id.into(),
        }
    }

    pub fn turn_completed(turn_id: impl Into<String>) -> Self {
        Self::TurnCompleted {
            turn_id: turn_id.into(),
        }
    }

    pub fn turn_failed(turn_id: impl Into<String>, error: impl Into<String>) -> Self {
        Self::TurnFailed {
            turn_id: turn_id.into(),
            error: error.into(),
        }
    }

    pub fn turn_interrupted(
        turn_id: impl Into<String>,
        reason: impl Into<String>,
        synthetic: bool,
    ) -> Self {
        Self::TurnInterrupted {
            turn_id: turn_id.into(),
            reason: reason.into(),
            synthetic,
        }
    }

    pub fn thread_settings(
        provider: impl Into<String>,
        model: impl Into<String>,
        reasoning: Option<String>,
    ) -> Result<Self> {
        Ok(Self::ThreadSettings {
            provider: Some(provider.into()),
            model: model.into(),
            reasoning,
        })
    }

    pub fn thread_name(name: impl Into<String>) -> Result<Self> {
        let name = name.into();
        if name.trim().is_empty() {
            return Err(SessionError::InvalidStructure(
                "thread name must not be empty".to_string(),
            ));
        }
        Ok(Self::ThreadName { name })
    }

    pub fn usage(turn_id: impl Into<String>, usage: Value) -> Result<Self> {
        if !usage.is_object() {
            return Err(SessionError::InvalidStructure(
                "usage metadata must be a JSON object".to_string(),
            ));
        }
        Ok(Self::Usage {
            turn_id: turn_id.into(),
            usage,
        })
    }

    pub(super) fn validate(self) -> Result<Self> {
        match &self {
            Self::ThreadName { name } if name.trim().is_empty() => Err(
                SessionError::InvalidStructure("thread name must not be empty".to_string()),
            ),
            Self::Usage { usage, .. } if !usage.is_object() => Err(SessionError::InvalidStructure(
                "usage metadata must be a JSON object".to_string(),
            )),
            _ => Ok(self),
        }
    }
}

fn metadata_payload(value: &Value) -> Value {
    let mut payload = value.as_object().cloned().unwrap_or_default();
    for key in ["id", "timestamp", "type"] {
        payload.remove(key);
    }
    Value::Object(payload)
}
/// 会话条目类型。仅保留 Message, Compaction, Metadata。
#[derive(Debug, Clone, PartialEq)]
pub enum SessionEntryType {
    Message(AgentMessage),
    Compaction(CompactionEntry),
    Metadata(SessionMetadata),
}

impl SessionEntryType {
    fn type_name(&self) -> &'static str {
        match self {
            Self::Message(_) => "message",
            Self::Compaction(_) => "compaction",
            Self::Metadata(_) => "metadata",
        }
    }
}

/// 一条会话顺序条目。会话是严格的线性序列：文件行的物理顺序即模型上下文
/// 顺序，条目按其落盘次序推进；不再存储 parentId。
#[derive(Debug, Clone, PartialEq)]
pub struct SessionEntry {
    pub id: String,
    pub timestamp: Option<String>,
    pub entry_type: SessionEntryType,
}

impl Serialize for SessionEntry {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut map = Map::new();
        map.insert("id".to_string(), json!(self.id));
        if let Some(timestamp) = &self.timestamp {
            map.insert("timestamp".to_string(), json!(timestamp));
        }
        map.insert("type".to_string(), json!(self.entry_type.type_name()));
        match &self.entry_type {
            SessionEntryType::Message(message) => {
                map.insert(
                    "message".to_string(),
                    serde_json::to_value(message).map_err(S::Error::custom)?,
                );
            }
            SessionEntryType::Compaction(compaction) => {
                merge_payload(
                    &mut map,
                    serde_json::to_value(compaction).map_err(S::Error::custom)?,
                );
            }
            SessionEntryType::Metadata(metadata) => {
                merge_payload(
                    &mut map,
                    serde_json::to_value(metadata).map_err(S::Error::custom)?,
                );
            }
        }
        Value::Object(map).serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for SessionEntry {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        strict_entry_from_value(&value).map_err(serde::de::Error::custom)
    }
}

fn merge_payload(map: &mut Map<String, Value>, payload: Value) {
    if let Some(object) = payload.as_object() {
        for (key, value) in object {
            map.insert(key.clone(), value.clone());
        }
    }
}
pub(super) fn validate_header(value: &Value) -> Result<(String, u32, Option<String>, String)> {
    let object = value
        .as_object()
        .ok_or_else(|| SessionError::InvalidHeader("header is not a JSON object".into()))?;
    if object.get("type").and_then(Value::as_str) != Some("session") {
        return Err(SessionError::InvalidHeader(
            "first entry is not a session header".into(),
        ));
    }
    for key in object.keys() {
        if !matches!(
            key.as_str(),
            "type" | "version" | "id" | "timestamp" | "cwd"
        ) {
            return Err(SessionError::InvalidHeader(format!(
                "unknown header field: {key}"
            )));
        }
    }
    let session_id = object
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.trim().is_empty())
        .ok_or_else(|| SessionError::InvalidHeader("header id must be a non-empty string".into()))?
        .to_string();
    Uuid::parse_str(&session_id).map_err(|_| {
        SessionError::InvalidHeader(format!("header id must be a valid UUID: {session_id}"))
    })?;
    let version = object
        .get("version")
        .and_then(Value::as_u64)
        .and_then(|version| u32::try_from(version).ok())
        .filter(|version| *version == CURRENT_SESSION_VERSION)
        .ok_or_else(|| {
            SessionError::InvalidHeader(format!(
                "header version must be exactly {CURRENT_SESSION_VERSION}"
            ))
        })?;
    let cwd = match object.get("cwd") {
        Some(Value::String(cwd)) => Some(cwd.clone()),
        Some(_) => {
            return Err(SessionError::InvalidHeader(
                "header cwd must be a string".into(),
            ));
        }
        None => return Err(SessionError::InvalidHeader("header cwd is required".into())),
    };
    let timestamp = object
        .get("timestamp")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| SessionError::InvalidHeader("header timestamp is required".into()))?
        .to_string();
    Ok((session_id, version, cwd, timestamp))
}

pub(super) fn validate_entries(
    raw_entries: &[Value],
    lines: &[usize],
) -> Result<Vec<SessionEntry>> {
    // 会话是严格的线性序列：单趟相邻检查 = 逐条严格解析并保证 id 唯一、
    // 无中间 header。文件行的物理顺序就是事实源顺序。
    let mut entries = Vec::with_capacity(raw_entries.len().saturating_sub(1));
    let mut ids = HashSet::new();
    for (index, raw) in raw_entries.iter().enumerate().skip(1) {
        let line = lines.get(index).copied().unwrap_or(index + 1);
        if raw.get("type").and_then(Value::as_str) == Some("session") {
            return Err(SessionError::InvalidStructure(format!(
                "intermediate session header at line {line}"
            )));
        }
        let entry = strict_entry_from_value(raw)
            .map_err(|cause| SessionError::InvalidEntry { line, cause })?;
        if !ids.insert(entry.id.clone()) {
            return Err(SessionError::DuplicateId(entry.id));
        }
        entries.push(entry);
    }
    Ok(entries)
}

pub(super) fn strict_entry_from_value(value: &Value) -> std::result::Result<SessionEntry, String> {
    let object = value
        .as_object()
        .ok_or_else(|| "session entry is not a JSON object".to_string())?;
    for key in object.keys() {
        if !matches!(
            key.as_str(),
            "id" | "timestamp"
                | "type"
                | "message"
                | "summary"
                | "firstKeptEntryId"
                | "tokensBefore"
                | "previousSummary"
                | "details"
                | "metadataType"
                | "turnId"
                | "error"
                | "reason"
                | "synthetic"
                | "provider"
                | "model"
                | "reasoning"
                | "name"
                | "usage"
        ) {
            return Err(format!("unknown session entry field: {key}"));
        }
    }
    let id = object
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.trim().is_empty())
        .ok_or_else(|| "session entry id must be a non-empty string".to_string())?
        .to_string();
    let timestamp = object
        .get("timestamp")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string)
        .ok_or_else(|| "session entry timestamp is required".to_string())?;
    let entry_type = match object.get("type").and_then(Value::as_str) {
        Some("message") => {
            let message = value
                .get("message")
                .ok_or_else(|| "message entry has no message payload".to_string())?;
            SessionEntryType::Message(
                serde_json::from_value(message.clone())
                    .map_err(|error| format!("invalid message payload: {error}"))?,
            )
        }
        Some("compaction") => SessionEntryType::Compaction(
            serde_json::from_value(value.clone())
                .map_err(|error| format!("invalid compaction payload: {error}"))?,
        ),
        Some("metadata") => SessionEntryType::Metadata(
            serde_json::from_value(metadata_payload(value))
                .map_err(|error| format!("invalid metadata payload: {error}"))?,
        ),
        Some(other) => return Err(format!("unknown session entry type: {other}")),
        None => return Err("session entry has no type".into()),
    };
    Ok(SessionEntry {
        id,
        timestamp: Some(timestamp),
        entry_type,
    })
}
