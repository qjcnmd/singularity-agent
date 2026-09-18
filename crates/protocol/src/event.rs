//! 执行事件唯一事实源与 wire 投影。
//!
//! TurnEvent 以 serde 的 method/params 标签序列化；事件声明同时维护方法名与载荷。
//! WorkbenchTurnEvent 仅补充工作台水位与时间，由序列化 fixture 验证。
//!
//! Agent 内部诊断 code 由 agent 事件模块定义；runtime 诊断 code 由
//! diagnostic_code 定义。

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::params::Turn;

/// agent/diagnostic 事件携带的稳定诊断代码词表。
pub mod diagnostic_code {
    pub const PROJECT_INSTRUCTIONS_TRUNCATED: &str = "project_instructions_truncated";
    /// 存储故障阻止可信终态落盘（执行期写入失败或终态提交失败）。
    pub const STORAGE_FATAL: &str = "storage_fatal";
    /// 程序故障（panic）终止执行链：不是可交给模型继续处理的业务失败。
    pub const HOST_FATAL: &str = "host_fatal";
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

impl std::fmt::Display for TurnErrorDetail {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "[{}]: {} ({})",
            self.stage, self.message, self.cause
        )
    }
}

/// 事件里被指认的 item：wire 上嵌套为 item: {"itemId": …}。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct ItemRef {
    pub item_id: String,
}

/// wire 名称、载荷类型与观察者方法词表放在一处维护。
macro_rules! turn_events {
    ($($(#[$attr:meta])* $variant:ident => $wire:literal { $($fields:tt)* }),* $(,)?) => {
        #[derive(Debug, Clone, PartialEq, Serialize)]
        #[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
        #[serde(tag = "method", content = "params")]
        pub enum TurnEvent {
            $($(#[$attr])* #[serde(rename = $wire)] $variant { $($fields)* }),*
        }
        impl TurnEvent {
            /// JSON 与进程内观察者共用的稳定方法名。
            pub const fn method(&self) -> &'static str {
                match self { $(Self::$variant { .. } => $wire),* }
            }
        }
    }
}
turn_events! {
    /// turn 的用户输入事实由 turn/userMessage 携带：turn/started 只负责
    /// turn 身份与开始时刻，不重复消息正文。
    #[serde(rename_all = "camelCase")]
    TurnStarted => "turn/started" {
        turn: Turn,
        started_at: String,
    },
    /// 已持久化的用户消息事实：初始输入与注入输入共用同一条出口。
    /// item 是该条目首个文本块的公开内容块身份，与历史投影共用同一派生，
    /// 客户端不再自己拼接 id。
    #[serde(rename_all = "camelCase")]
    UserMessage => "turn/userMessage" {
        thread_id: String,
        turn_id: String,
        item: ItemRef,
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
    /// 当前思考块的公开文本增量，与终态思考块使用相同 item 身份。
    #[serde(rename_all = "camelCase")]
    AssistantThinkingDelta => "item/agentThinking/delta" {
        thread_id: String,
        turn_id: String,
        item: ItemRef,
        delta: String,
    },
    /// 工具事实的静态定义：名称与参数只在 Start 发布一次，后续按同一 item
    /// 身份更新。事件本身不重复携带工具定义。
    #[serde(rename_all = "camelCase")]
    ToolExecutionStart => "tool/execution/start" {
        thread_id: String,
        turn_id: String,
        /// 与历史共享的公开 occurrence 身份，不是 provider 的 wire 调用 ID。
        item: ItemRef,
        tool_name: String,
        args: Value,
        started_at: String,
    },
    /// 累计的有界进度文本；替换而非追加，恢复快照与其实时投影因此一致。
    #[serde(rename_all = "camelCase")]
    ToolExecutionUpdate => "tool/execution/update" {
        thread_id: String,
        turn_id: String,
        item: ItemRef,
        partial_result: String,
    },
    /// 最终工具结果：模型可见文本、失败标志与文件变更各保持原字段。
    #[serde(rename_all = "camelCase")]
    ToolExecutionEnd => "tool/execution/end" {
        thread_id: String,
        turn_id: String,
        item: ItemRef,
        output: String,
        is_error: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "typescript", ts(optional))]
        diff: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "typescript", ts(optional))]
        duration_ms: Option<u64>,
        /// read 的真实来源范围；其它工具与旧记录没有。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "typescript", ts(optional))]
        read_source: Option<crate::ReadSource>,
    },
    #[serde(rename_all = "camelCase")]
    ItemCompleted => "item/completed" {
        thread_id: String,
        turn_id: String,
        item: ItemRef,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "typescript", ts(optional))]
        content: Option<crate::HistoryItem>,
    },
    #[serde(rename_all = "camelCase")]
    ItemFailed => "item/failed" {
        thread_id: String,
        turn_id: String,
        item: ItemRef,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "typescript", ts(optional))]
        content: Option<crate::HistoryItem>,
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
    /// 实时 attempt 事件与持久历史共享同一个 RequestObservation；事件自身只
    /// 补充 turn 身份、实际 wire 协议与重试诊断。诊断码只由
    /// observation.diagnostic_code 承载，不在事件外层重复一份。
    #[serde(rename_all = "camelCase")]
    ProviderAttempt => "provider/attempt" {
        observation: crate::RequestObservation,
        thread_id: String,
        turn_id: String,
        protocol: String,
        retry_after_ms: Option<u64>,
        retry_after_source: Option<RetryAfterSource>,
    },
    TurnCompleted => "turn/completed" {
        turn: Turn,
    },
    /// 进程内的控制处置变化通知（接受、撤回、编辑、消耗、归还）；控制队列与
    /// 处置只存在于内存，不因此成为 durable ledger 条目。工作台把它归约为会话
    /// 快照发布，客户端据快照读取当前处置。
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

/// 对外宣告的重试延迟的来源。
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
    /// 请求与响应预算超出当前模型窗口：本地容量事实，不是提供方错误。
    ContextCapacity,
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
