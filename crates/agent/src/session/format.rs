//! 会话 JSONL 的 schema、严格校验与公开格式类型。
//!
//! 当前版本（CURRENT_SESSION_VERSION）在 v7 的线性消息与压缩序列、操作记录、
//! 文件指令与工具剪枝记录之上，把工具结果改为只经调用 ID 关联原始 ToolCall，
//! 不再携带冗余的工具名。文件指令与工具剪枝记录改变模型视图，操作与请求观测只
//! 用于恢复和查看；系统与工具定义按内容去重。turn 的终态只落在 operation_finished。

use serde::{Deserialize, Serialize};
use serde_json::Value;
use singularity_model::ModelUsage;
use singularity_protocol::{TurnModelUsage, TurnStatus};
use thiserror::Error;
use uuid::Uuid;

use crate::message::AgentMessage;
/// 当前会话格式版本。旧格式不做迁移，未知字段依旧拒绝。
pub const CURRENT_SESSION_VERSION: u32 = 8;
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

/// 会话操作的统一返回结果。
pub type Result<T> = std::result::Result<T, SessionError>;
/// compaction 条目携带的内容：摘要正文与保留锚点。摘要请求自身的 token 消耗记在
/// 请求账本的 request observation 上，这条条目只表达被替换历史的摘要和保留边界。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompactionEntry {
    pub summary: String,
    #[serde(rename = "firstKeptEntryId")]
    pub first_kept_entry_id: String,
}
/// 把领域 usage 转成会话统一的落盘形状 TurnModelUsage；complete 由调用方
/// 按聚合语义给出，表示本次终态里每个 provider 请求是否都报告了精确 usage。
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

/// 一条可恢复的 session metadata；每个 variant 直接带着自己合法的 payload。
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

/// operation 的种类：一次 run（绑定 turn）或一次独立的 compaction。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationKind {
    Run,
    Compaction,
}

/// 单 lane operation ledger 的一条记录：执行恢复唯一依赖的持久事实。记录只在
/// durable acceptance 之后才对消费者可见；物理行序就是记录顺序，引用只向后指。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "recordType", rename_all = "snake_case", deny_unknown_fields)]
pub enum LedgerRecord {
    /// 被中断时留下来展示的内容，不进入模型上下文，也不参与摘要。
    AssistantInterrupted {
        items: Vec<singularity_protocol::HistoryItem>,
    },
    /// 已有 v8 会话中的文件指令记录；当前请求从文件重新读取，不使用这条历史快照。
    Instructions { text: String },
    /// 用户显式选择的技能完整指令；和触发它的那次输入一起持久化。
    SkillInstructions { text: String },
    /// 用来替换模型上下文里那份工具结果的正文；原始 Message 仍保留，供历史和轨迹查看。
    ToolResultPruned {
        #[serde(rename = "entryId")]
        entry_id: String,
        content: Vec<crate::message::ContentBlock>,
    },
    /// 一次模型请求尝试的开始与终态观测：同一个 request 先写 Started、再写终态。
    /// 它不参与 operation 恢复。
    ModelRequest {
        observation: singularity_protocol::RequestObservation,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        context: Option<Box<super::request::RequestContext>>,
    },
    /// 不可变的规范请求消息或工具定义；后续观测只引用这条记录的条目 ID。
    RequestDefinitions {
        definitions: super::request::RequestDefinitions,
    },
    /// operation 已被接受的起步事实，先于任何实时执行事件落盘。
    OperationStarted {
        #[serde(rename = "operationId")]
        operation_id: String,
        kind: OperationKind,
        /// run operation 绑定的 turn id；独立 compaction 没有，为 None。
        #[serde(rename = "turnId", default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<String>,
    },
    /// operation 的终态。run 的记录同时就是这个 turn 唯一的终态事实：status/usage/
    /// truncated 同存于此，outcome 恒为终态。error 是 run 失败终态里可持久化的细节
    /// （stage/cause/message），也是该 turn 失败原因的长期来源，历史投影直接复用它。
    /// 成功、中断、独立 compaction 以及崩溃修复关闭的 operation，这里都是 None。
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

/// 会话条目：以 type 为标签的 tagged enum，序列化和严格类型校验都由 serde 生成。
///
/// payload 一律嵌成子对象（message/compaction/metadata/record），外层和各载荷都是
/// deny_unknown_fields——出现未知字段就拒绝。文件行的物理顺序就是追加顺序；模型上下文
/// 顺序另由 `session::context` 的投影安排（例如工具结果按 assistant 声明的顺序排列）。
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

/// 已持久化消息里某个文本块的稳定公开身份；消费方直接读取结果，不要自己拼后缀。
pub fn text_item_id(entry_id: &str, index: usize) -> String {
    format!("{entry_id}:text:{index}")
}

/// 已持久化消息里某个思考块的稳定公开身份。
pub fn thinking_item_id(entry_id: &str, index: usize) -> String {
    format!("{entry_id}:thinking:{index}")
}

/// 工具出现位置的稳定公开身份；即使 provider 复用了 call-ID 也不受影响。
pub fn tool_item_id(assistant_entry_id: &str, call_index: usize) -> String {
    format!("{assistant_entry_id}:tool:{call_index}")
}

/// session 文件头在磁盘上的形状：字段集固定、未知字段拒绝，读写共用这一份表示。
/// 各字段值原样保留磁盘内容——cwd 不在这里归一化，运行期路径由
/// [`SessionHeader::canonical_cwd`] 单独给出，修复写回也不会改写已存的路径。
/// 构造只有 `new`（写入）与 `parse`（读取）两个入口，类型不变量由它们把关。
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

/// 文件头 `type` 字段唯一允许的取值；写入端恒写此值，读取端只认此值。
const SESSION_HEADER_TYPE: &str = "session";

impl SessionHeader {
    /// 新建会话文件头：类型与版本由 schema 的持有者固定，调用方只提供身份信息。
    pub(super) fn new(session_id: String, cwd: String, timestamp: String) -> Self {
        Self {
            kind: SESSION_HEADER_TYPE.to_string(),
            version: CURRENT_SESSION_VERSION,
            id: session_id,
            timestamp,
            cwd,
        }
    }

    /// 从磁盘 JSON 解析文件头；字段形状和未知字段由 serde 把关，这里只做必须显式
    /// 表达的语义校验：type 取值、非空 UUID、版本必须是当前版本、创建时间非空。
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

    /// 已存 cwd 唯一的归一化入口：磁盘上的字面值仍留在 header 里，归一化结果
    /// 只供运行期使用，列表、Thread 投影与系统提示词共用这一形状。
    pub(super) fn canonical_cwd(&self) -> Result<String> {
        singularity_core::CanonicalWorkspacePath::from_saved(&self.cwd)
            .map_err(SessionError::InvalidHeader)
            .map(|cwd| cwd.display().to_string())
    }
}

pub(super) fn parse_entry(raw: Value, line: usize) -> Result<SessionEntry> {
    // 文件头只能出现在首行，中途再出现即视为损坏。
    if raw.get("type").and_then(Value::as_str) == Some(SESSION_HEADER_TYPE) {
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
