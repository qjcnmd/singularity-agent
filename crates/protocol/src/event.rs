//! 执行事件唯一事实源与 wire 投影。
//!
//! TurnEvent 以 serde 的 method/params 标签序列化；事件声明同时维护方法名与载荷。
//! WorkbenchTurnEvent 仅补充工作台水位与时间，由序列化 fixture 验证。
//!
//! Agent 内部诊断 code 由 agent 事件模块定义；runtime 诊断 code 由
//! diagnostic_code 定义。

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::params::Turn;

/// agent/diagnostic 事件携带的稳定诊断代码词表。
pub mod diagnostic_code {
    pub const PROJECT_INSTRUCTIONS_TRUNCATED: &str = "project_instructions_truncated";
    pub const STORAGE_FATAL: &str = "storage_fatal";
}

/// 无字段枚举的 wire 词形唯一来源：serde 的 rename_all = "snake_case"
/// 投影。Display 用它把同一词形呈现给人读的错误与诊断文本，词形不存在
/// 第二份手写表。
// 不变量：无字段枚举的 serde 投影恒为字符串。
#[allow(clippy::expect_used)]
pub fn wire_word<T: Serialize + std::fmt::Debug>(value: T) -> String {
    serde_json::to_value(value)
        .expect("fieldless enum serializes")
        .as_str()
        .expect("fieldless enum serializes to a string")
        .to_string()
}

/// 终态失败的分类信息；message 是失败本身的当前描述，认证材料不进入错误文本。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
pub struct TurnErrorDetail {
    pub stage: TurnFailureStage,
    pub cause: TurnFailureCause,
    pub message: String,
}

/// 事件里被指认的 item：wire 上嵌套为 item: {"itemId": …}。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct ItemRef {
    pub item_id: String,
}

/// 一段结果内容。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
struct ContentText {
    #[serde(rename = "type")]
    #[cfg_attr(feature = "typescript", ts(type = "\"text\""))]
    kind: &'static str,
    text: String,
}

/// 工具结果载荷：wire 上嵌套为
/// result: {"content": [{"type":"text","text":…}], "isError": …, "diff"?: …}。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct ToolResultPayload {
    content: [ContentText; 1],
    pub is_error: bool,
    /// 文件变更独立于模型可见文本，供客户端直接解析和展示。
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "typescript", ts(optional))]
    #[cfg_attr(feature = "typescript", ts(optional))]
    pub diff: Option<String>,
}

impl ToolResultPayload {
    /// 工具结果的公开投影；模型文本与文件变更各自保持结构。
    pub fn new(text: String, is_error: bool, diff: Option<String>) -> Self {
        Self {
            content: [ContentText { kind: "text", text }],
            is_error,
            diff,
        }
    }
}

/// Keep wire names, payload types and the observer method vocabulary together.
macro_rules! turn_events {
    ($($(#[$attr:meta])* $variant:ident => $wire:literal { $($fields:tt)* }),* $(,)?) => {
        #[derive(Debug, Clone, PartialEq, Serialize)]
        #[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
        #[serde(tag = "method", content = "params")]
        pub enum TurnEvent {
            $($(#[$attr])* #[serde(rename = $wire)] $variant { $($fields)* }),*
        }
        impl TurnEvent {
            /// Stable method name shared by JSON and in-process observers.
            pub const fn method(&self) -> &'static str {
                match self { $(Self::$variant { .. } => $wire),* }
            }
        }
    }
}
turn_events! {
    /// turn 的用户输入事实由 turn/userMessage 携带：turn/started 只负责
    /// turn 身份与开始时刻，不重复消息正文。
    TurnStarted => "turn/started" {
        turn: Turn,
    },
    /// 已持久化的用户消息事实：初始输入与注入输入共用同一条出口，
    /// entryId 与持久历史中的消息条目身份一致。
    #[serde(rename_all = "camelCase")]
    UserMessage => "turn/userMessage" {
        thread_id: String,
        turn_id: String,
        entry_id: String,
        text: String,
    },
    #[serde(rename_all = "camelCase")]
    ItemStarted => "item/started" {
        thread_id: String,
        turn_id: String,
        item: ItemRef,
    },
    #[serde(rename_all = "camelCase")]
    AssistantDelta => "item/agentMessage/delta" {
        thread_id: String,
        turn_id: String,
        item: ItemRef,
        delta: String,
    },
    /// assistant 消息内的思考块事实；持久化后实时逐块发布。
    #[serde(rename_all = "camelCase")]
    AssistantThinking => "item/agentThinking" {
        thread_id: String,
        turn_id: String,
        item: ItemRef,
        text: String,
    },
    /// 当前思考块的公开文本增量，与终态思考块使用相同 item 身份。
    #[serde(rename_all = "camelCase")]
    AssistantThinkingDelta => "item/agentThinking/delta" {
        thread_id: String,
        turn_id: String,
        item: ItemRef,
        delta: String,
    },
    #[serde(rename_all = "camelCase")]
    ToolExecutionStart => "tool/execution/start" {
        thread_id: String,
        turn_id: String,
        /// Public occurrence ID shared with history, distinct from the provider's wire ID.
        tool_call_id: String,
        tool_name: String,
        args: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "typescript", ts(optional))]
        started_at: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    ToolExecutionUpdate => "tool/execution/update" {
        thread_id: String,
        turn_id: String,
        tool_call_id: String,
        tool_name: String,
        args: Value,
        partial_result: String,
    },
    #[serde(rename_all = "camelCase")]
    ToolExecutionEnd => "tool/execution/end" {
        thread_id: String,
        turn_id: String,
        tool_call_id: String,
        tool_name: String,
        result: ToolResultPayload,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "typescript", ts(optional))]
        duration_ms: Option<u64>,
    },
    #[serde(rename_all = "camelCase")]
    ItemCompleted => "item/completed" {
        thread_id: String,
        turn_id: String,
        item: ItemRef,
    },
    #[serde(rename_all = "camelCase")]
    ItemFailed => "item/failed" {
        thread_id: String,
        turn_id: String,
        item: ItemRef,
        error: String,
    },
    #[serde(rename_all = "camelCase")]
    Diagnostic => "agent/diagnostic" {
        thread_id: String,
        turn_id: String,
        severity: DiagnosticSeverity,
        code: String,
        message: String,
    },
    #[serde(rename_all = "camelCase")]
    ProviderAttempt => "provider/attempt" {
        #[serde(default)]
        request_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "typescript", ts(optional))]
        request_head: Option<Box<crate::ModelRequestSnapshot>>,
        #[serde(default)]
        purpose: crate::RequestPurpose,
        thread_id: String,
        turn_id: String,
        /// 1-based provider request sequence within the current turn.
        attempt: u32,
        model_turn_ordinal: u32,
        provider: String,
        model: String,
        protocol: String,
        status: ProviderAttemptStatus,
        attempt_duration_ms: Option<u64>,
        /// Measured usage from this attempt; absent when the provider did not report it.
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
        cached_input_tokens: Option<u64>,
        error_category: Option<String>,
        diagnostic_code: Option<String>,
        retry_after_ms: Option<u64>,
        retry_after_source: Option<RetryAfterSource>,
    },
    TurnCompleted => "turn/completed" {
        turn: Turn,
    },
    /// 已落盘的控制处置变化；工作台把它归约为会话快照发布，控制队列与
    /// 投递状态只由会话快照这一种表示承载。
    #[serde(rename_all = "camelCase")]
    ControlChanged => "turn/controlChanged" {
        control: crate::ControlSnapshot,
    },
    #[serde(rename_all = "camelCase")]
    TurnFailed => "turn/error" {
        thread_id: String,
        turn_id: String,
        error: TurnErrorDetail,
    },
}

/// The JSON event envelope is the event's own tagged serialization.
pub fn turn_event_envelope(event: &TurnEvent) -> Value {
    json!(event)
}

/// agent/diagnostic 的稳定严重级别词形（serde snake_case 单源）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticSeverity {
    Info,
    Warning,
    Error,
}

/// 经 runtime 重导出后被 CLI 诊断行以 Display 使用。
impl std::fmt::Display for DiagnosticSeverity {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&wire_word(*self))
    }
}

/// provider/attempt 的稳定进度与终态词形（serde snake_case 单源）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum ProviderAttemptStatus {
    Started,
    Ok,
    Error,
    Cancelled,
}

/// Provenance of an advertised retry delay.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum RetryAfterSource {
    ProviderHeader,
}

/// turn/error.error.stage 的稳定管线阶段词形（serde snake_case 单源）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum TurnFailureStage {
    AgentLoop,
    TerminalOutcome,
}

/// 错误文本以 Display 呈现阶段词形。
impl std::fmt::Display for TurnFailureStage {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&wire_word(*self))
    }
}

/// turn/error.error.cause 的稳定失败来源词形（serde snake_case 单源）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
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

/// 错误文本与 golden 词表测试经由 Display 呈现 wire 词形。
impl std::fmt::Display for TurnFailureCause {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&wire_word(*self))
    }
}
