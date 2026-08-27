//! 应用事件：事件方法名、事件参数与构造器。

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::envelope::JsonRpcMessage;
use crate::params::{Thread, Turn};

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

/// `provider/attempt` 的稳定进度与终态词形。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderAttemptStatus {
    Started,
    Ok,
    Error,
    Cancelled,
}

/// `turn/error.error.stage` 的稳定管线阶段词形。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnFailureStage {
    AgentLoop,
    TerminalOutcome,
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
    /// 构造 thread started 事件。
    pub fn thread_started(thread: &Thread) -> Self {
        Self {
            method: event_method::THREAD_STARTED.to_string(),
            params: serde_json::to_value(ThreadEventParams {
                thread: thread.clone(),
            })
            .expect("typed event params serialize"),
        }
    }

    /// 构造 turn started 事件。
    pub fn turn_started(turn: &Turn) -> Self {
        Self {
            method: event_method::TURN_STARTED.to_string(),
            params: serde_json::to_value(TurnEventParams { turn: turn.clone() })
                .expect("typed event params serialize"),
        }
    }

    /// 构造 turn completed 事件。
    pub fn turn_completed(turn: &Turn) -> Self {
        Self {
            method: event_method::TURN_COMPLETED.to_string(),
            params: serde_json::to_value(TurnEventParams { turn: turn.clone() })
                .expect("typed event params serialize"),
        }
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
        Self {
            method: event_method::TURN_ERROR.to_string(),
            params: serde_json::to_value(params).expect("typed event params serialize"),
        }
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
        Self {
            method: event_method::AGENT_DIAGNOSTIC.to_string(),
            params: serde_json::to_value(params).expect("typed event params serialize"),
        }
    }

    /// 构造单次 Provider attempt 的脱敏进度/终态事件。
    pub fn provider_attempt(params: ProviderAttemptParams) -> Self {
        Self {
            method: event_method::PROVIDER_ATTEMPT.to_string(),
            params: serde_json::to_value(params).expect("typed event params serialize"),
        }
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
        Self {
            method: event_method::THREAD_SETTINGS_APPLIED.to_string(),
            params: serde_json::to_value(ThreadEventParams { thread: thread.clone() })
                .expect("typed event params serialize"),
        }
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
        Self {
            method: event_method::TOOL_EXECUTION_START.to_string(),
            params: serde_json::to_value(params).expect("typed event params serialize"),
        }
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
        Self {
            method: event_method::TOOL_EXECUTION_UPDATE.to_string(),
            params: serde_json::to_value(params).expect("typed event params serialize"),
        }
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
        Self {
            method: event_method::TOOL_EXECUTION_END.to_string(),
            params: serde_json::to_value(params).expect("typed event params serialize"),
        }
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
        Self {
            method: method.to_string(),
            params: serde_json::to_value(params).expect("typed event params serialize"),
        }
    }

    /// 返回事件方法名。
    pub fn method(&self) -> &str {
        &self.method
    }

    /// 将应用事件包装为 JSON-RPC 通知。
    pub fn to_notification(&self) -> JsonRpcMessage {
        JsonRpcMessage::notification(self.method.clone(), &self.params)
            .expect("application event params serialize")
    }
}
