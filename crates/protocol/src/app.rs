use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{ThreadTurn, TurnEvent, TurnStatus};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct Workspace {
    pub root: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct ThreadSummary {
    pub thread_id: String,
    pub cwd: String,
    pub created_at: String,
    pub updated_at: String,
    pub title: Option<String>,
    pub status: Option<TurnStatus>,
    pub manually_stopped: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct ThreadHistory {
    pub summary: ThreadSummary,
    pub turns: Vec<ThreadTurn>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum SessionPhase {
    Idle,
    Running,
    Compacting,
    Stopping,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct TurnEventEnvelope {
    pub turn_id: Option<String>,
    #[serde(flatten)]
    pub event: TurnEvent,
    pub phase: SessionPhase,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum SessionOperation {
    Turn,
    Compaction,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct SessionFailure {
    pub source: SessionOperation,
    pub error: crate::TurnErrorDetail,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct SessionStatus {
    pub phase: SessionPhase,
    pub failure: Option<SessionFailure>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct PendingUserMessage {
    pub item_id: String,
    pub text: String,
    pub images: Vec<crate::ImageAttachment>,
    pub files: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[cfg_attr(
        feature = "typescript",
        ts(as = "Option<Vec<crate::MessageAnnotation>>", optional)
    )]
    pub annotations: Vec<crate::MessageAnnotation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelSelection {
    pub provider: String,
    pub model: String,
    pub reasoning: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct SessionRuntime {
    pub phase: SessionPhase,
    pub selection: ModelSelection,
    pub context_usage: Option<ContextUsage>,
    pub pending_question: Option<crate::PendingQuestion>,
    pub pending_messages: Vec<PendingUserMessage>,
    pub failure: Option<SessionFailure>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct ContextUsage {
    pub used: u64,
    pub capacity: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct ModelConfigurationInput {
    pub model_id: String,
    pub max_context_tokens: Option<u32>,
    pub max_output_tokens: Option<u32>,
    pub reasoning_variants: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct RedactedProvider {
    pub api_protocol: ProviderApiProtocol,
    pub provider_id: String,
    pub display_name: Option<String>,
    pub base_url: String,
    pub credential_configured: bool,
    pub models: Vec<ModelConfigurationInput>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct RedactedWebSearchConfig {
    pub base_url: String,
    pub credential_configured: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct RedactedModelCatalog {
    pub error: Option<String>,
    pub default_selection: Option<ModelSelection>,
    pub providers: Vec<RedactedProvider>,
    pub web_search: RedactedWebSearchConfig,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum ProviderApiProtocol {
    Messages,
    Responses,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct ProviderConfigurationInput {
    pub api_protocol: ProviderApiProtocol,
    pub provider_id: String,
    pub display_name: Option<String>,
    pub base_url: String,
    pub models: Vec<ModelConfigurationInput>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct AppBootstrap {
    pub user_home: Option<String>,
    pub session_states: BTreeMap<String, SessionStatus>,
    pub revision: u64,
    pub workspaces: Vec<Workspace>,
    pub sessions_by_workspace: BTreeMap<String, Vec<ThreadSummary>>,
    pub model_catalog: RedactedModelCatalog,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct SessionReadResult {
    pub revision: u64,
    pub history: ThreadHistory,
    pub runtime: SessionRuntime,
    pub background_tasks: Vec<crate::VisibleBackgroundTask>,
    pub context: Option<Vec<crate::ContextModule>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct RpcError {
    pub message: String,
}

impl RpcError {
    pub fn new(message: impl std::fmt::Display) -> Self {
        Self { message: message.to_string() }
    }
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for RpcError {}

#[derive(Debug, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[cfg_attr(feature = "typescript", ts(concrete(T = Value)))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RpcResponse<T = Value> {
    Success { result: T },
    Error { error: RpcError },
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(tag = "type", rename_all = "snake_case", rename_all_fields = "camelCase")]
pub enum StreamEvent {
    Ready,
    AppChanged {
        payload: AppBootstrap,
    },
    SessionChanged {
        session_id: String,
        payload: SessionRuntime,
    },
    TurnEvent {
        session_id: String,
        payload: Box<TurnEventEnvelope>,
    },
    SessionSettled {
        session_id: String,
        payload: SessionRuntime,
        summary: ThreadSummary,
    },
    SessionSummaryChanged {
        summary: ThreadSummary,
    },
    BackgroundTaskUpdated {
        session_id: String,
        payload: Box<crate::BackgroundTaskUpdate>,
    },
    AppReadFailed {
        error: RpcError,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct StreamEnvelope {
    pub revision: u64,
    #[serde(flatten)]
    pub event: StreamEvent,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct SessionStartResult {
    pub summary: ThreadSummary,
    pub status: SessionStatus,
    pub revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct ModelCatalogResult {
    pub catalog: crate::RedactedModelCatalog,
    pub revision: u64,
}
