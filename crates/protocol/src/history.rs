use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct ReadSource {
    pub start_line: u64,
    pub line_count: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct ToolResult {
    pub output: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "typescript", ts(optional))]
    pub background_task_id: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    #[cfg_attr(
        feature = "typescript",
        ts(as = "Option<Vec<crate::ImageAttachment>>", optional)
    )]
    pub images: Vec<crate::ImageAttachment>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "typescript", ts(optional))]
    pub read_source: Option<ReadSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "typescript", ts(optional))]
    pub file_created: Option<bool>,
    pub is_error: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "typescript", ts(optional))]
    pub duration_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MessageAnnotation {
    pub message_id: String,
    pub quote: String,
    #[serde(default)]
    pub comment: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum HistoryMessageRole {
    User,
    Assistant,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(tag = "type", rename_all = "snake_case", rename_all_fields = "camelCase")]
pub enum HistoryItem {
    Message {
        id: String,
        status: TurnStatus,
        role: HistoryMessageRole,
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "typescript", ts(optional))]
        at: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "typescript", ts(optional))]
        stats: Option<SessionModelUsage>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        #[cfg_attr(
            feature = "typescript",
            ts(as = "Option<Vec<crate::ImageAttachment>>", optional)
        )]
        images: Vec<crate::ImageAttachment>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        #[cfg_attr(feature = "typescript", ts(as = "Option<Vec<String>>", optional))]
        files: Vec<String>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        #[cfg_attr(feature = "typescript", ts(as = "Option<Vec<MessageAnnotation>>", optional))]
        annotations: Vec<MessageAnnotation>,
    },
    Thinking {
        id: String,
        status: TurnStatus,
        text: String,
    },
    Tool {
        id: String,
        status: ToolStatus,
        name: String,
        args: Value,
        result: Option<ToolResult>,
    },
    Compaction {
        id: String,
        summary: String,
    },
    CompactionResult {
        id: String,
        status: TurnStatus,
        message: Option<String>,
    },
}

impl HistoryItem {
    pub fn id(&self) -> &str {
        match self {
            Self::Message { id, .. }
            | Self::Thinking { id, .. }
            | Self::Tool { id, .. }
            | Self::Compaction { id, .. }
            | Self::CompactionResult { id, .. } => id,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct ThreadTurn {
    pub started_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "typescript", ts(optional))]
    pub finished_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "typescript", ts(optional))]
    pub first_token_at: Option<String>,
    pub turn_id: String,
    pub status: TurnStatus,
    pub manually_stopped: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "typescript", ts(optional))]
    pub error: Option<crate::TurnErrorDetail>,
    pub items: Vec<HistoryItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "typescript", ts(optional))]
    pub stats: Option<SessionModelUsage>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct SessionModelUsage {
    pub ttft_ms: u64,
    pub ttft_requests: u64,
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub total_tokens: u64,
    pub decode_tokens: u64,
    pub decode_ms: u64,
    pub cache_usage_complete: bool,
    /// 是否有请求上报了用量；为 false 时 Token 数都是 0，不代表没有消耗。
    pub usage_present: bool,
}

impl SessionModelUsage {
    pub fn has_metrics(&self) -> bool {
        self.ttft_requests > 0 || self.decode_ms > 0 || self.usage_present
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum TurnStatus {
    Running,
    Completed,
    Failed,
    Interrupted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    Pending,
    Running,
    Completed,
    Failed,
    Interrupted,
}
