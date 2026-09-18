//! 会话 JSONL schema、严格校验与公开格式类型。
//!
//! 当前版本（CURRENT_SESSION_VERSION）在 v7 的线性消息与压缩序列、操作/文件指令/
//! 工具剪枝记录之上，把工具结果改为只经调用 ID 关联原始 ToolCall，不再携带冗余名称。
//! 文件指令直接进入模型上下文；工具剪枝记录替换模型视图中的对应输出。
//! 操作与请求观测用于恢复及查看；系统和工具定义通过索引去重。
//! turn 的终态唯一落盘位置是
//! operation_finished（run 记录携带 turnId）。

use serde::{Deserialize, Serialize};
use serde_json::Value;
use singularity_model::ModelUsage;
use singularity_protocol::{TurnModelUsage, TurnStatus};
use thiserror::Error;
use uuid::Uuid;

use crate::message::AgentMessage;
/// 当前会话格式版本；不迁移旧格式，未知字段仍拒绝。
pub const CURRENT_SESSION_VERSION: u32 = 8;
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
    #[error("session directory {actual} does not match the requested directory {expected}")]
    ScopeMismatch { actual: String, expected: String },
    #[error("session is being written by an active writer: {thread_id}")]
    WriterConflict { thread_id: String },
}

/// 会话操作结果。
pub type Result<T> = std::result::Result<T, SessionError>;
/// compaction 条目 payload：摘要正文与保留锚点。
///
/// 摘要请求的计量由请求账本的 request observation 承担，条目只表达被替换历史
/// 的摘要与保留边界。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompactionEntry {
    pub summary: String,
    #[serde(rename = "firstKeptEntryId")]
    pub first_kept_entry_id: String,
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

/// 单 lane operation ledger 记录：执行恢复的唯一持久事实。记录只在
/// durable acceptance 后对消费者可见；物理行序即记录顺序（单调引用）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "recordType", rename_all = "snake_case", deny_unknown_fields)]
pub enum LedgerRecord {
    /// 来自用户全局与项目文件的完整指令上下文，可被摘要但必须由来源重新注入。
    Instructions { text: String },
    /// 用户显式选择的完整指令；与该输入一同持久化。
    SkillInstructions { text: String },
    /// 模型上下文中的工具结果替换；原始 Message 保留供历史和轨迹查看。
    ToolResultPruned {
        #[serde(rename = "entryId")]
        entry_id: String,
        content: Vec<crate::message::ContentBlock>,
    },
    /// 一次模型请求尝试的开始/终态观测：同一 request 先写 Started、后写终态。
    /// 它不驱动 operation 恢复。
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
    /// error 是 run 失败终态的可持久化细节（stage/cause/message）：它是该 turn
    /// 失败原因的长期来源，历史投影直接复用它，不再依赖最近一次 runtime 文本。
    /// 成功、中断、独立 compaction 与崩溃修复关闭的 operation 为 None。
    OperationFinished {
        #[serde(rename = "operationId")]
        operation_id: String,
        #[serde(rename = "turnId", default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<String>,
        outcome: TurnStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<TurnModelUsage>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<singularity_protocol::TurnErrorDetail>,
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

/// 已持久化消息内文本块的稳定公开身份；历史投影与实时事件都由此派生，
/// 消费方只读取结果，不自行拼接后缀。
pub fn text_item_id(entry_id: &str, index: usize) -> String {
    format!("{entry_id}:text:{index}")
}

/// 已持久化消息内思考块的稳定公开身份。
pub fn thinking_item_id(entry_id: &str, index: usize) -> String {
    format!("{entry_id}:thinking:{index}")
}

/// 工具出现位置的稳定公开身份，不受 provider 复用 call-ID 影响。
pub fn tool_item_id(assistant_entry_id: &str, call_index: usize) -> String {
    format!("{assistant_entry_id}:tool:{call_index}")
}

/// session 文件头的磁盘形状：固定字段集、未知字段拒绝与读写的唯一表示。
/// 字段值原样保留磁盘内容——cwd 不在此归一化，运行期路径由
/// [`SessionHeader::canonical_cwd`] 单独给出，修复写回不改写已存路径。
/// 构造走 `new`（写入）与 `parse`（读取），二者是类型不变量的唯一入口。
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

/// 文件头 `type` 字段的唯一取值；写入端恒为此值，读取端只接受此值。
const SESSION_HEADER_TYPE: &str = "session";

impl SessionHeader {
    /// 新建会话文件头：类型与版本由 schema 拥有者固定，调用方只提供身份。
    pub(super) fn new(session_id: String, cwd: String, timestamp: String) -> Self {
        Self {
            kind: SESSION_HEADER_TYPE.to_string(),
            version: CURRENT_SESSION_VERSION,
            id: session_id,
            timestamp,
            cwd,
        }
    }

    /// 从磁盘 JSON 解析文件头。serde 负责固定字段形状与未知字段拒绝，
    /// 此处保留需要显式表达的语义校验：type 取值、非空 UUID、当前版本、
    /// 非空创建时间。
    pub(super) fn parse(value: Value) -> Result<Self> {
        if value.get("type").and_then(Value::as_str) != Some(SESSION_HEADER_TYPE) {
            return Err(SessionError::InvalidHeader(
                "first entry is not a session header".into(),
            ));
        }
        let header: Self = serde_json::from_value(value)
            .map_err(|error| SessionError::InvalidHeader(error.to_string()))?;
        if header.id.trim().is_empty() {
            return Err(SessionError::InvalidHeader(
                "header id must be a non-empty string".into(),
            ));
        }
        Uuid::parse_str(&header.id).map_err(|_| {
            SessionError::InvalidHeader(format!("header id must be a valid UUID: {}", header.id))
        })?;
        if header.version != CURRENT_SESSION_VERSION {
            return Err(SessionError::InvalidHeader(format!(
                "unsupported session version; expected {CURRENT_SESSION_VERSION}"
            )));
        }
        if header.timestamp.trim().is_empty() {
            return Err(SessionError::InvalidHeader(
                "header timestamp is required".into(),
            ));
        }
        Ok(header)
    }

    /// 已存 cwd 的唯一归一化点：磁盘字面值仍保留在 header 中，归一化结果
    /// 只供运行期使用，列表、Thread 投影与系统提示词共用这一形状。
    pub(super) fn canonical_cwd(&self) -> Result<String> {
        singularity_core::CanonicalWorkspacePath::from_saved(&self.cwd)
            .map_err(SessionError::InvalidHeader)
            .map(|cwd| cwd.display().to_string())
    }
}

pub(super) fn parse_entry(raw: Value, line: usize) -> Result<SessionEntry> {
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
    Ok(entry)
}
