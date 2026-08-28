//! 应用事件：事件方法名、事件参数与构造器。

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::envelope::JsonRpcMessage;
use crate::params::{Thread, ThreadStatus, Turn, TurnModelUsage, TurnStatus};

/// 对外广播的应用事件方法名（事件信封 method 字段的唯一来源）。
pub mod event_method {
    pub const THREAD_STARTED: &str = "thread/started";
    pub const TURN_STARTED: &str = "turn/started";
    pub const TURN_COMPLETED: &str = "turn/completed";
    pub const TURN_ERROR: &str = "turn/error";
    pub const AGENT_DIAGNOSTIC: &str = "agent/diagnostic";
    pub const PROVIDER_ATTEMPT: &str = "provider/attempt";
    pub const ITEM_STARTED: &str = "item/started";
    pub const ITEM_AGENT_MESSAGE_DELTA: &str = "item/agentMessage/delta";
    pub const ITEM_COMPLETED: &str = "item/completed";
    pub const ITEM_FAILED: &str = "item/failed";
    pub const TOOL_EXECUTION_START: &str = "tool/execution/start";
    pub const TOOL_EXECUTION_UPDATE: &str = "tool/execution/update";
    pub const TOOL_EXECUTION_END: &str = "tool/execution/end";
    pub const THREAD_SETTINGS_APPLIED: &str = "thread/settingsApplied";
}

/// runtime 直接发射的稳定诊断代码。
pub mod diagnostic_code {
    pub const PROJECT_INSTRUCTIONS_TRUNCATED: &str = "project_instructions_truncated";
    pub const STORAGE_FATAL: &str = "storage_fatal";
}

/// runtime 与客户端共同消费的线程事件投影。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecutionThread {
    pub thread_id: String,
    pub cwd: String,
    pub model: Option<String>,
    pub last_turn_status: Option<ThreadStatus>,
}

/// runtime 与客户端共同消费的 turn usage 投影。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecutionTurnUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    pub cached_input_tokens: u64,
    pub reasoning_tokens: u64,
    pub usage_present: bool,
    pub usage_complete: bool,
}

/// runtime 与客户端共同消费的 turn 事件投影。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecutionTurn {
    pub turn_id: String,
    pub thread_id: String,
    pub status: TurnStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<ExecutionTurnUsage>,
}

/// 终态失败的分类信息；message 已经过脱敏边界处理。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnErrorDetail {
    pub stage: TurnFailureStage,
    pub cause: TurnFailureCause,
    pub message: String,
}

/// turn 执行事件的唯一类型化出口。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum TurnEvent {
    TurnStarted {
        turn: ExecutionTurn,
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
        args: Value,
    },
    ToolExecutionUpdate {
        thread_id: String,
        turn_id: String,
        tool_call_id: String,
        tool_name: String,
        args: Value,
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
        severity: DiagnosticSeverity,
        code: String,
        message: String,
    },
    ProviderAttempt {
        thread_id: String,
        turn_id: String,
        model_turn_ordinal: u32,
        provider: String,
        model: String,
        protocol: String,
        status: ProviderAttemptStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attempt_duration_ms: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error_category: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        diagnostic_code: Option<String>,
    },
    TurnCompleted {
        turn: ExecutionTurn,
    },
    TurnFailed {
        turn: ExecutionTurn,
        error: TurnErrorDetail,
    },
    ThreadSettingsApplied {
        thread: ExecutionThread,
    },
}

impl TurnEvent {
    pub const fn method(&self) -> &'static str {
        match self {
            Self::TurnStarted { .. } => event_method::TURN_STARTED,
            Self::ItemStarted { .. } => event_method::ITEM_STARTED,
            Self::AssistantDelta { .. } => event_method::ITEM_AGENT_MESSAGE_DELTA,
            Self::ToolExecutionStart { .. } => event_method::TOOL_EXECUTION_START,
            Self::ToolExecutionUpdate { .. } => event_method::TOOL_EXECUTION_UPDATE,
            Self::ToolExecutionEnd { .. } => event_method::TOOL_EXECUTION_END,
            Self::ItemCompleted { .. } => event_method::ITEM_COMPLETED,
            Self::ItemFailed { .. } => event_method::ITEM_FAILED,
            Self::Diagnostic { .. } => event_method::AGENT_DIAGNOSTIC,
            Self::ProviderAttempt { .. } => event_method::PROVIDER_ATTEMPT,
            Self::TurnCompleted { .. } => event_method::TURN_COMPLETED,
            Self::TurnFailed { .. } => event_method::TURN_ERROR,
            Self::ThreadSettingsApplied { .. } => event_method::THREAD_SETTINGS_APPLIED,
        }
    }
}

/// `thread/started` 事件参数。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadEventParams {
    pub thread: Thread,
}

/// `turn/started` 与 `turn/completed` 事件参数。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnEventParams {
    pub turn: Turn,
}

/// item 事件的条目引用。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ItemRef {
    #[serde(rename = "itemId")]
    pub item_id: String,
}

/// item 生命周期事件参数（started/completed/delta/failed 共用）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ItemEventParams {
    #[serde(rename = "threadId")]
    pub thread_id: String,
    #[serde(rename = "turnId")]
    pub turn_id: String,
    pub item: ItemRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delta: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// `tool/execution/start` 事件参数。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolExecutionStartParams {
    #[serde(rename = "threadId")]
    pub thread_id: String,
    #[serde(rename = "turnId")]
    pub turn_id: String,
    #[serde(rename = "toolCallId")]
    pub tool_call_id: String,
    #[serde(rename = "toolName")]
    pub tool_name: String,
    pub args: Value,
}

/// `tool/execution/update` 事件参数。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolExecutionUpdateParams {
    #[serde(rename = "threadId")]
    pub thread_id: String,
    #[serde(rename = "turnId")]
    pub turn_id: String,
    #[serde(rename = "toolCallId")]
    pub tool_call_id: String,
    #[serde(rename = "toolName")]
    pub tool_name: String,
    pub args: Value,
    #[serde(rename = "partialResult")]
    pub partial_result: String,
}

/// `tool/execution/end` 事件参数。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolExecutionEndParams {
    #[serde(rename = "threadId")]
    pub thread_id: String,
    #[serde(rename = "turnId")]
    pub turn_id: String,
    #[serde(rename = "toolCallId")]
    pub tool_call_id: String,
    #[serde(rename = "toolName")]
    pub tool_name: String,
    pub result: ToolExecutionResult,
}

/// 工具执行结果的类型化载体。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolExecutionResult {
    pub content: Vec<ToolExecutionTextPart>,
    #[serde(rename = "isError")]
    pub is_error: bool,
}

/// 工具执行结果中的一段纯文本。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolExecutionTextPart {
    #[serde(rename = "type")]
    pub kind: ToolResultPartType,
    pub text: String,
}

/// 工具执行结果片段类型词形。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolResultPartType {
    Text,
}

/// 对外广播的应用事件。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppEvent {
    pub method: String,
    pub params: Value,
}

/// `agent/diagnostic` 的稳定严重级别词形。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticSeverity {
    Info,
    Warning,
    Error,
}

impl DiagnosticSeverity {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warning => "warning",
            Self::Error => "error",
        }
    }
}

impl std::fmt::Display for DiagnosticSeverity {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// `provider/attempt` 的稳定进度与终态词形。
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

/// `turn/error.error.stage` 的稳定管线阶段词形。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnFailureStage {
    AgentLoop,
    TerminalOutcome,
}

impl TurnFailureStage {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AgentLoop => "agent_loop",
            Self::TerminalOutcome => "terminal_outcome",
        }
    }
}

impl std::fmt::Display for TurnFailureStage {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// `turn/error.error.cause` 的稳定失败来源词形。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnFailureCause {
    Store,
    ProjectInstructions,
    Workspace,
    ProviderRateLimited,
    ProviderNetwork,
    ProviderTimeout,
    ProviderAuth,
    ProviderValidation,
    ProviderOverloaded,
    ProviderCancelled,
    ProviderContextOverflow,
    ProviderUnknown,
    Serialization,
    Internal,
}

impl TurnFailureCause {
    pub const fn wire_str(self) -> &'static str {
        match self {
            Self::Store => "store",
            Self::ProjectInstructions => "project_instructions",
            Self::Workspace => "workspace",
            Self::ProviderRateLimited => "provider_rate_limited",
            Self::ProviderNetwork => "provider_network",
            Self::ProviderTimeout => "provider_timeout",
            Self::ProviderAuth => "provider_auth",
            Self::ProviderValidation => "provider_validation",
            Self::ProviderOverloaded => "provider_overloaded",
            Self::ProviderCancelled => "provider_cancelled",
            Self::ProviderContextOverflow => "provider_context_overflow",
            Self::ProviderUnknown => "provider_unknown",
            Self::Serialization => "serialization",
            Self::Internal => "internal",
        }
    }

    pub fn as_str(self) -> &'static str {
        self.wire_str()
            .strip_prefix("provider_")
            .unwrap_or_else(|| self.wire_str())
    }
}

impl std::fmt::Display for TurnFailureCause {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.wire_str())
    }
}

impl From<&ExecutionThread> for Thread {
    fn from(thread: &ExecutionThread) -> Self {
        Self {
            thread_id: thread.thread_id.clone(),
            model: thread.model.clone(),
            cwd: thread.cwd.clone(),
            last_turn_status: thread.last_turn_status,
        }
    }
}

impl From<&ExecutionTurn> for Turn {
    fn from(turn: &ExecutionTurn) -> Self {
        Self {
            turn_id: turn.turn_id.clone(),
            thread_id: turn.thread_id.clone(),
            status: turn.status,
            model_usage: turn.usage.as_ref().map(TurnModelUsage::from),
        }
    }
}

impl From<&ExecutionTurnUsage> for TurnModelUsage {
    fn from(usage: &ExecutionTurnUsage) -> Self {
        Self {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            total_tokens: usage.total_tokens,
            cached_input_tokens: usage.cached_input_tokens,
            reasoning_tokens: usage.reasoning_tokens,
            usage_present: usage.usage_present,
            usage_complete: usage.usage_complete,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnErrorParams {
    #[serde(rename = "turnId")]
    pub turn_id: String,
    #[serde(rename = "threadId")]
    pub thread_id: String,
    pub error: TurnError,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnError {
    pub stage: TurnFailureStage,
    pub cause: TurnFailureCause,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentDiagnosticParams {
    #[serde(rename = "threadId")]
    pub thread_id: String,
    #[serde(rename = "turnId")]
    pub turn_id: String,
    pub severity: DiagnosticSeverity,
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderAttemptParams {
    #[serde(rename = "threadId")]
    pub thread_id: String,
    #[serde(rename = "turnId")]
    pub turn_id: String,
    pub model_turn_ordinal: u32,
    pub provider: String,
    pub model: String,
    pub protocol: String,
    pub status: ProviderAttemptStatus,
    pub attempt_duration_ms: Option<u64>,
    pub error_category: Option<String>,
    pub diagnostic_code: Option<String>,
}

impl AppEvent {
    /// 构造一个带类型化参数的应用事件；参数结构恒可序列化，失败视为
    /// crate 内部错误。
    fn build(method: &str, params: impl Serialize) -> Self {
        // 不变量：params 均为本 crate 的静态类型，无 #[serde(skip)] 字段，序列化恒不失败。
        #[allow(clippy::expect_used)]
        let params = serde_json::to_value(params).expect("typed event params serialize");
        Self {
            method: method.to_string(),
            params,
        }
    }

    /// 将共享执行事件投影为稳定的 JSON-RPC 事件参数。
    pub fn from_turn_event(event: &TurnEvent) -> Self {
        match event {
            TurnEvent::TurnStarted { turn } => Self::turn_started(&Turn::from(turn)),
            TurnEvent::TurnCompleted { turn } => Self::turn_completed(&Turn::from(turn)),
            TurnEvent::TurnFailed { turn, error } => Self::turn_error(
                &turn.turn_id,
                &turn.thread_id,
                error.stage,
                error.cause,
                &error.message,
            ),
            TurnEvent::ThreadSettingsApplied { thread } => {
                Self::thread_settings_applied(&Thread::from(thread))
            }
            TurnEvent::ItemStarted {
                thread_id,
                turn_id,
                item_id,
            } => Self::item_started(thread_id, turn_id, item_id),
            TurnEvent::AssistantDelta {
                thread_id,
                turn_id,
                item_id,
                delta,
            } => Self::item_agent_message_delta(thread_id, turn_id, item_id, delta),
            TurnEvent::ItemCompleted {
                thread_id,
                turn_id,
                item_id,
            } => Self::item_completed(thread_id, turn_id, item_id),
            TurnEvent::ItemFailed {
                thread_id,
                turn_id,
                item_id,
                error,
            } => Self::item_failed(thread_id, turn_id, item_id, error),
            TurnEvent::ToolExecutionStart {
                thread_id,
                turn_id,
                tool_call_id,
                tool_name,
                args,
            } => Self::tool_execution_start(
                thread_id,
                turn_id,
                tool_call_id,
                tool_name,
                args.clone(),
            ),
            TurnEvent::ToolExecutionUpdate {
                thread_id,
                turn_id,
                tool_call_id,
                tool_name,
                args,
                partial_result,
            } => Self::tool_execution_update(
                thread_id,
                turn_id,
                tool_call_id,
                tool_name,
                args.clone(),
                partial_result,
            ),
            TurnEvent::ToolExecutionEnd {
                thread_id,
                turn_id,
                tool_call_id,
                tool_name,
                result,
                is_error,
            } => Self::tool_execution_end(
                thread_id,
                turn_id,
                tool_call_id,
                tool_name,
                result,
                *is_error,
            ),
            TurnEvent::Diagnostic {
                thread_id,
                turn_id,
                severity,
                code,
                message,
            } => Self::agent_diagnostic(thread_id, turn_id, *severity, code, message),
            TurnEvent::ProviderAttempt {
                thread_id,
                turn_id,
                model_turn_ordinal,
                provider,
                model,
                protocol,
                status,
                attempt_duration_ms,
                error_category,
                diagnostic_code,
            } => Self::provider_attempt(ProviderAttemptParams {
                thread_id: thread_id.clone(),
                turn_id: turn_id.clone(),
                model_turn_ordinal: *model_turn_ordinal,
                provider: provider.clone(),
                model: model.clone(),
                protocol: protocol.clone(),
                status: *status,
                attempt_duration_ms: *attempt_duration_ms,
                error_category: error_category.clone(),
                diagnostic_code: diagnostic_code.clone(),
            }),
        }
    }

    /// 构造 thread started 事件。
    pub fn thread_started(thread: &Thread) -> Self {
        Self::build(
            event_method::THREAD_STARTED,
            ThreadEventParams {
                thread: thread.clone(),
            },
        )
    }

    /// 构造 turn started 事件。
    pub fn turn_started(turn: &Turn) -> Self {
        Self::build(
            event_method::TURN_STARTED,
            TurnEventParams { turn: turn.clone() },
        )
    }

    /// 构造 turn completed 事件。
    pub fn turn_completed(turn: &Turn) -> Self {
        Self::build(
            event_method::TURN_COMPLETED,
            TurnEventParams { turn: turn.clone() },
        )
    }

    /// 构造 Turn 执行错误终态事件。
    pub fn turn_error(
        turn_id: &str,
        thread_id: &str,
        stage: TurnFailureStage,
        cause: TurnFailureCause,
        message: &str,
    ) -> Self {
        let params = TurnErrorParams {
            turn_id: turn_id.to_string(),
            thread_id: thread_id.to_string(),
            error: TurnError {
                stage,
                cause,
                message: message.to_string(),
            },
        };
        Self::build(event_method::TURN_ERROR, params)
    }

    /// 构造非致命、脱敏的 Agent 诊断事件。
    pub fn agent_diagnostic(
        thread_id: impl Into<String>,
        turn_id: impl Into<String>,
        severity: DiagnosticSeverity,
        code: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        let params = AgentDiagnosticParams {
            thread_id: thread_id.into(),
            turn_id: turn_id.into(),
            severity,
            code: code.into(),
            message: message.into(),
        };
        Self::build(event_method::AGENT_DIAGNOSTIC, params)
    }

    /// 构造单次 Provider attempt 的脱敏进度/终态事件。
    pub fn provider_attempt(params: ProviderAttemptParams) -> Self {
        Self::build(event_method::PROVIDER_ATTEMPT, params)
    }

    /// 构造 item started 事件。
    pub fn item_started(
        thread_id: impl Into<String>,
        turn_id: impl Into<String>,
        item_id: impl Into<String>,
    ) -> Self {
        Self::item_event(
            event_method::ITEM_STARTED,
            thread_id,
            turn_id,
            item_id,
            None,
            None,
        )
    }

    /// 构造线程设置已应用事件（待生效设置在可信终态后持久化并更新线程投影）。
    pub fn thread_settings_applied(thread: &Thread) -> Self {
        Self::build(
            event_method::THREAD_SETTINGS_APPLIED,
            ThreadEventParams {
                thread: thread.clone(),
            },
        )
    }

    /// 构造 agent message 增量事件。
    pub fn item_agent_message_delta(
        thread_id: impl Into<String>,
        turn_id: impl Into<String>,
        item_id: impl Into<String>,
        delta: impl Into<String>,
    ) -> Self {
        Self::item_event(
            event_method::ITEM_AGENT_MESSAGE_DELTA,
            thread_id,
            turn_id,
            item_id,
            Some(delta.into()),
            None,
        )
    }

    /// 构造 item completed 事件。
    pub fn item_completed(
        thread_id: impl Into<String>,
        turn_id: impl Into<String>,
        item_id: impl Into<String>,
    ) -> Self {
        Self::item_event(
            event_method::ITEM_COMPLETED,
            thread_id,
            turn_id,
            item_id,
            None,
            None,
        )
    }

    /// 构造 item failed 事件。
    pub fn item_failed(
        thread_id: impl Into<String>,
        turn_id: impl Into<String>,
        item_id: impl Into<String>,
        error: impl Into<String>,
    ) -> Self {
        Self::item_event(
            event_method::ITEM_FAILED,
            thread_id,
            turn_id,
            item_id,
            None,
            Some(error.into()),
        )
    }

    /// 构造工具开始执行事件通知。
    pub fn tool_execution_start(
        thread_id: impl Into<String>,
        turn_id: impl Into<String>,
        tool_call_id: impl Into<String>,
        tool_name: impl Into<String>,
        args: Value,
    ) -> Self {
        let params = ToolExecutionStartParams {
            thread_id: thread_id.into(),
            turn_id: turn_id.into(),
            tool_call_id: tool_call_id.into(),
            tool_name: tool_name.into(),
            args,
        };
        Self::build(event_method::TOOL_EXECUTION_START, params)
    }

    /// 构造工具执行流式输出增量更新通知。
    pub fn tool_execution_update(
        thread_id: impl Into<String>,
        turn_id: impl Into<String>,
        tool_call_id: impl Into<String>,
        tool_name: impl Into<String>,
        args: Value,
        partial_result: impl Into<String>,
    ) -> Self {
        let params = ToolExecutionUpdateParams {
            thread_id: thread_id.into(),
            turn_id: turn_id.into(),
            tool_call_id: tool_call_id.into(),
            tool_name: tool_name.into(),
            args,
            partial_result: partial_result.into(),
        };
        Self::build(event_method::TOOL_EXECUTION_UPDATE, params)
    }

    /// 构造工具执行完成事件通知。
    pub fn tool_execution_end(
        thread_id: impl Into<String>,
        turn_id: impl Into<String>,
        tool_call_id: impl Into<String>,
        tool_name: impl Into<String>,
        result: impl Into<String>,
        is_error: bool,
    ) -> Self {
        let params = ToolExecutionEndParams {
            thread_id: thread_id.into(),
            turn_id: turn_id.into(),
            tool_call_id: tool_call_id.into(),
            tool_name: tool_name.into(),
            result: ToolExecutionResult {
                content: vec![ToolExecutionTextPart {
                    kind: ToolResultPartType::Text,
                    text: result.into(),
                }],
                is_error,
            },
        };
        Self::build(event_method::TOOL_EXECUTION_END, params)
    }

    fn item_event(
        method: &'static str,
        thread_id: impl Into<String>,
        turn_id: impl Into<String>,
        item_id: impl Into<String>,
        delta: Option<String>,
        error: Option<String>,
    ) -> Self {
        let params = ItemEventParams {
            thread_id: thread_id.into(),
            turn_id: turn_id.into(),
            item: ItemRef {
                item_id: item_id.into(),
            },
            delta,
            error,
        };
        Self::build(method, params)
    }

    /// 返回事件方法名。
    pub fn method(&self) -> &str {
        &self.method
    }

    /// 将应用事件包装为 JSON-RPC 通知。
    // 不变量：params 在 build 时已成功序列化，此处再次包装恒不失败。
    #[allow(clippy::expect_used)]
    pub fn to_notification(&self) -> JsonRpcMessage {
        JsonRpcMessage::notification(self.method.clone(), &self.params)
            .expect("application event params serialize")
    }
}
