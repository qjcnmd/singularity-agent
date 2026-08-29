//! 执行事件唯一事实源与两面 wire 投影。
//!
//! [`TurnEvent`] 是 runtime 直接发射、全部客户端共同消费的唯一事件形态，
//! 各变体直接携带 [`params`](crate::params) 的协议对象类型，不存在第二份
//! 同构镜像；方法名由 [`TurnEvent::method`] 单点定义，params 由
//! [`turn_event_params`] 单一投影：
//!
//! - [`turn_event_notification`]：桌面端 JSON-RPC 通知（camelCase wire，
//!   嵌套形状在本文件唯一一处组装）；
//! - `--json` 事件行 = `{"method", "params"}`，与桌面端共用同一 params 投影。
//!
//! `thread/started` 不是执行事件，由 app-server 作为桌面端局部生命周期通知
//! 自行发出。Agent 内部诊断 code 由 agent 事件模块定义；runtime 诊断 code 由
//! [`diagnostic_code`] 定义。

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::envelope::JsonRpcMessage;
use crate::params::{Thread, Turn};

/// `agent/diagnostic` 事件携带的稳定诊断代码词表。
pub mod diagnostic_code {
    pub const PROJECT_INSTRUCTIONS_TRUNCATED: &str = "project_instructions_truncated";
    pub const STEER_UNDELIVERED: &str = "steer_undelivered";
    pub const STORAGE_FATAL: &str = "storage_fatal";
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
        turn: Turn,
    },
    TurnFailed {
        turn: Turn,
        error: TurnErrorDetail,
    },
    ThreadSettingsApplied {
        thread: Thread,
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
    JsonRpcMessage::notification(event.method(), turn_event_params(event))
        .expect("notification with json value params serializes")
}

/// 事件 `params` 的唯一投影（camelCase，item/result 嵌套形态在此唯一一处
/// 定义）。桌面端 JSON-RPC 通知与 `--json` 事件行共用此形状；可选字段恒以
/// null 出现（省略即未知），由 golden 测试逐字钉住。
pub fn turn_event_params(event: &TurnEvent) -> Value {
    match event {
        TurnEvent::TurnStarted { turn } => json!({"turn": turn}),
        TurnEvent::TurnCompleted { turn } => json!({"turn": turn}),
        TurnEvent::TurnFailed { turn, error } => json!({
            "turnId": turn.turn_id,
            "threadId": turn.thread_id,
            "error": {
                "stage": error.stage.as_str(),
                "cause": error.cause.wire_str(),
                "message": error.message,
            },
        }),
        TurnEvent::ThreadSettingsApplied { thread } => json!({"thread": thread}),
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

/// 经 runtime 重导出后被 CLI 诊断行以 Display 使用。
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

/// app-server 错误消息使用 Display 呈现阶段词形。
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
            Self::Internal => "internal",
        }
    }
}

/// app-server 错误消息与 golden 词表测试经由 Display 呈现 wire 词形。
impl std::fmt::Display for TurnFailureCause {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.wire_str())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)] // 测试断言惯例
    use super::*;
    use crate::params::{TurnModelUsage, TurnStatus};

    fn execution_turn(status: TurnStatus, usage: bool) -> Turn {
        Turn {
            turn_id: "turn-1".to_string(),
            thread_id: "thread-1".to_string(),
            status,
            usage: usage.then_some(TurnModelUsage {
                input_tokens: 101,
                output_tokens: 202,
                total_tokens: 303,
                cached_input_tokens: 404,
                reasoning_tokens: 505,
                usage_present: true,
                usage_complete: true,
            }),
        }
    }

    /// 事件两面 golden：每行一个事件（fixture + JSON-RPC 通知全文 +
    /// `--json` params），字节级合同。方法名、键名、嵌套形态、可选字段
    /// 出现/省略的差异都会先在这张表上显形。
    #[test]
    fn turn_event_wire_goldens() {
        let args = json!({"path": "src/main.rs", "old_string": "a"});
        let cases: Vec<(&str, TurnEvent, &str, &str)> = vec![
            (
                "turn/started",
                TurnEvent::TurnStarted {
                    turn: execution_turn(TurnStatus::Running, false),
                },
                r#"{"jsonrpc":"2.0","method":"turn/started","params":{"turn":{"status":"running","threadId":"thread-1","turnId":"turn-1"}}}"#,
                r#"{"turn":{"status":"running","threadId":"thread-1","turnId":"turn-1"}}"#,
            ),
            (
                "item/started",
                TurnEvent::ItemStarted {
                    thread_id: "thread-1".to_string(),
                    turn_id: "turn-1".to_string(),
                    item_id: "item-1".to_string(),
                },
                r#"{"jsonrpc":"2.0","method":"item/started","params":{"item":{"itemId":"item-1"},"threadId":"thread-1","turnId":"turn-1"}}"#,
                r#"{"item":{"itemId":"item-1"},"threadId":"thread-1","turnId":"turn-1"}"#,
            ),
            (
                "item/agentMessage/delta",
                TurnEvent::AssistantDelta {
                    thread_id: "thread-1".to_string(),
                    turn_id: "turn-1".to_string(),
                    item_id: "item-1".to_string(),
                    delta: "hel".to_string(),
                },
                r#"{"jsonrpc":"2.0","method":"item/agentMessage/delta","params":{"delta":"hel","item":{"itemId":"item-1"},"threadId":"thread-1","turnId":"turn-1"}}"#,
                r#"{"delta":"hel","item":{"itemId":"item-1"},"threadId":"thread-1","turnId":"turn-1"}"#,
            ),
            (
                "item/agentThinking",
                TurnEvent::AssistantThinking {
                    thread_id: "thread-1".to_string(),
                    turn_id: "turn-1".to_string(),
                    text: "think".to_string(),
                },
                r#"{"jsonrpc":"2.0","method":"item/agentThinking","params":{"text":"think","threadId":"thread-1","turnId":"turn-1"}}"#,
                r#"{"text":"think","threadId":"thread-1","turnId":"turn-1"}"#,
            ),
            (
                "tool/execution/start",
                TurnEvent::ToolExecutionStart {
                    thread_id: "thread-1".to_string(),
                    turn_id: "turn-1".to_string(),
                    tool_call_id: "call-1".to_string(),
                    tool_name: "edit".to_string(),
                    args: args.clone(),
                },
                r#"{"jsonrpc":"2.0","method":"tool/execution/start","params":{"args":{"old_string":"a","path":"src/main.rs"},"threadId":"thread-1","toolCallId":"call-1","toolName":"edit","turnId":"turn-1"}}"#,
                r#"{"args":{"old_string":"a","path":"src/main.rs"},"threadId":"thread-1","toolCallId":"call-1","toolName":"edit","turnId":"turn-1"}"#,
            ),
            (
                "tool/execution/update",
                TurnEvent::ToolExecutionUpdate {
                    thread_id: "thread-1".to_string(),
                    turn_id: "turn-1".to_string(),
                    tool_call_id: "call-1".to_string(),
                    tool_name: "edit".to_string(),
                    args,
                    partial_result: "chunk".to_string(),
                },
                r#"{"jsonrpc":"2.0","method":"tool/execution/update","params":{"args":{"old_string":"a","path":"src/main.rs"},"partialResult":"chunk","threadId":"thread-1","toolCallId":"call-1","toolName":"edit","turnId":"turn-1"}}"#,
                r#"{"args":{"old_string":"a","path":"src/main.rs"},"partialResult":"chunk","threadId":"thread-1","toolCallId":"call-1","toolName":"edit","turnId":"turn-1"}"#,
            ),
            (
                "tool/execution/end",
                TurnEvent::ToolExecutionEnd {
                    thread_id: "thread-1".to_string(),
                    turn_id: "turn-1".to_string(),
                    tool_call_id: "call-1".to_string(),
                    tool_name: "edit".to_string(),
                    result: "done".to_string(),
                    is_error: false,
                },
                r#"{"jsonrpc":"2.0","method":"tool/execution/end","params":{"result":{"content":[{"text":"done","type":"text"}],"isError":false},"threadId":"thread-1","toolCallId":"call-1","toolName":"edit","turnId":"turn-1"}}"#,
                r#"{"result":{"content":[{"text":"done","type":"text"}],"isError":false},"threadId":"thread-1","toolCallId":"call-1","toolName":"edit","turnId":"turn-1"}"#,
            ),
            (
                "item/completed",
                TurnEvent::ItemCompleted {
                    thread_id: "thread-1".to_string(),
                    turn_id: "turn-1".to_string(),
                    item_id: "item-1".to_string(),
                },
                r#"{"jsonrpc":"2.0","method":"item/completed","params":{"item":{"itemId":"item-1"},"threadId":"thread-1","turnId":"turn-1"}}"#,
                r#"{"item":{"itemId":"item-1"},"threadId":"thread-1","turnId":"turn-1"}"#,
            ),
            (
                "item/failed",
                TurnEvent::ItemFailed {
                    thread_id: "thread-1".to_string(),
                    turn_id: "turn-1".to_string(),
                    item_id: "item-1".to_string(),
                    error: "boom".to_string(),
                },
                r#"{"jsonrpc":"2.0","method":"item/failed","params":{"error":"boom","item":{"itemId":"item-1"},"threadId":"thread-1","turnId":"turn-1"}}"#,
                r#"{"error":"boom","item":{"itemId":"item-1"},"threadId":"thread-1","turnId":"turn-1"}"#,
            ),
            (
                "agent/diagnostic",
                TurnEvent::Diagnostic {
                    thread_id: "thread-1".to_string(),
                    turn_id: "turn-1".to_string(),
                    severity: DiagnosticSeverity::Warning,
                    code: "project_instructions_truncated".to_string(),
                    message: "truncated".to_string(),
                },
                r#"{"jsonrpc":"2.0","method":"agent/diagnostic","params":{"code":"project_instructions_truncated","message":"truncated","severity":"warning","threadId":"thread-1","turnId":"turn-1"}}"#,
                r#"{"code":"project_instructions_truncated","message":"truncated","severity":"warning","threadId":"thread-1","turnId":"turn-1"}"#,
            ),
            (
                "provider/attempt",
                TurnEvent::ProviderAttempt {
                    thread_id: "thread-1".to_string(),
                    turn_id: "turn-1".to_string(),
                    model_turn_ordinal: 3,
                    provider: "opencode-go".to_string(),
                    model: "deepseek-v4-flash".to_string(),
                    protocol: "openai_chat_completions".to_string(),
                    status: ProviderAttemptStatus::Started,
                    attempt_duration_ms: None,
                    error_category: None,
                    diagnostic_code: None,
                },
                r#"{"jsonrpc":"2.0","method":"provider/attempt","params":{"attemptDurationMs":null,"diagnosticCode":null,"errorCategory":null,"model":"deepseek-v4-flash","modelTurnOrdinal":3,"protocol":"openai_chat_completions","provider":"opencode-go","status":"started","threadId":"thread-1","turnId":"turn-1"}}"#,
                r#"{"attemptDurationMs":null,"diagnosticCode":null,"errorCategory":null,"model":"deepseek-v4-flash","modelTurnOrdinal":3,"protocol":"openai_chat_completions","provider":"opencode-go","status":"started","threadId":"thread-1","turnId":"turn-1"}"#,
            ),
            (
                "provider/attempt",
                TurnEvent::ProviderAttempt {
                    thread_id: "thread-1".to_string(),
                    turn_id: "turn-1".to_string(),
                    model_turn_ordinal: 3,
                    provider: "opencode-go".to_string(),
                    model: "deepseek-v4-flash".to_string(),
                    protocol: "openai_responses".to_string(),
                    status: ProviderAttemptStatus::Error,
                    attempt_duration_ms: Some(421),
                    error_category: Some("rate_limited".to_string()),
                    diagnostic_code: Some("provider_retry_scheduled".to_string()),
                },
                r#"{"jsonrpc":"2.0","method":"provider/attempt","params":{"attemptDurationMs":421,"diagnosticCode":"provider_retry_scheduled","errorCategory":"rate_limited","model":"deepseek-v4-flash","modelTurnOrdinal":3,"protocol":"openai_responses","provider":"opencode-go","status":"error","threadId":"thread-1","turnId":"turn-1"}}"#,
                r#"{"attemptDurationMs":421,"diagnosticCode":"provider_retry_scheduled","errorCategory":"rate_limited","model":"deepseek-v4-flash","modelTurnOrdinal":3,"protocol":"openai_responses","provider":"opencode-go","status":"error","threadId":"thread-1","turnId":"turn-1"}"#,
            ),
            (
                "turn/completed",
                TurnEvent::TurnCompleted {
                    turn: execution_turn(TurnStatus::Completed, true),
                },
                r#"{"jsonrpc":"2.0","method":"turn/completed","params":{"turn":{"usage":{"cachedInputTokens":404,"inputTokens":101,"outputTokens":202,"reasoningTokens":505,"totalTokens":303,"usageComplete":true,"usagePresent":true},"status":"completed","threadId":"thread-1","turnId":"turn-1"}}}"#,
                r#"{"turn":{"status":"completed","threadId":"thread-1","turnId":"turn-1","usage":{"cachedInputTokens":404,"inputTokens":101,"outputTokens":202,"reasoningTokens":505,"totalTokens":303,"usageComplete":true,"usagePresent":true}}}"#,
            ),
            (
                "turn/error",
                TurnEvent::TurnFailed {
                    turn: execution_turn(TurnStatus::Failed, false),
                    error: TurnErrorDetail {
                        stage: TurnFailureStage::AgentLoop,
                        cause: TurnFailureCause::ProviderRateLimited,
                        message: "rate limited".to_string(),
                    },
                },
                r#"{"jsonrpc":"2.0","method":"turn/error","params":{"error":{"cause":"provider_rate_limited","message":"rate limited","stage":"agent_loop"},"threadId":"thread-1","turnId":"turn-1"}}"#,
                r#"{"error":{"cause":"provider_rate_limited","message":"rate limited","stage":"agent_loop"},"threadId":"thread-1","turnId":"turn-1"}"#,
            ),
            (
                "thread/settingsApplied",
                TurnEvent::ThreadSettingsApplied {
                    thread: Thread {
                        thread_id: "thread-1".to_string(),
                        cwd: "C:\\work".to_string(),
                        model: Some("opencode-go/deepseek-v4-flash#max".to_string()),
                        last_turn_status: Some(TurnStatus::Completed),
                    },
                },
                r#"{"jsonrpc":"2.0","method":"thread/settingsApplied","params":{"thread":{"cwd":"C:\\work","lastTurnStatus":"completed","model":"opencode-go/deepseek-v4-flash#max","threadId":"thread-1"}}}"#,
                r#"{"thread":{"cwd":"C:\\work","lastTurnStatus":"completed","model":"opencode-go/deepseek-v4-flash#max","threadId":"thread-1"}}"#,
            ),
        ];
        for (method, event, notification, jsonl_params) in cases {
            assert_eq!(event.method(), method, "method drift");
            let expected_notification: Value =
                serde_json::from_str(notification).expect("notification golden parses");
            assert_eq!(
                turn_event_notification(&event).to_wire_value(),
                expected_notification,
                "{method}: json-rpc notification drift"
            );
            let expected_jsonl: Value =
                serde_json::from_str(jsonl_params).expect("jsonl golden parses");
            assert_eq!(
                turn_event_params(&event),
                expected_jsonl,
                "{method}: --json params drift"
            );
        }
    }

    /// `turn/error.error.{stage,cause}` 线格式词形的唯一权威表。runtime 的
    /// provider 分组映射与评估器解析都以此为终点：任一词条改名即协议破坏，
    /// 必须先经 docs §2.1 与客户端合同评审。
    #[test]
    fn failure_taxonomy_wire_words_are_stable() {
        assert_eq!(TurnFailureStage::AgentLoop.as_str(), "agent_loop");
        assert_eq!(
            TurnFailureStage::TerminalOutcome.as_str(),
            "terminal_outcome"
        );
        for (cause, word) in [
            (TurnFailureCause::Store, "store"),
            (
                TurnFailureCause::ProjectInstructions,
                "project_instructions",
            ),
            (TurnFailureCause::Workspace, "workspace"),
            (
                TurnFailureCause::ProviderRateLimited,
                "provider_rate_limited",
            ),
            (TurnFailureCause::ProviderNetwork, "provider_network"),
            (TurnFailureCause::ProviderTimeout, "provider_timeout"),
            (TurnFailureCause::ProviderAuth, "provider_auth"),
            (TurnFailureCause::ProviderValidation, "provider_validation"),
            (TurnFailureCause::ProviderOverloaded, "provider_overloaded"),
            (TurnFailureCause::ProviderCancelled, "provider_cancelled"),
            (
                TurnFailureCause::ProviderContextOverflow,
                "provider_context_overflow",
            ),
            (TurnFailureCause::ProviderUnknown, "provider_unknown"),
            (TurnFailureCause::Internal, "internal"),
        ] {
            assert_eq!(cause.wire_str(), word);
            assert_eq!(cause.to_string(), word, "Display must equal wire_str");
            let round_trip: TurnFailureCause =
                serde_json::from_value(json!(word)).expect("wire word deserializes");
            assert_eq!(round_trip, cause, "serde tag must equal wire_str");
        }
    }
}
