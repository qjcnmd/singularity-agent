//! 会话日志的格式定义和文件头检查。
//!
//! 回合结束记录（`operation_finished`）保存最终状态、错误和是否由用户停止。产出可见内容的普通回复
//! 随消息保存请求耗时与用量，其余请求（重试、无输出的失败请求和整理请求）单独记为 `model_request`；指令和工具定义不写日志，每轮重新读取。

use protocol::TurnStatus;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use model::Message;
/// 开发阶段固定为 1，格式随当前实现更新；发布前再确定版本策略。
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
/// 任务设置：模型选择、创建时关闭的上下文模块或任务名称。
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "metadataType", rename_all = "snake_case", deny_unknown_fields)]
pub enum SessionMetadata {
    ThreadSettings {
        selection: protocol::ModelSelection,
    },
    /// 创建任务时关闭的上下文模块 ID，只写这一次；全开时不写。
    DisabledModules {
        ids: Vec<String>,
    },
    ThreadName {
        name: String,
    },
}

/// 仅用于本地界面展示的消息元数据，与发给模型的 `Message` 协议结构解耦。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum MessageDisplay {
    #[serde(rename_all = "camelCase")]
    User {
        /// 附加技能正文和附件路径前的原始用户输入；界面和标题显示原文。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        display_text: Option<String>,
        /// 用户随消息附上的文件路径，界面展示为附件卡片。
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        files: Vec<String>,
        /// 用户随消息附上的文本批注，界面展示为引用卡片。
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        annotations: Vec<protocol::MessageAnnotation>,
    },
    #[serde(rename_all = "camelCase")]
    Tool {
        /// 本地界面正文（如含 ANSI 颜色或 unified diff）；为空时显示模型正文。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output: Option<String>,
        /// bash 结果关联的后台命令 ID，供界面跟随最新状态。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        background_task_id: Option<String>,
        /// 工具实际执行耗时（毫秒）。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
        /// `read` 实际读取到的源文件行范围。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        read_source: Option<protocol::ReadSource>,
    },
}

/// 执行记录：回合开始与结束、未生成消息的模型请求和后台命令状态。不进入模型上下文。
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "recordType", rename_all = "snake_case", deny_unknown_fields)]
pub enum LedgerRecord {
    /// 后台命令的开始和终态；进度只在内存中发布。
    BackgroundTaskUpdated { task: protocol::BackgroundTask },
    /// 一次没有产生可见回复消息的模型请求（重试、无输出的失败/取消或整理请求）。
    ModelRequest {
        observation: protocol::RequestObservation,
    },
    /// 回合开始，记录 ID 就是回合 ID。
    OperationStarted,
    /// 回合结束：完成、失败或中断，失败时带错误详情。
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

/// 日志里文件头之后的一行。
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
    /// 取出这一行的模型请求记录（来自带请求记录的助手消息或单独的 `ModelRequest` 记录）。
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

// 界面条目 ID 由消息 ID 和块序号拼成，流式显示和重新读取历史时得到同一个 ID。

/// 消息中第 index 段正文的界面条目 ID。
pub fn text_item_id(entry_id: &str, index: usize) -> String {
    format!("{entry_id}:text:{index}")
}

/// 消息中第 index 段思考的界面条目 ID。
pub fn thinking_item_id(entry_id: &str, index: usize) -> String {
    format!("{entry_id}:thinking:{index}")
}

/// 助手消息中第 call_index 个工具调用的界面条目 ID。
pub(crate) fn tool_item_id(assistant_entry_id: &str, call_index: usize) -> String {
    format!("{assistant_entry_id}:tool:{call_index}")
}

/// 日志的第一行：类型、格式版本、会话 ID、创建时间和工作目录。
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
