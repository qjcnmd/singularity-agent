//! 执行事件的定义。TurnEvent 序列化为 `{ method, params }`，方法名写在各个变体上；
//! 外层的 TurnEventEnvelope 补充回合 ID 和阶段。

use serde::{Deserialize, Serialize};

use crate::params::Turn;

/// 失败的分类和原因；message 不含密钥等认证信息。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
pub struct TurnErrorDetail {
    pub cause: TurnFailureCause,
    pub message: String,
    /// 已经显示了这次失败的条目 ID（如压缩条目），界面据此避免重复报错。
    #[serde(rename = "itemId", default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "typescript", ts(optional))]
    pub item_id: Option<String>,
}

impl std::fmt::Display for TurnErrorDetail {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{} ({})", self.message, self.cause)
    }
}

/// Agent 发出、工作台接收的执行事件；回合 ID 在外层 TurnEventEnvelope 上。
#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(tag = "method", content = "params", rename_all_fields = "camelCase")]
pub enum TurnEvent {
    #[serde(rename = "context/loaded")]
    ContextLoaded { context: crate::InjectedContext },
    #[serde(rename = "turn/started")]
    TurnStarted { turn: Turn, started_at: String },
    #[serde(rename = "item/updated")]
    ItemUpdated { item: crate::HistoryItem },
    #[serde(rename = "item/textDelta")]
    TextDelta {
        item_id: String,
        thinking: bool,
        delta: String,
    },
    #[serde(rename = "message/finished")]
    MessageFinished {
        message_id: String,
        items: Vec<crate::HistoryItem>,
    },
    #[serde(rename = "message/discarded")]
    MessageDiscarded { message_id: String },
    #[serde(rename = "tool/output")]
    ToolOutput { item_id: String, output: String },
    #[serde(rename = "tool/finished")]
    ToolFinished {
        item_id: String,
        result: crate::ToolResult,
    },
    #[cfg_attr(feature = "typescript", ts(skip))]
    ProviderAttempt { observation: crate::RequestObservation },
    #[serde(rename = "turn/completed")]
    TurnCompleted {
        turn: Turn,
        manually_stopped: bool,
        finished_at: String,
    },
    /// 提问或压缩状态变化，通知外层推送最新的 `SessionRuntime`。
    #[cfg_attr(feature = "typescript", ts(skip))]
    SessionStateChanged {},
    #[serde(rename = "turn/error")]
    TurnFailed {
        error: TurnErrorDetail,
        finished_at: String,
    },
}

/// 模型请求的实际结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum ProviderAttemptStatus {
    Ok,
    Error,
    Cancelled,
}

/// 失败来源：写日志失败、提供方错误或内部错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum TurnFailureCause {
    Store,
    Provider,
    Internal,
}

impl std::fmt::Display for TurnFailureCause {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Store => "store",
            Self::Provider => "provider",
            Self::Internal => "internal",
        })
    }
}
