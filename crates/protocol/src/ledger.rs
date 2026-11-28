use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestPurpose {
    Generation,
    Compaction,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    /// None 为未上报；Some(0) 为明确上报零缓存用量。
    pub cached_input_tokens: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderAttemptStatus {
    Ok,
    Error,
    Cancelled,
}

impl ProviderAttemptStatus {
    pub const fn turn_status(self) -> crate::TurnStatus {
        match self {
            Self::Ok => crate::TurnStatus::Completed,
            Self::Cancelled => crate::TurnStatus::Interrupted,
            Self::Error => crate::TurnStatus::Failed,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RequestObservation {
    pub purpose: RequestPurpose,
    pub provider: String,
    pub model: String,
    pub status: ProviderAttemptStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttft_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decode_ms: Option<u64>,
    pub usage: Option<ModelUsage>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CommandOutputSnapshot {
    pub text: String,
    pub total_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub full_output: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_error: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub incomplete: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BackgroundTask {
    pub task_id: String,
    pub command: String,
    pub description: String,
    pub output: String,
    pub model_output: CommandOutputSnapshot,
    pub state: crate::BackgroundTaskState,
}
