//! 执行事件唯一事实源与两面 wire 投影。
//!
//! [`TurnEvent`] 是 runtime 直接发射、全部客户端共同消费的唯一事件形态；
//! 两个对外表面各有一个显式投影函数，方法名由 [`TurnEvent::method`] 单点定义：
//!
//! - [`turn_event_notification`]：桌面端 JSON-RPC 通知（camelCase wire，
//!   嵌套形状在本文件唯一一处组装）；
//! - [`turn_event_jsonl_params`]：`sg --json` 事件行的 `params`（snake_case）。
//!
//! `thread/started` 不是执行事件，由 app-server 作为桌面端局部生命周期通知
//! 自行发出。Agent 内部诊断 code 由 agent 事件模块定义；runtime 诊断 code 由
//! [`diagnostic_code`] 定义。

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::envelope::JsonRpcMessage;
use crate::params::{Thread, ThreadStatus, Turn, TurnModelUsage, TurnStatus};

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

/// turn 执行事件的唯一类型化出口。纯数据载体：不携带任何 serde derive，
/// 两面 wire 形状只存在于本文件的投影函数中。
#[derive(Debug, Clone, PartialEq)]
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
    /// assistant 消息内的思考块事实；持久化后实时逐块发布。
    AssistantThinking {
        thread_id: String,
        turn_id: String,
        text: String,
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
        attempt_duration_ms: Option<u64>,
        error_category: Option<String>,
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
    /// 事件方法名的唯一词表：两面投影与 `--json` 行都从这里取方法名。
    pub const fn method(&self) -> &'static str {
        match self {
            Self::TurnStarted { .. } => "turn/started",
            Self::ItemStarted { .. } => "item/started",
            Self::AssistantDelta { .. } => "item/agentMessage/delta",
            Self::AssistantThinking { .. } => "item/agentThinking",
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

/// 桌面端 JSON-RPC 通知：`turn/started` 等方法的信封消息。
// 不变量：params 为刚组装完成的 Value，不存在序列化失败路径。
#[allow(clippy::expect_used)]
pub fn turn_event_notification(event: &TurnEvent) -> JsonRpcMessage {
    JsonRpcMessage::notification(event.method(), turn_event_wire_params(event))
        .expect("notification with json value params serializes")
}

/// 桌面端 wire 的 `params` 形状（camelCase，item/result 嵌套形态在此唯一一处
/// 定义）。与 `--json` 面的差异（`modelUsage` 键名、`turn/error` 平铺、可选
/// 字段以 null 出现）都是桌面端协议的既有合同，由 golden 测试逐字钉住。
fn turn_event_wire_params(event: &TurnEvent) -> Value {
    match event {
        TurnEvent::TurnStarted { turn } => json!({"turn": Turn::from(turn)}),
        TurnEvent::TurnCompleted { turn } => json!({"turn": Turn::from(turn)}),
        TurnEvent::TurnFailed { turn, error } => json!({
            "turnId": turn.turn_id,
            "threadId": turn.thread_id,
            "error": {
                "stage": error.stage.as_str(),
                "cause": error.cause.wire_str(),
                "message": error.message,
            },
        }),
        TurnEvent::ThreadSettingsApplied { thread } => json!({"thread": Thread::from(thread)}),
        TurnEvent::ItemStarted {
            thread_id,
            turn_id,
            item_id,
        } => json!({
            "threadId": thread_id,
            "turnId": turn_id,
            "item": {"itemId": item_id},
        }),
        TurnEvent::AssistantDelta {
            thread_id,
            turn_id,
            item_id,
            delta,
        } => json!({
            "threadId": thread_id,
            "turnId": turn_id,
            "item": {"itemId": item_id},
            "delta": delta,
        }),
        TurnEvent::AssistantThinking {
            thread_id,
            turn_id,
            text,
        } => json!({"threadId": thread_id, "turnId": turn_id, "text": text}),
        TurnEvent::ItemCompleted {
            thread_id,
            turn_id,
            item_id,
        } => json!({
            "threadId": thread_id,
            "turnId": turn_id,
            "item": {"itemId": item_id},
        }),
        TurnEvent::ItemFailed {
            thread_id,
            turn_id,
            item_id,
            error,
        } => json!({
            "threadId": thread_id,
            "turnId": turn_id,
            "item": {"itemId": item_id},
            "error": error,
        }),
        TurnEvent::ToolExecutionStart {
            thread_id,
            turn_id,
            tool_call_id,
            tool_name,
            args,
        } => json!({
            "threadId": thread_id,
            "turnId": turn_id,
            "toolCallId": tool_call_id,
            "toolName": tool_name,
            "args": args,
        }),
        TurnEvent::ToolExecutionUpdate {
            thread_id,
            turn_id,
            tool_call_id,
            tool_name,
            args,
            partial_result,
        } => json!({
            "threadId": thread_id,
            "turnId": turn_id,
            "toolCallId": tool_call_id,
            "toolName": tool_name,
            "args": args,
            "partialResult": partial_result,
        }),
        TurnEvent::ToolExecutionEnd {
            thread_id,
            turn_id,
            tool_call_id,
            tool_name,
            result,
            is_error,
        } => json!({
            "threadId": thread_id,
            "turnId": turn_id,
            "toolCallId": tool_call_id,
            "toolName": tool_name,
            "result": {
                "content": [{"type": "text", "text": result}],
                "isError": is_error,
            },
        }),
        TurnEvent::Diagnostic {
            thread_id,
            turn_id,
            severity,
            code,
            message,
        } => json!({
            "threadId": thread_id,
            "turnId": turn_id,
            "severity": severity.as_str(),
            "code": code,
            "message": message,
        }),
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
        } => json!({
            "threadId": thread_id,
            "turnId": turn_id,
            "modelTurnOrdinal": model_turn_ordinal,
            "provider": provider,
            "model": model,
            "protocol": protocol,
            "status": status.as_str(),
            // 可选字段在桌面端 wire 上恒出现：无值时为 null。
            "attemptDurationMs": attempt_duration_ms,
            "errorCategory": error_category,
            "diagnosticCode": diagnostic_code,
        }),
    }
}

/// `sg --json` 事件行的 `params` 形状（snake_case；缺值的可选字段整体省略，
/// 与桌面端 wire 的 null 语义刻意不同，两面的合同由 protocol golden 测试分别
/// 钉住）。
pub fn turn_event_jsonl_params(event: &TurnEvent) -> Value {
    match event {
        TurnEvent::TurnStarted { turn } | TurnEvent::TurnCompleted { turn } => {
            json!({"turn": turn})
        }
        TurnEvent::TurnFailed { turn, error } => json!({"turn": turn, "error": error}),
        TurnEvent::ThreadSettingsApplied { thread } => json!({"thread": thread}),
        TurnEvent::ItemStarted {
            thread_id,
            turn_id,
            item_id,
        } => json!({"thread_id": thread_id, "turn_id": turn_id, "item_id": item_id}),
        TurnEvent::AssistantDelta {
            thread_id,
            turn_id,
            item_id,
            delta,
        } => json!({
            "thread_id": thread_id,
            "turn_id": turn_id,
            "item_id": item_id,
            "delta": delta,
        }),
        TurnEvent::AssistantThinking {
            thread_id,
            turn_id,
            text,
        } => json!({"thread_id": thread_id, "turn_id": turn_id, "text": text}),
        TurnEvent::ItemCompleted {
            thread_id,
            turn_id,
            item_id,
        } => json!({"thread_id": thread_id, "turn_id": turn_id, "item_id": item_id}),
        TurnEvent::ItemFailed {
            thread_id,
            turn_id,
            item_id,
            error,
        } => json!({
            "thread_id": thread_id,
            "turn_id": turn_id,
            "item_id": item_id,
            "error": error,
        }),
        TurnEvent::ToolExecutionStart {
            thread_id,
            turn_id,
            tool_call_id,
            tool_name,
            args,
        } => json!({
            "thread_id": thread_id,
            "turn_id": turn_id,
            "tool_call_id": tool_call_id,
            "tool_name": tool_name,
            "args": args,
        }),
        TurnEvent::ToolExecutionUpdate {
            thread_id,
            turn_id,
            tool_call_id,
            tool_name,
            args,
            partial_result,
        } => json!({
            "thread_id": thread_id,
            "turn_id": turn_id,
            "tool_call_id": tool_call_id,
            "tool_name": tool_name,
            "args": args,
            "partial_result": partial_result,
        }),
        TurnEvent::ToolExecutionEnd {
            thread_id,
            turn_id,
            tool_call_id,
            tool_name,
            result,
            is_error,
        } => json!({
            "thread_id": thread_id,
            "turn_id": turn_id,
            "tool_call_id": tool_call_id,
            "tool_name": tool_name,
            "result": result,
            "is_error": is_error,
        }),
        TurnEvent::Diagnostic {
            thread_id,
            turn_id,
            severity,
            code,
            message,
        } => json!({
            "thread_id": thread_id,
            "turn_id": turn_id,
            "severity": severity.as_str(),
            "code": code,
            "message": message,
        }),
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
        } => {
            let mut object = serde_json::Map::new();
            object.insert("thread_id".to_string(), json!(thread_id));
            object.insert("turn_id".to_string(), json!(turn_id));
            object.insert("model_turn_ordinal".to_string(), json!(model_turn_ordinal));
            object.insert("provider".to_string(), json!(provider));
            object.insert("model".to_string(), json!(model));
            object.insert("protocol".to_string(), json!(protocol));
            object.insert("status".to_string(), json!(status.as_str()));
            // --json 面的可选字段仅在已知时出现（省略即未知，不写 null）。
            if let Some(duration) = attempt_duration_ms {
                object.insert("attempt_duration_ms".to_string(), json!(duration));
            }
            if let Some(category) = error_category {
                object.insert("error_category".to_string(), json!(category));
            }
            if let Some(code) = diagnostic_code {
                object.insert("diagnostic_code".to_string(), json!(code));
            }
            Value::Object(object)
        }
    }
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
