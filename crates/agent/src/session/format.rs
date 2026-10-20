//! 会话 JSONL 的 schema、严格校验与公开格式类型。
//!
//! 当前版本（CURRENT_SESSION_VERSION）的 operation_finished 只持久化终态所需的
//! 结果、错误与停止事实。请求用量由请求
//! 观测记录提供；指令与工具定义在每轮首次请求前保存。

use serde::{Deserialize, Serialize};
use serde_json::Value;
use singularity_protocol::TurnStatus;
use thiserror::Error;
use uuid::Uuid;

use crate::message::AgentMessage;
/// 开发阶段固定为 1，格式随当前实现更新；发布前再确定版本策略。
pub const CURRENT_SESSION_VERSION: u32 = 1;
/// 会话读写过程中可能出现的错误。
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
    #[error("{0}")]
    InvalidSession(String),
}

/// 会话操作的统一返回结果。
pub type Result<T> = std::result::Result<T, SessionError>;
/// 摘要与保留位置一同提交；完整历史仍保留在日志中。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompactionEntry {
    pub summary: String,
    #[serde(rename = "firstKeptEntryId")]
    pub first_kept_entry_id: String,
}
/// 会话元数据条目（如模型配置覆盖、自定义名称）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "metadataType", rename_all = "snake_case", deny_unknown_fields)]
pub enum SessionMetadata {
    ThreadSettings {
        provider: String,
        model: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reasoning: Option<String>,
    },
    ThreadName {
        name: String,
    },
}

/// 会话执行与请求观测的持久化账本记录。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "recordType", rename_all = "snake_case", deny_unknown_fields)]
pub enum LedgerRecord {
    /// 后台命令的开始和终态；进度只在内存中发布。
    BackgroundTaskUpdated {
        task: singularity_protocol::BackgroundTask,
    },
    /// 请求中断前已生成的正文和思考；仅供显示，不进入后续模型上下文。
    AssistantInterrupted { message: AgentMessage },
    /// 单次模型调用的请求与结果观测。
    ModelRequest {
        observation: singularity_protocol::RequestObservation,
    },
    /// 每轮实际注入的内容，直接供会话展示。
    ContextInjected { name: String, content: String },
    /// 回合开始记录。
    OperationStarted {
        /// 回合 ID。
        #[serde(rename = "turnId")]
        turn_id: String,
    },
    /// 回合终态记录，包含成功/失败/中断状态与错误详情。
    OperationFinished {
        #[serde(rename = "turnId")]
        turn_id: String,
        outcome: TurnStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<singularity_protocol::TurnErrorDetail>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        user_stopped: bool,
    },
}

/// 会话 JSONL 单行条目枚举。物理行顺序对应事件写入顺序。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum SessionEntry {
    Message {
        id: String,
        timestamp: String,
        message: AgentMessage,
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
    /// 条目写入时间戳。
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

/// 派生文本消息块的稳定 item ID。
pub fn text_item_id(entry_id: &str, index: usize) -> String {
    format!("{entry_id}:text:{index}")
}

/// 派生思考过程块的稳定 item ID。
pub fn thinking_item_id(entry_id: &str, index: usize) -> String {
    format!("{entry_id}:thinking:{index}")
}

/// 派生工具调用的稳定 item ID（基于助手消息 ID 与调用序号）。
pub fn tool_item_id(assistant_entry_id: &str, call_index: usize) -> String {
    format!("{assistant_entry_id}:tool:{call_index}")
}

/// 会话 JSONL 文件的 Header（第一行）。
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

    /// 从 JSON 解析并校验会话文件头（校验类型、版本号、UUID 与时间戳格式）。
    pub(super) fn parse(value: Value) -> Result<Self> {
        if value.get("type").and_then(Value::as_str) != Some(SESSION_HEADER_TYPE) {
            return Err(SessionError::InvalidHeader("first entry is not a session header".into()));
        }
        let header: Self =
            serde_json::from_value(value).map_err(|error| SessionError::InvalidHeader(error.to_string()))?;
        Uuid::parse_str(&header.id).map_err(|_| {
            SessionError::InvalidHeader(format!("header id must be a valid UUID: {}", header.id))
        })?;
        if header.version != CURRENT_SESSION_VERSION {
            return Err(SessionError::InvalidHeader(format!(
                "unsupported session version; expected {CURRENT_SESSION_VERSION}"
            )));
        }
        if header.timestamp.trim().is_empty() {
            return Err(SessionError::InvalidHeader("header timestamp is required".into()));
        }
        Ok(header)
    }

    pub(super) fn canonical_cwd(&self) -> Result<std::path::PathBuf> {
        singularity_core::CanonicalWorkspacePath::from_saved(&self.cwd)
            .map_err(SessionError::InvalidHeader)
            .map(|cwd| cwd.as_path().to_path_buf())
    }
}
