//! 会话 JSONL schema、严格校验与公开格式类型。
//!
//! v7：线性消息与压缩序列，以及操作、文件指令和工具剪枝记录。
//! 文件指令直接进入模型上下文；工具剪枝记录替换模型视图中的对应输出。
//! 操作与请求观测用于恢复及查看；系统和工具定义通过索引去重。
//! turn 的终态唯一落盘位置是
//! operation_finished（run 记录携带 turnId）。

use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use singularity_model::ModelUsage;
use singularity_protocol::{TurnModelUsage, TurnStatus, wire_word};
use thiserror::Error;
use uuid::Uuid;

use crate::message::AgentMessage;
/// 当前会话格式版本；不迁移旧格式，未知字段仍拒绝。
pub const CURRENT_SESSION_VERSION: u32 = 7;
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
    #[error("session ledger is corrupt: {reason}: {detail}")]
    LedgerCorrupt { reason: String, detail: String },
    #[error("session append exceeds {kind} limit {limit}; attempted value is {actual}")]
    AppendLimitExceeded {
        kind: &'static str,
        limit: u64,
        actual: u64,
    },
    #[error("{0}")]
    InvalidSession(String),
    #[error("session is being written by an active writer: {thread_id}")]
    WriterConflict { thread_id: String },
}

/// 会话操作结果。
pub type Result<T> = std::result::Result<T, SessionError>;
/// compaction 条目 payload。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompactionEntry {
    pub summary: String,
    #[serde(rename = "firstKeptEntryId")]
    pub first_kept_entry_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<TurnModelUsage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}
/// 领域 usage → 会话统一落盘形状 TurnModelUsage；complete 由调用方的
/// 聚合语义给出（终态：每个 provider 请求是否都报告了精确 usage）。
pub fn turn_usage_from_model_usage(usage: &ModelUsage, complete: bool) -> TurnModelUsage {
    TurnModelUsage {
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        total_tokens: usage.total_tokens,
        cached_input_tokens: usage.cached_input_tokens,
        reasoning_tokens: usage.reasoning_tokens,
        usage_present: usage.usage_present,
        usage_complete: complete,
    }
}

/// 一条可恢复的 session metadata；variant 直接携带其合法 payload。
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

impl SessionMetadata {
    /// 只组装载荷，不做校验；不变量统一由 Self::validate 在写入路径
    /// （append_metadata）收敛检查。
    pub fn thread_settings(
        provider: impl Into<String>,
        model: impl Into<String>,
        reasoning: Option<String>,
    ) -> Self {
        Self::ThreadSettings {
            provider: provider.into(),
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
            _ => Ok(self),
        }
    }
}

/// operation 种类：一次 run（绑定 turn）或一次独立 compaction。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationKind {
    Run,
    Compaction,
}

pub use singularity_protocol::{ControlChannel, ControlDisposition};

/// 控制请求的运行时载体：接受时组装的 identity、payload 与接受顺序。
/// 接受、编辑与终态记录共用 control_id，归约后保留最新内容与处置状态。
/// control_id 使用 {turn_id}:{channel_word}:{sequence} 格式。
#[derive(Debug, Clone, PartialEq)]
pub struct ControlRequest {
    pub control_id: String,
    pub turn_id: String,
    pub channel: ControlChannel,
    pub sequence: u64,
    pub text: Option<String>,
}

/// 控制记录 identity 的单点构造形式：{turn_id}:{channel_word}:{sequence}。
/// channel_word 是 ControlChannel 的 serde snake_case 词形；
/// 所有控制记录的 control_id 字段均由此产生，归约据此推断所属 turn。
pub fn control_id(turn_id: &str, channel: ControlChannel, sequence: u64) -> String {
    let channel_word = wire_word(channel);
    format!("{turn_id}:{channel_word}:{sequence}")
}

impl ControlRequest {
    /// 当前控制事实的公开投影；接受、执行与恢复共用同一字段映射。
    pub fn snapshot(
        &self,
        disposition: ControlDisposition,
    ) -> singularity_protocol::ControlSnapshot {
        singularity_protocol::ControlSnapshot {
            control_id: self.control_id.clone(),
            turn_id: self.turn_id.clone(),
            channel: self.channel,
            sequence: self.sequence,
            text: self.text.clone(),
            disposition,
        }
    }
}

/// 单 lane operation ledger 记录：执行恢复的唯一持久事实。记录只在
/// durable acceptance 后对消费者可见；物理行序即记录顺序（单调引用）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "recordType", rename_all = "snake_case", deny_unknown_fields)]
pub enum LedgerRecord {
    /// 来自用户全局与项目文件的完整指令上下文，可被摘要但必须由来源重新注入。
    Instructions { text: String },
    /// Full instructions explicitly selected by the user; durable alongside that input.
    SkillInstructions { text: String },
    /// 模型上下文中的工具结果替换；原始 Message 保留供历史和轨迹查看。
    ToolResultPruned {
        #[serde(rename = "entryId")]
        entry_id: String,
        content: Vec<crate::message::ContentBlock>,
    },
    /// A completed model request observed by the trajectory. It does not drive recovery.
    ModelRequest {
        observation: singularity_protocol::RequestObservation,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        context: Option<Box<super::request::RequestContext>>,
    },
    /// 不可变的规范请求消息或工具定义；后续观测只引用其条目 ID。
    RequestDefinitions {
        definitions: super::request::RequestDefinitions,
    },
    /// 已接受 operation 的起步事实；先于任何实时执行事件落盘。
    OperationStarted {
        #[serde(rename = "operationId")]
        operation_id: String,
        kind: OperationKind,
        /// run operation 绑定的 turn id；独立 compaction 为 None。
        #[serde(rename = "turnId", default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<String>,
    },
    /// operation 终态：run 记录同时是该 turn 的唯一终态事实（status/usage/
    /// truncated 保存在同一条记录中）。outcome 恒为终态（非 running）。
    OperationFinished {
        #[serde(rename = "operationId")]
        operation_id: String,
        #[serde(rename = "turnId", default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<String>,
        outcome: TurnStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<TurnModelUsage>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        truncated: bool,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        user_stopped: bool,
    },
}

/// 会话条目：以 type 为标签的 tagged enum，serde 生成序列化与严格类型校验。
///
/// payload 一律嵌套为子对象（message/compaction/metadata/record），外层
/// 与各载荷均 deny_unknown_fields——未知字段写入即拒绝。会话是严格的线性
/// 序列：文件行的物理顺序即模型上下文顺序与记录单调序。
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
    pub fn id(&self) -> &str {
        match self {
            Self::Message { id, .. }
            | Self::Compaction { id, .. }
            | Self::Metadata { id, .. }
            | Self::Record { id, .. } => id,
        }
    }
}

/// Stable public identity for a tool occurrence, independent of provider call-ID reuse.
pub fn tool_item_id(assistant_entry_id: &str, call_index: usize) -> String {
    format!("{assistant_entry_id}:tool:{call_index}")
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
                "unsupported session version; expected {CURRENT_SESSION_VERSION}"
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
    // 解析即归一：header 的 cwd 一旦离开这里就只有唯一形状，列表、Thread 投影
    // 与系统提示词不再各自派生写法。
    let cwd = singularity_core::CanonicalWorkspacePath::from_saved(&cwd)
        .map_err(SessionError::InvalidHeader)?
        .display()
        .to_string();
    let timestamp = object
        .get("timestamp")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| SessionError::InvalidHeader("header timestamp is required".into()))?
        .to_string();
    Ok((session_id, version, cwd, timestamp))
}

pub(super) fn validate_entries(
    raw_entries: impl ExactSizeIterator<Item = Value>,
    lines: &[usize],
) -> Result<Vec<SessionEntry>> {
    // 会话是严格的线性序列：单趟相邻检查 = 逐条 serde 严格解析并保证 id 唯一、
    // 无中间 header。文件行的物理顺序就是事实源顺序。
    let mut entries = Vec::with_capacity(raw_entries.len());
    let mut ids = HashSet::new();
    for (index, raw) in raw_entries.enumerate() {
        let line = lines.get(index + 1).copied().unwrap_or(index + 2);
        if raw.get("type").and_then(Value::as_str) == Some("session") {
            return Err(SessionError::InvalidStructure(format!(
                "intermediate session header at line {line}"
            )));
        }
        let entry = serde_json::from_value::<SessionEntry>(raw).map_err(|error| {
            SessionError::InvalidEntry {
                line,
                cause: error.to_string(),
            }
        })?;
        if matches!(
            &entry,
            SessionEntry::Record {
                record: LedgerRecord::OperationFinished {
                    outcome: TurnStatus::Running,
                    ..
                },
                ..
            }
        ) {
            return Err(SessionError::InvalidEntry {
                line,
                cause: "operation_finished must not persist a running outcome".to_string(),
            });
        }
        if !ids.insert(entry.id().to_string()) {
            return Err(SessionError::DuplicateId(entry.id().to_string()));
        }
        entries.push(entry);
    }
    Ok(entries)
}
