use serde::{Deserialize, Serialize};

use crate::params::Turn;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct TurnErrorDetail {
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "typescript", ts(optional))]
    pub item_id: Option<String>,
}

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
    #[serde(rename = "turn/completed")]
    TurnCompleted {
        turn: Turn,
        manually_stopped: bool,
        finished_at: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "typescript", ts(optional))]
        stats: Option<crate::SessionModelUsage>,
    },
    #[serde(rename = "turn/error")]
    TurnFailed {
        error: TurnErrorDetail,
        finished_at: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "typescript", ts(optional))]
        stats: Option<crate::SessionModelUsage>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderAttemptStatus {
    Ok,
    Error,
    Cancelled,
}
