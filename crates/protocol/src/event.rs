//! 执行事件的唯一事实源，以及它到 wire 格式的投影。
//!
//! TurnEvent 用 serde 的 method/params 标签序列化：事件声明本身就同时定义了方法名
//! 和载荷。TurnEventEnvelope 只额外补上工作台的数据版本与时间，由序列化 fixture 验证。
//!
//! Agent 内部的诊断 code 由 agent 的事件模块定义；runtime 的诊断 code 由
//! diagnostic_code 定义。

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::params::Turn;

/// agent/diagnostic 事件携带的稳定诊断代码词表。
pub mod diagnostic_code {
    pub const PROJECT_INSTRUCTIONS_TRUNCATED: &str = "project_instructions_truncated";
    /// 存储故障导致可信终态无法落盘（执行期写入失败，或终态提交失败）。
    pub const STORAGE_FATAL: &str = "storage_fatal";
    /// 程序故障（panic）终止了执行链：这不是能交给模型继续处理的业务失败。
    pub const HOST_FATAL: &str = "host_fatal";
}

/// 无字段枚举在 wire 上的词形只有这一处来源：serde 的 rename_all =
/// "snake_case" 投影。Display 用它把同一个词形呈现到给人读的错误和诊断文本里，
/// 不存在第二份手写的词形表。
#[allow(clippy::expect_used)]
pub fn wire_word<T: Serialize + std::fmt::Debug>(value: T) -> String {
    serde_json::to_value(value)
        .expect("fieldless enum serializes")
        .as_str()
        .expect("fieldless enum serializes to a string")
        .to_string()
}

/// 终态失败的分类信息；message 描述这次失败本身，认证材料不会进入错误文本。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
pub struct TurnErrorDetail {
    pub cause: TurnFailureCause,
    pub message: String,
}

impl std::fmt::Display for TurnErrorDetail {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{} ({})", self.message, self.cause)
    }
}

/// 事件里被指认的 item：在 wire 上嵌套成 item: {"itemId": …}。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct ItemRef {
    pub item_id: String,
}

/// 执行事件在 wire 上的名称与载荷；所有载荷字段统一用 camelCase。
#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(tag = "method", content = "params", rename_all_fields = "camelCase")]
pub enum TurnEvent {
    /// turn 的用户输入由 turn/userMessage 携带：turn/started 只负责 turn 身份
    /// 和开始时刻，不重复消息正文。
    #[serde(rename = "turn/started")]
    TurnStarted { turn: Turn, started_at: String },
    /// 已经落盘的用户消息事实：初始输入和注入输入共用这同一条出口。
    /// item 是该条目首个文本块的公开内容块身份，与历史投影用的是同一套派生规则，
    /// 客户端不必自己拼接 id。
    #[serde(rename = "turn/userMessage")]
    UserMessage {
        thread_id: String,
        turn_id: String,
        item: ItemRef,
        text: String,
    },
    #[serde(rename = "item/started")]
    ItemStarted {
        thread_id: String,
        turn_id: String,
        item: ItemRef,
    },
    #[serde(rename = "item/agentMessage/delta")]
    AssistantDelta {
        thread_id: String,
        turn_id: String,
        item: ItemRef,
        delta: String,
    },
    /// assistant 消息里思考块的事实；落盘之后按块实时发布。
    /// 这是当前思考块的公开文本增量，与终态的思考块使用同一个 item 身份。
    #[serde(rename = "item/agentThinking/delta")]
    AssistantThinkingDelta {
        thread_id: String,
        turn_id: String,
        item: ItemRef,
        delta: String,
    },
    /// 工具事实的静态定义：名称和参数只在 Start 发布一次，之后都按同一个 item
    /// 身份更新。事件本身不会重复携带工具定义。
    #[serde(rename = "tool/execution/start")]
    ToolExecutionStart {
        thread_id: String,
        turn_id: String,
        /// 与历史共享的公开 occurrence 身份，不是 provider 在 wire 上的调用 ID。
        item: ItemRef,
        tool_name: String,
        args: Value,
        started_at: String,
    },
    /// 累计的有界进度文本；是整体替换而不是追加，所以恢复快照和它的实时投影一致。
    #[serde(rename = "tool/execution/update")]
    ToolExecutionUpdate {
        thread_id: String,
        turn_id: String,
        item: ItemRef,
        partial_result: String,
    },
    /// 最终的工具结果：模型可见文本、失败标志和文件变更各自保持原字段。
    #[serde(rename = "tool/execution/end")]
    ToolExecutionEnd {
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
        /// read 工具真实读到的来源范围；其他工具和旧记录没有这个字段。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "typescript", ts(optional))]
        read_source: Option<crate::ReadSource>,
    },
    /// 重试之前移除临时的输出条目。
    #[serde(rename = "item/discarded")]
    ItemDiscarded {
        thread_id: String,
        turn_id: String,
        item: ItemRef,
    },
    #[serde(rename = "item/completed")]
    ItemCompleted {
        thread_id: String,
        turn_id: String,
        item: ItemRef,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "typescript", ts(optional))]
        content: Option<crate::HistoryItem>,
    },
    #[serde(rename = "item/failed")]
    ItemFailed {
        thread_id: String,
        turn_id: String,
        item: ItemRef,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "typescript", ts(optional))]
        content: Option<crate::HistoryItem>,
        error: String,
    },
    #[serde(rename = "agent/diagnostic")]
    Diagnostic {
        thread_id: String,
        turn_id: String,
        severity: DiagnosticSeverity,
        code: String,
        message: String,
    },
    /// 实时 attempt 事件与持久历史共享同一个 RequestObservation；事件自身只
    /// 补充 turn 身份、实际使用的 wire 协议和重试诊断。诊断码只由
    /// observation.diagnostic_code 承载，不在事件外层再重复一份。
    #[serde(rename = "provider/attempt")]
    ProviderAttempt {
        observation: crate::RequestObservation,
        thread_id: String,
        turn_id: String,
        protocol: String,
        retry_after_ms: Option<u64>,
    },
    #[serde(rename = "turn/completed")]
    TurnCompleted { turn: Turn },
    /// 进程内的控制处置变化通知（接受、撤回、编辑、消耗、归还）。控制队列和
    /// 处置只存在于内存，不会因此变成持久账本里的条目。工作台把它归约进会话
    /// 快照再发布，客户端从快照里读取当前处置。
    #[serde(rename = "turn/controlChanged")]
    ControlChanged { control: crate::ControlSnapshot },
    #[serde(rename = "turn/error")]
    TurnFailed {
        thread_id: String,
        turn_id: String,
        error: TurnErrorDetail,
    },
}

/// agent/diagnostic 的稳定严重级别词形。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticSeverity {
    Info,
    Warning,
    Error,
}

/// 诊断文本用 Display 呈现与 wire 一致的词形，不另写第二份词表。
impl std::fmt::Display for DiagnosticSeverity {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&wire_word(*self))
    }
}

/// provider/attempt 的稳定进度与终态词形。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum ProviderAttemptStatus {
    Started,
    Ok,
    Error,
    Cancelled,
}

/// turn/error.error.cause 的稳定失败来源词形。
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

/// 错误文本和 golden 词表测试都通过 Display 呈现 wire 词形。
impl std::fmt::Display for TurnFailureCause {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&wire_word(*self))
    }
}
