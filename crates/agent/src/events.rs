//! Agent 运行事件出口：生命周期事件、脱敏诊断与尽力而为的发射 helper。
//!
//! 事件统一经 [`AgentEvents::on_event`] 流式投递；投影为尽力而为，
//! 消费方自行吸收失败，不改变轮次结果。

use serde_json::Value;
use singularity_model::ProviderAttemptEvent;

use crate::tools::ToolExecution;

pub(crate) mod diagnostic_code {
    pub const COMPACTION_FAILED: &str = "compaction_failed";
    pub const COMPACTION_SKIPPED: &str = "compaction_skipped";
    pub const CONTEXT_OVERFLOW_RECOVERY_FAILED: &str = "context_overflow_recovery_failed";
    pub const PROVIDER_RETRY_SCHEDULED: &str = "provider_retry_scheduled";
}

/// 非致命运行时诊断的严重级别（AgentLoop 发射）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentDiagnosticSeverity {
    Info,
    Warning,
    Error,
}

impl AgentDiagnosticSeverity {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warning => "warning",
            Self::Error => "error",
        }
    }
}

impl std::fmt::Display for AgentDiagnosticSeverity {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// 安全、非持久化的诊断。`code` 对投影方稳定；`message` 文本刻意
/// 不包含原始 provider payload（脱敏边界）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentDiagnostic {
    pub severity: AgentDiagnosticSeverity,
    pub code: String,
    pub message: String,
}

impl AgentDiagnostic {
    pub fn info(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            severity: AgentDiagnosticSeverity::Info,
            code: code.into(),
            message: message.into(),
        }
    }

    pub fn warning(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            severity: AgentDiagnosticSeverity::Warning,
            code: code.into(),
            message: message.into(),
        }
    }
}

/// Agent 运行生命周期事件，统一经 `AgentEvents::on_event` 出口流式投递。
///
/// tool 事件按调用的串行执行顺序投递；持久化的 toolResult 顺序不受影响。
#[derive(Debug, Clone, PartialEq)]
pub enum AgentEvent {
    /// 模型流式文本输出增量更新。
    MessageUpdate { delta: String },
    /// assistant 消息中的思考块事实；持久化该消息后逐块上报。
    Thinking { text: String },
    /// 工具开始执行事件。
    ToolExecutionStarted {
        tool_name: String,
        tool_call_id: String,
        arguments: Value,
    },
    /// 工具执行中产生的流式增量输出事件。
    ToolExecutionUpdate {
        tool_name: String,
        tool_call_id: String,
        arguments: Value,
        partial_result: String,
    },
    /// 工具执行完成事件。
    ToolExecutionEnded {
        tool_name: String,
        tool_call_id: String,
        execution: ToolExecution,
    },
    /// 非致命、脱敏 Agent 诊断；不会写入 Session JSONL。
    Diagnostic(AgentDiagnostic),
    /// provider HTTP attempt 生命周期观测；model-turn 序号已在循环内绑定。
    ///
    /// 投影为尽力而为；消费方自行吸收投影失败，不影响 provider 结果。
    ProviderAttempt {
        model_turn_ordinal: u32,
        event: ProviderAttemptEvent,
    },
}

/// Agent 运行生命周期事件出口。
///
/// 单一回调统一承载全部事件。投影为尽力而为：消费方自行吸收失败，
/// 不改变轮次结果。
pub struct AgentEvents<'a> {
    pub on_event: Option<&'a mut dyn FnMut(AgentEvent)>,
}

impl<'a> AgentEvents<'a> {
    pub fn new() -> Self {
        Self { on_event: None }
    }
}

impl Default for AgentEvents<'_> {
    fn default() -> Self {
        Self::new()
    }
}

/// 尽力而为的事件发射：无消费者或投影失败都只丢弃该事件。
pub(crate) fn emit(events: &mut AgentEvents<'_>, event: AgentEvent) {
    if let Some(callback) = events.on_event.as_deref_mut() {
        callback(event);
    }
}

/// 非致命诊断的统一发射侧信道。
pub(crate) fn emit_diagnostic(events: &mut AgentEvents, diagnostic: AgentDiagnostic) {
    emit(events, AgentEvent::Diagnostic(diagnostic));
}
