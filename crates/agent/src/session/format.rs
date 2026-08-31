//! 会话 JSONL schema、严格校验与公开格式类型。
//!
//! v4：在 v3 的线性消息/压缩序列之上，加入单 lane operation ledger 记录
//! （[`LedgerRecord`]）：operation 起止、step attempt、provider 观测、tool
//! 启动（含 replay 分类）、已接受控制。记录是审计与恢复事实，不进入模型
//! 上下文；消息与压缩条目仍是模型可见历史。turn 的终态唯一落盘位置是
//! `operation_finished`（run 记录携带 `turnId`）。

use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use singularity_model::{ModelConfigurationSnapshot, ModelUsage};
use singularity_protocol::{TurnModelUsage, TurnStatus};
use thiserror::Error;
use uuid::Uuid;

use crate::message::AgentMessage;
/// 唯一支持的当前会话格式版本。v4：单 lane operation ledger 记录进入条目
/// 词汇；全量未知字段拒绝；终态以单条 `operation_finished` 落盘。
pub const CURRENT_SESSION_VERSION: u32 = 4;
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
    #[serde(rename = "firstKeptEntryId")]
    pub first_kept_entry_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<TurnModelUsage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}
/// JSONL 中不参与模型上下文的持久化 metadata 类型。turn 生命周期事实由
/// [`LedgerRecord`] 承载，这里只保留 thread 级设置与名称。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionMetadataKind {
    ThreadSettings,
    ThreadName,
}

/// 领域 usage → 会话统一落盘形状 `TurnModelUsage`；`complete` 由调用方的
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
    pub fn kind(&self) -> SessionMetadataKind {
        match self {
            Self::ThreadSettings { .. } => SessionMetadataKind::ThreadSettings,
            Self::ThreadName { .. } => SessionMetadataKind::ThreadName,
        }
    }

    /// 只组装载荷，不做校验；不变量统一由 [`Self::validate`] 在写入路径
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

/// 压缩发起原因；随 step attempt 与 operation intent 落盘，恢复据此续接
/// 同一工作而不是猜测。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionReason {
    Manual,
    Threshold,
    Overflow,
}

/// operation 的不可变意图。run 携带本 turn 冻结的模型配置快照与规范化、
/// 不可变的本轮用户输入；compaction 携带发起原因。durable acceptance 之后
/// 不再改写。输入意图先于任何执行事件落盘，崩溃窗口不会丢失已接受的
/// run 输入（Pi 同形：`OperationStartedRecord.intent.originalPrompt`，
/// `D:\refs\pi\packages\agent\src\harness\session\types.ts:87-99`）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "intentType", rename_all = "snake_case", deny_unknown_fields)]
pub enum OperationIntent {
    Run {
        model: ModelConfigurationSnapshot,
        /// 本轮用户输入（steer 注入与 compaction 摘要请求不产生 run operation；
        /// 该输入与后续 user 消息条目同源，此处是 durable 接受事实）。
        input: String,
    },
    Compaction {
        reason: CompactionReason,
    },
}

/// step 种类：assistant 模型步与 compaction 摘要步。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepKind {
    Assistant,
    Compaction,
}

/// 工具调用的恢复重放分类。`never` 调用在结果未知时绝不自动重放。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolReplayClass {
    Safe,
    Never,
}

/// 预留 durable 条目的写入类别。记录先于目标条目落盘，恢复可据此区分
/// 已声明但尚未写入的结果，而不依赖进程内状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PendingWriteKind {
    AssistantMessage,
    Compaction,
    ToolResult,
}

/// 控制通道：即时转向、排队后续、取消。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlChannel {
    Steer,
    FollowUp,
    Cancel,
}

/// 已接受控制在落盘时刻的归宿。`Pending` 是接受时的初始状态，表示控制
/// 已 durable 接受但尚未执行或收敛；后续同 control_id 的记录将 disposition
/// 推进到终态（`Injected`/`StartedAsNewTurn`/`Cancelled`）。折叠时按
/// control_id 取最后一条记录（Pi 同形：`QueueEnqueuedRecord` 的持久接受 +
/// `AbortRequestedRecord` 的持久取消，`types.ts:115-176`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlDisposition {
    /// 接受时的初始状态；控制已 durable 接受但尚未执行或收敛。
    Pending,
    /// steer 输入已注入当前活动 turn。
    Injected,
    /// follow-up 或 requeued steer 已作为独立轮次启动。
    StartedAsNewTurn,
    /// 控制已取消或撤回。
    Cancelled,
}

/// 控制请求的运行时载体：接受时组装的 identity、payload 与接受顺序。
/// 一个 ControlRequest 生成两条 durable 记录（pending 接受 + 终态落盘），
/// 折叠后产生完整事实（data-model Control Request：stable identity,
/// channel, payload, sequence, acceptance FIFO, disposition lifecycle）。
/// 埋点于 `{turn_id}:{channel_word}:{sequence}` 格式的 control_id。
#[derive(Debug, Clone, PartialEq)]
pub struct ControlRequest {
    pub control_id: String,
    pub turn_id: String,
    pub channel: ControlChannel,
    pub sequence: u64,
    pub text: Option<String>,
}

/// 控制记录 identity 的单点构造形式：`{turn_id}:{channel_word}:{sequence}`。
/// channel_word 是 ControlChannel 的 serde snake_case 词形；
/// 所有控制记录的 control_id 字段均由此产生，归约据此推断所属 turn。
pub fn control_id(turn_id: &str, channel: ControlChannel, sequence: u64) -> String {
    let channel_word = match channel {
        ControlChannel::Steer => "steer",
        ControlChannel::FollowUp => "follow_up",
        ControlChannel::Cancel => "cancel",
    };
    format!("{turn_id}:{channel_word}:{sequence}")
}

impl ControlRequest {
    /// 构造 pending 接受记录（durable 接受事实）。
    pub fn pending_record(&self) -> LedgerRecord {
        LedgerRecord::ControlAccepted {
            control_id: self.control_id.clone(),
            turn_id: self.turn_id.clone(),
            channel: self.channel,
            sequence: self.sequence,
            disposition: ControlDisposition::Pending,
            text: self.text.clone(),
        }
    }

    /// 构造终态 disposition 记录（payload 不再重复——已存在于 pending 记录）。
    pub fn disposition_record(&self, disposition: ControlDisposition) -> LedgerRecord {
        LedgerRecord::ControlAccepted {
            control_id: self.control_id.clone(),
            turn_id: self.turn_id.clone(),
            channel: self.channel,
            sequence: self.sequence,
            disposition,
            text: None,
        }
    }
}

/// 单 lane operation ledger 记录：执行审计与恢复的唯一持久事实。记录只在
/// durable acceptance 后对消费者可见；物理行序即记录顺序（单调引用）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "recordType", rename_all = "snake_case", deny_unknown_fields)]
pub enum LedgerRecord {
    /// 已接受 operation 的意图；先于任何实时执行事件落盘。
    OperationStarted {
        #[serde(rename = "operationId")]
        operation_id: String,
        kind: OperationKind,
        /// run operation 绑定的 turn id；独立 compaction 为 `None`。
        #[serde(rename = "turnId", default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<String>,
        intent: OperationIntent,
    },
    /// operation 终态：run 记录同时是该 turn 的唯一终态事实（status/usage/
    /// truncated 单条原子落盘）。`outcome` 恒为终态（非 running）。
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
    },
    /// 一次 step 的第 N 次 attempt；重试产生新 attempt，绝不隐藏第二次执行。
    /// `result_entry_id` 预分配：恢复据此判定结果是否已落盘。
    StepAttempt {
        #[serde(rename = "operationId")]
        operation_id: String,
        step: StepKind,
        attempt: u32,
        #[serde(rename = "resultEntryId")]
        result_entry_id: String,
        #[serde(
            rename = "compactionReason",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        compaction_reason: Option<CompactionReason>,
    },
    /// 一次出站模型请求的终态观测（attempt 与 step attempt 序号对齐）。
    ProviderAttempt {
        #[serde(rename = "operationId")]
        operation_id: String,
        attempt: u32,
        provider: String,
        model: String,
        protocol: String,
        status: singularity_protocol::ProviderAttemptStatus,
        #[serde(rename = "attemptDurationMs", default)]
        attempt_duration_ms: Option<u64>,
        #[serde(
            rename = "errorCategory",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        error_category: Option<String>,
        #[serde(
            rename = "diagnosticCode",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        diagnostic_code: Option<String>,
        #[serde(
            rename = "retryAfterMs",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        retry_after_ms: Option<u64>,
        #[serde(
            rename = "retryAfterSource",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        retry_after_source: Option<singularity_protocol::RetryAfterSource>,
    },
    /// 已接受但目标条目尚未落盘的写入意图。目标条目出现后由归约器
    /// 自动闭合；未闭合记录会保留在 operation 状态供恢复处理。
    WriteDeferred {
        #[serde(rename = "operationId")]
        operation_id: String,
        #[serde(rename = "entryId")]
        entry_id: String,
        kind: PendingWriteKind,
    },
    /// Recovery explicitly abandons a deferred write whose target cannot be
    /// reconstructed without inventing an execution result.
    WriteAbandoned {
        #[serde(rename = "operationId")]
        operation_id: String,
        #[serde(rename = "entryId")]
        entry_id: String,
        kind: PendingWriteKind,
        reason: String,
    },
    /// 工具调用已开始执行（副作用可能已发生）；先于执行落盘。
    ToolStarted {
        #[serde(rename = "operationId")]
        operation_id: String,
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
        #[serde(rename = "toolName")]
        tool_name: String,
        /// 模型响应内的 source order（0 起）。
        #[serde(rename = "sourceOrder")]
        source_order: u32,
        #[serde(rename = "effectiveArgs")]
        effective_args: Value,
        #[serde(rename = "resultEntryId")]
        result_entry_id: String,
        replay: ToolReplayClass,
    },
    /// 协调器已接受的控制输入；sequence 是 FIFO 接受顺序的权威落盘。
    /// 接受时先落 disposition `pending`（携带 payload 与 turn_id），消费或
    /// 收敛时以同一 control_id 落终态 disposition（payload 不再重复）。
    /// 归约按 control_id 折叠出当前 disposition。
    ControlAccepted {
        #[serde(rename = "controlId")]
        control_id: String,
        /// 接受时刻的活动 turn（data-model Control Request.target_turn_id）；
        /// follow-up 的终态由后续轮次写入，identity 不变。
        #[serde(rename = "turnId")]
        turn_id: String,
        channel: ControlChannel,
        sequence: u64,
        disposition: ControlDisposition,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
    },
}

impl LedgerRecord {
    pub fn operation_id(&self) -> &str {
        match self {
            Self::OperationStarted { operation_id, .. }
            | Self::OperationFinished { operation_id, .. }
            | Self::StepAttempt { operation_id, .. }
            | Self::ProviderAttempt { operation_id, .. }
            | Self::WriteDeferred { operation_id, .. }
            | Self::WriteAbandoned { operation_id, .. }
            | Self::ToolStarted { operation_id, .. } => operation_id,
            Self::ControlAccepted { control_id, .. } => control_id,
        }
    }
}

/// 会话条目：以 `type` 为标签的 tagged enum，serde 生成序列化与严格类型校验。
///
/// payload 一律嵌套为子对象（`message`/`compaction`/`metadata`/`record`），外层
/// 与各载荷均 `deny_unknown_fields`——未知字段写入即拒绝。会话是严格的线性
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
