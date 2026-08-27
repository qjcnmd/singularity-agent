//! 事件事实源：typed [`TurnEvent`] 与其观察端 [`TurnEventSink`]。
//!
//! 一个 turn 的全部生命周期与执行事件都从这一个枚举流出；文本渲染、JSONL
//! 输出、TUI 和 app-server 协议投影是同一事实的不同 adapter。`method()`
//! 给出该事件在流式输出中的稳定方法名，字段序列化为 camelCase。

use serde::{Deserialize, Serialize};
pub use singularity_agent::agent::AgentDiagnosticSeverity;
use singularity_model::{ModelErrorCategory, ProviderApiProtocol};

use crate::error::{TurnFailureCause, TurnFailureStage};
use crate::objects::{Thread, Turn};

/// provider attempt 在事件流中的进度或终态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderAttemptStatus {
    Started,
    Ok,
    Error,
    Cancelled,
}

impl ProviderAttemptStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Started => "started",
            Self::Ok => "ok",
            Self::Error => "error",
            Self::Cancelled => "cancelled",
        }
    }
}

impl std::fmt::Display for ProviderAttemptStatus {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// turn 执行事件的唯一类型化出口。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum TurnEvent {
    TurnStarted {
        turn: Turn,
    },
    ItemStarted {
        thread_id: String,
        turn_id: String,
        item_id: String,
    },
    AssistantDelta {
        thread_id: String,
        turn_id: String,
        item_id: String,
        delta: String,
    },
    ToolExecutionStart {
        thread_id: String,
        turn_id: String,
        tool_call_id: String,
        tool_name: String,
        args: serde_json::Value,
    },
    ToolExecutionUpdate {
        thread_id: String,
        turn_id: String,
        tool_call_id: String,
        tool_name: String,
        args: serde_json::Value,
        partial_result: String,
    },
    ToolExecutionEnd {
        thread_id: String,
        turn_id: String,
        tool_call_id: String,
        tool_name: String,
        result: String,
        is_error: bool,
    },
    ItemCompleted {
        thread_id: String,
        turn_id: String,
        item_id: String,
    },
    ItemFailed {
        thread_id: String,
        turn_id: String,
        item_id: String,
        error: String,
    },
    Diagnostic {
        thread_id: String,
        turn_id: String,
        severity: AgentDiagnosticSeverity,
        code: String,
        message: String,
    },
    ProviderAttempt {
        thread_id: String,
        turn_id: String,
        model_turn_ordinal: u32,
        provider: String,
        model: String,
        protocol: ProviderApiProtocol,
        status: ProviderAttemptStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attempt_duration_ms: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error_category: Option<ModelErrorCategory>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        diagnostic_code: Option<String>,
    },
    TurnCompleted {
        turn: Turn,
    },
    TurnFailed {
        turn: Turn,
        error: TurnErrorDetail,
    },
    /// 待生效设置已在可信终态后持久化并更新线程投影（下一 turn 生效）。
    ThreadSettingsApplied {
        thread: Thread,
    },
}

/// 终态失败的分类信息；message 已经过脱敏边界处理。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnErrorDetail {
    pub stage: TurnFailureStage,
    pub cause: TurnFailureCause,
    pub message: String,
}

impl TurnEvent {
    /// 事件在流式输出中的稳定方法名（与协议事件名一致）。
    pub fn method(&self) -> &'static str {
        match self {
            Self::TurnStarted { .. } => "turn/started",
            Self::ItemStarted { .. } => "item/started",
            Self::AssistantDelta { .. } => "item/agentMessage/delta",
            Self::ToolExecutionStart { .. } => "tool/execution/start",
            Self::ToolExecutionUpdate { .. } => "tool/execution/update",
            Self::ToolExecutionEnd { .. } => "tool/execution/end",
            Self::ItemCompleted { .. } => "item/completed",
            Self::ItemFailed { .. } => "item/failed",
            Self::Diagnostic { .. } => "agent/diagnostic",
            Self::ProviderAttempt { .. } => "provider/attempt",
            Self::TurnCompleted { .. } => "turn/completed",
            Self::TurnFailed { .. } => "turn/error",
            Self::ThreadSettingsApplied { .. } => "thread/settingsApplied",
        }
    }
}

/// 事件观察端。投影失败不得影响 Agent 执行：实现方自行吞掉并记录自身的
/// 投影错误，runtime 只保证按合同顺序调用。
pub trait TurnEventSink {
    fn emit(&mut self, event: TurnEvent);
}

impl<F> TurnEventSink for F
where
    F: FnMut(TurnEvent),
{
    fn emit(&mut self, event: TurnEvent) {
        self(event)
    }
}
