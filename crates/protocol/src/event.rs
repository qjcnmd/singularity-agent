//! 执行事件的定义，以及它到 wire 格式的投影。
//! TurnEvent 用 serde 的 method/params 标签序列化，方法名和载荷都由事件声明定义，
//! TurnEventEnvelope 只补工作台的数据版本。

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::params::Turn;

/// 无字段枚举的 serde wire 词形；Display 用它让错误和诊断文本与 wire 一致。
pub fn wire_word<T: Serialize>(value: T) -> String {
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
    /// 已展示这次失败的条目；回合结束时只关联它，不再重复显示原因。
    #[serde(rename = "itemId", default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "typescript", ts(optional))]
    pub item_id: Option<String>,
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
    /// 会话中显示实际注入的内容及后续变化。
    #[serde(rename = "context/injected")]
    ContextInjected {
        turn_id: Option<String>,
        content: crate::HistoryItem,
    },
    /// turn 的用户输入由 turn/userMessage 携带：turn/started 只负责 turn 身份
    /// 和开始时刻，不重复消息正文。
    #[serde(rename = "turn/started")]
    TurnStarted { turn: Turn, started_at: String },
    /// 已经落盘的用户消息事实：初始输入和注入输入共用这同一条出口。
    /// item 是该条目首个文本块的公开内容块 id，与历史投影的派生规则相同。
    #[serde(rename = "turn/userMessage")]
    UserMessage {
        turn_id: String,
        item: ItemRef,
        text: String,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        #[cfg_attr(
            feature = "typescript",
            ts(as = "Option<Vec<crate::ImageAttachment>>", optional)
        )]
        images: Vec<crate::ImageAttachment>,
    },
    #[serde(rename = "item/agentMessage/delta")]
    AssistantDelta {
        turn_id: String,
        item: ItemRef,
        delta: String,
    },
    /// assistant 消息里思考块的事实，落盘后按块实时发布。这是当前思考块的公开文本
    /// 增量，与终态的思考块使用同一个 item 身份。
    #[serde(rename = "item/agentThinking/delta")]
    AssistantThinkingDelta {
        turn_id: String,
        item: ItemRef,
        delta: String,
    },
    /// 工具名称和参数只在 Start 事件发布一次，之后按同一个 item 身份更新。
    #[serde(rename = "tool/execution/start")]
    ToolExecutionStart {
        turn_id: String,
        /// 与历史共享的公开 occurrence 身份；provider 的 wire 调用 ID 不在协议里。
        item: ItemRef,
        tool_name: String,
        args: Value,
    },
    /// 累计的有界进度文本，每条整体替换；恢复快照与实时投影读到的内容一致。
    #[serde(rename = "tool/execution/update")]
    ToolExecutionUpdate {
        turn_id: String,
        item: ItemRef,
        partial_result: String,
    },
    /// 最终的工具结果：输出、失败标志与观测信息。
    #[serde(rename = "tool/execution/end")]
    ToolExecutionEnd {
        turn_id: String,
        item: ItemRef,
        #[serde(flatten)]
        result: crate::ToolResult,
    },
    /// 重试之前移除临时的输出条目。
    #[serde(rename = "item/discarded")]
    ItemDiscarded { turn_id: String, item: ItemRef },
    #[serde(rename = "item/completed")]
    ItemCompleted {
        turn_id: String,
        item: ItemRef,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "typescript", ts(optional))]
        content: Option<crate::HistoryItem>,
    },
    #[serde(rename = "item/failed")]
    ItemFailed {
        turn_id: String,
        item: ItemRef,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "typescript", ts(optional))]
        content: Option<crate::HistoryItem>,
    },
    /// 实时 attempt 事件与持久历史共享同一个 RequestObservation，事件自身只补 turn 身份。
    #[serde(rename = "provider/attempt")]
    ProviderAttempt {
        observation: crate::RequestObservation,
        /// 独立压缩不属于回合；普通执行使用实际回合身份。
        turn_id: Option<String>,
    },
    /// 同一次压缩从开始到结束共用条目身份；手动和自动使用同一事件。
    #[serde(rename = "compaction/updated")]
    CompactionUpdated {
        turn_id: Option<String>,
        content: crate::HistoryItem,
    },
    /// 已保存的模型设置，直接更新当前显示中的历史条目。
    #[serde(rename = "session/settingsUpdated")]
    SettingsUpdated {
        turn_id: Option<String>,
        content: crate::HistoryItem,
    },
    #[serde(rename = "turn/completed")]
    TurnCompleted {
        turn: Turn,
        manually_stopped: bool,
        finished_at: String,
    },
    /// 提问或压缩阶段变化；宿主截获后重新发布会话快照，不直接发送给客户端。
    #[serde(rename = "turn/sessionStateChanged")]
    #[cfg_attr(feature = "typescript", ts(skip))]
    SessionStateChanged {},
    #[serde(rename = "turn/error")]
    TurnFailed {
        turn_id: String,
        error: TurnErrorDetail,
        finished_at: String,
    },
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

impl std::fmt::Display for TurnFailureCause {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&wire_word(*self))
    }
}
