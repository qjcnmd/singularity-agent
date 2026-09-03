//! 执行事件唯一事实源与 wire 投影。
//!
//! [`TurnEvent`] 是 runtime 直接发射、全部客户端共同消费的唯一事件形态，
//! 各变体直接携带 [`params`](crate::params) 的协议对象类型，不存在第二份
//! 同构镜像；wire 形状由本类型上的 serde 属性单点声明（`untagged` 写出变体
//! 内容，变体级 `rename_all` 给出 camelCase 键名，嵌套对象由 payload 结构体
//! 自身表达），方法名由 [`TurnEvent::method`] 单点定义，`--json` 事件行的
//! `{"method", "params"}` envelope 由 [`turn_event_envelope`] 单点拥有。
//!
//! 可选字段在 wire 上恒出现：无值时为 `null`（省略即未知），由 golden 测试
//! 逐字钉住。
//!
//! Agent 内部诊断 code 由 agent 事件模块定义；runtime 诊断 code 由
//! [`diagnostic_code`] 定义。

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::params::Turn;

/// `agent/diagnostic` 事件携带的稳定诊断代码词表。
pub mod diagnostic_code {
    pub const PROJECT_INSTRUCTIONS_TRUNCATED: &str = "project_instructions_truncated";
    pub const STORAGE_FATAL: &str = "storage_fatal";
}

/// 无字段枚举的 wire 词形唯一来源：serde 的 `rename_all = "snake_case"`
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
pub struct TurnErrorDetail {
    pub stage: TurnFailureStage,
    pub cause: TurnFailureCause,
    pub message: String,
}

/// 事件里被指认的 item：wire 上嵌套为 `item: {"itemId": …}`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ItemRef {
    pub item_id: String,
}

/// 一段结果内容。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct ContentText {
    #[serde(rename = "type")]
    kind: &'static str,
    text: String,
}

/// 工具结果载荷：wire 上嵌套为
/// `result: {"content": [{"type":"text","text":…}], "isError": …}`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolResultPayload {
    content: [ContentText; 1],
    pub is_error: bool,
}

impl ToolResultPayload {
    /// 单段文本结果的构造点：工具执行只有文本内容，形状在此唯一处给出。
    pub fn text(text: String, is_error: bool) -> Self {
        Self {
            content: [ContentText { kind: "text", text }],
            is_error,
        }
    }

    /// 返回工具结果文本。
    pub fn text_content(&self) -> &str {
        &self.content[0].text
    }
}

/// 执行事件的唯一类型化出口：wire 形状即本类型的 serde 属性，字段增删自动
/// 出现在 `params` 中，不存在第二处需要同步的逐字段抄写。
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum TurnEvent {
    TurnStarted {
        turn: Turn,
    },
    #[serde(rename_all = "camelCase")]
    ItemStarted {
        thread_id: String,
        turn_id: String,
        item: ItemRef,
    },
    #[serde(rename_all = "camelCase")]
    AssistantDelta {
        thread_id: String,
        turn_id: String,
        item: ItemRef,
        delta: String,
    },
    /// assistant 消息内的思考块事实；持久化后实时逐块发布。
    #[serde(rename_all = "camelCase")]
    AssistantThinking {
        thread_id: String,
        turn_id: String,
        text: String,
    },
    #[serde(rename_all = "camelCase")]
    ToolExecutionStart {
        thread_id: String,
        turn_id: String,
        tool_call_id: String,
        tool_name: String,
        args: Value,
    },
    #[serde(rename_all = "camelCase")]
    ToolExecutionUpdate {
        thread_id: String,
        turn_id: String,
        tool_call_id: String,
        tool_name: String,
        args: Value,
        partial_result: String,
    },
    #[serde(rename_all = "camelCase")]
    ToolExecutionEnd {
        thread_id: String,
        turn_id: String,
        tool_call_id: String,
        tool_name: String,
        result: ToolResultPayload,
    },
    #[serde(rename_all = "camelCase")]
    ItemCompleted {
        thread_id: String,
        turn_id: String,
        item: ItemRef,
    },
    #[serde(rename_all = "camelCase")]
    ItemFailed {
        thread_id: String,
        turn_id: String,
        item: ItemRef,
        error: String,
    },
    #[serde(rename_all = "camelCase")]
    Diagnostic {
        thread_id: String,
        turn_id: String,
        severity: DiagnosticSeverity,
        code: String,
        message: String,
    },
    #[serde(rename_all = "camelCase")]
    ProviderAttempt {
        thread_id: String,
        turn_id: String,
        /// 1-based provider request attempt within the current model step.
        attempt: u32,
        model_turn_ordinal: u32,
        provider: String,
        model: String,
        protocol: String,
        status: ProviderAttemptStatus,
        attempt_duration_ms: Option<u64>,
        error_category: Option<String>,
        diagnostic_code: Option<String>,
        retry_after_ms: Option<u64>,
        retry_after_source: Option<RetryAfterSource>,
    },
    TurnCompleted {
        turn: Turn,
    },
    #[serde(rename_all = "camelCase")]
    TurnFailed {
        thread_id: String,
        turn_id: String,
        error: TurnErrorDetail,
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
        }
    }
}

/// `--json` 事件行的唯一 envelope 投影：`{"method": <稳定方法名>, "params":
/// <typed payload>}`。键名与嵌套形状由本函数与 [`TurnEvent`] 的 serde 属性
/// 单点拥有；客户端只写入 [`serde_json::Value`] 行，不再各自组装 envelope。
pub fn turn_event_envelope(event: &TurnEvent) -> Value {
    json!({"method": event.method(), "params": event})
}

/// `agent/diagnostic` 的稳定严重级别词形（serde snake_case 单源）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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

/// `provider/attempt` 的稳定进度与终态词形（serde snake_case 单源）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderAttemptStatus {
    Started,
    Ok,
    Error,
    Cancelled,
}

/// Provenance of an advertised retry delay.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetryAfterSource {
    ProviderHeader,
}

/// `turn/error.error.stage` 的稳定管线阶段词形（serde snake_case 单源）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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

/// `turn/error.error.cause` 的稳定失败来源词形（serde snake_case 单源）。
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

/// 错误文本与 golden 词表测试经由 Display 呈现 wire 词形。
impl std::fmt::Display for TurnFailureCause {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&wire_word(*self))
    }
}
