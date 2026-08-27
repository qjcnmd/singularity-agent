//! 会话 JSONL schema、严格校验与公开格式类型。

use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use singularity_model::ModelUsage;
use thiserror::Error;
use uuid::Uuid;

use crate::message::AgentMessage;
/// 唯一支持的当前会话格式版本。v2：条目 payload 嵌套为子对象、全量未知字段
/// 拒绝、终态合并为单条 `turn_terminal`（status + usage + usageComplete）。
pub const CURRENT_SESSION_VERSION: u32 = 2;
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
    #[error("session repair failed: {context}")]
    Repair {
        context: String,
        #[source]
        source: std::io::Error,
    },
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
    #[error("session is being written by an active writer: {thread_id}")]
    WriterConflict { thread_id: String },
    #[error("session writer lock error: {context}")]
    WriterLock {
        context: String,
        #[source]
        source: std::io::Error,
    },
}

/// 会话操作结果。
pub type Result<T> = std::result::Result<T, SessionError>;
/// compaction 条目 payload。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
    TurnTerminal,
    ThreadSettings,
    ThreadName,
}

impl SessionMetadataKind {
    pub fn matches_turn_terminal(self) -> bool {
        matches!(self, Self::TurnTerminal)
    }
}

/// `turn_terminal` 的终态词形。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnTerminalStatus {
    Completed,
    Failed,
    Interrupted,
}

/// 一条可恢复的 session metadata；variant 直接携带其合法 payload。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "metadataType", rename_all = "snake_case", deny_unknown_fields)]
pub enum SessionMetadata {
    TurnStarted {
        #[serde(rename = "turnId")]
        turn_id: String,
    },
    /// turn 的原子终态：status、usage 与 usageComplete 单条落盘。
    TurnTerminal {
        #[serde(rename = "turnId")]
        turn_id: String,
        status: TurnTerminalStatus,
        usage: Value,
        #[serde(rename = "usageComplete")]
        usage_complete: bool,
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
}

impl SessionMetadata {
    pub fn kind(&self) -> SessionMetadataKind {
        match self {
            Self::TurnStarted { .. } => SessionMetadataKind::TurnStarted,
            Self::TurnTerminal { .. } => SessionMetadataKind::TurnTerminal,
            Self::ThreadSettings { .. } => SessionMetadataKind::ThreadSettings,
            Self::ThreadName { .. } => SessionMetadataKind::ThreadName,
        }
    }

    pub fn turn_id(&self) -> Option<&str> {
        match self {
            Self::TurnStarted { turn_id } | Self::TurnTerminal { turn_id, .. } => Some(turn_id),
            Self::ThreadSettings { .. } | Self::ThreadName { .. } => None,
        }
    }

    /// 终态词形；非终态返回 `None`。
    pub fn terminal_status(&self) -> Option<TurnTerminalStatus> {
        match self {
            Self::TurnTerminal { status, .. } => Some(*status),
            _ => None,
        }
    }

    pub fn turn_started(turn_id: impl Into<String>) -> Self {
        Self::TurnStarted {
            turn_id: turn_id.into(),
        }
    }

    /// 只组装载荷，不做校验；不变量统一由 [`Self::validate`] 在写入路径
    /// （append_metadata）收敛检查。
    pub fn turn_terminal(
        turn_id: impl Into<String>,
        status: TurnTerminalStatus,
        usage: Value,
        usage_complete: bool,
    ) -> Self {
        Self::TurnTerminal {
            turn_id: turn_id.into(),
            status,
            usage,
            usage_complete,
        }
    }

    pub fn thread_settings(
        provider: impl Into<String>,
        model: impl Into<String>,
        reasoning: Option<String>,
    ) -> Self {
        Self::ThreadSettings {
            provider: Some(provider.into()),
            model: model.into(),
            reasoning,
        }
    }

    pub fn thread_name(name: impl Into<String>) -> Self {
        Self::ThreadName { name: name.into() }
    }

    pub(super) fn validate(self) -> Result<Self> {
        match &self {
            Self::ThreadName { name } if name.trim().is_empty() => Err(
                SessionError::InvalidStructure("thread name must not be empty".to_string()),
            ),
            Self::TurnTerminal { usage, .. } if !usage.is_object() => Err(
                SessionError::InvalidStructure("terminal usage must be a JSON object".to_string()),
            ),
            _ => Ok(self),
        }
    }
}

/// 会话条目：以 `type` 为标签的 tagged enum，serde 生成序列化与严格类型校验。
///
/// v2：payload 一律嵌套为子对象（`message`/`compaction`/`metadata`），外层
/// 与各载荷均 `deny_unknown_fields`——未知字段写入即拒绝。会话是严格的线性
/// 序列：文件行的物理顺序即模型上下文顺序，条目按其落盘次序推进；不再存储
/// parentId。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum SessionEntry {
    Message {
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timestamp: Option<String>,
        message: AgentMessage,
    },
    Compaction {
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timestamp: Option<String>,
        compaction: CompactionEntry,
    },
    Metadata {
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timestamp: Option<String>,
        metadata: SessionMetadata,
    },
}

impl SessionEntry {
    pub fn id(&self) -> &str {
        match self {
            Self::Message { id, .. } | Self::Compaction { id, .. } | Self::Metadata { id, .. } => {
                id
            }
        }
    }
}
pub(super) fn validate_header(value: &Value) -> Result<(String, u32, String, String)> {
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
        Some(Value::String(cwd)) => cwd.clone(),
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
    // 会话是严格的线性序列：单趟相邻检查 = 逐条 serde 严格解析并保证 id 唯一、
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
        let entry = serde_json::from_value::<SessionEntry>(raw.clone()).map_err(|error| {
            SessionError::InvalidEntry {
                line,
                cause: error.to_string(),
            }
        })?;
        if !ids.insert(entry.id().to_string()) {
            return Err(SessionError::DuplicateId(entry.id().to_string()));
        }
        entries.push(entry);
    }
    Ok(entries)
}
