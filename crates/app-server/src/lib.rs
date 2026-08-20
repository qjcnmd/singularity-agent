#![forbid(unsafe_code)]

//! 在进程边界负责 session/turn 准入、AgentLoop 执行和取消的 stdio JSON-RPC 应用服务。
//!
//! JSONL rollout 是会话正文的唯一权威；SQLite `session_index` 只保存定位与展示元数据。

mod delete;
mod dispatch;
mod events;
mod lifecycle;
pub mod paths;
mod state;
#[allow(dead_code)]
mod state_paths;

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use singularity_agent::{
    agent::{Agent, AgentConfig, AgentError, AgentEvents, AgentOutcome, SteerHandle},
    session::{
        SessionEntryFilter, SessionError, SessionManager, SessionMetadataKind, SessionReadOptions,
        SessionRepository,
    },
    tools::{ToolExecution, ToolRegistry},
};
use singularity_core::{
    CancellationToken, ErrorCode, ProjectInstructionError, load_project_instructions_from_cwd,
    user_singularity_home,
};
use singularity_model::{DEFAULT_MAX_CONTEXT_TOKENS, ModelUsage, Provider, ProviderConfigSnapshot};
use singularity_protocol::{
    AgentCapabilityResult, AppEvent, EventClass, EventDelivery, EventMetadata, HistoryItem,
    InitializeParams, InitializeResult, JsonRpcId, JsonRpcMessage, Method, MethodKind,
    ProviderConfigurationStatus, ServerShutdownResult, SessionDeleteResult, SessionIdParams,
    SessionReadParams, SessionReadResult, Thread, ThreadIdParams, ThreadListResult, ThreadResult,
    ThreadSettingsParams, ThreadSettingsResult, ThreadStartParams, ThreadStartResult, Turn,
    TurnIdParams, TurnInjectionOutcome, TurnInjectionParams, TurnInjectionResult,
    TurnInterruptResult, TurnStartParams, TurnStartResult, TurnStatus,
};
use singularity_store::{
    SessionMetadataUpdate, SessionRecord, SessionStatus, SessionStore, StoreError,
    ensure_owner_only_file, now_iso,
};
use thiserror::Error;
use uuid::Uuid;

const THREAD_NOT_FOUND: &str = "Thread not found";
const TURN_NOT_FOUND: &str = "Turn not found";
const SESSION_DELETE_TURN_ACTIVE: &str =
    "session/delete rejected: a turn is still active for this session";
const MAX_SESSION_TITLE_CHARS: usize = 120;
const SAFE_WORKSPACE_FAILURE: &str = "workspace capability unavailable";
const APP_ERROR_INVALID_STATE: i64 = -32005;

/// 在应用边界转换为 JSON-RPC 响应的错误。
#[derive(Debug, Error)]
pub enum AppServerError {
    #[error("invalid json: {0}")]
    InvalidJson(#[from] serde_json::Error),
    #[error("invalid params: {0}")]
    InvalidParams(String),
    #[error("store error: {0}")]
    Store(#[from] StoreError),
    #[error("session error: {0}")]
    Session(#[from] SessionError),
    #[error("project instructions error: {0}")]
    ProjectInstructions(#[from] ProjectInstructionError),
    #[error("agent error: {0}")]
    Agent(#[from] AgentError),
    #[error("workspace error: {0}")]
    Workspace(String),
    #[error("turn execution failed during {stage} ({cause})")]
    TurnExecution {
        stage: TurnFailureStage,
        cause: TurnFailureCause,
        /// 原始失败文本；仅在 RPC 边界用于透出真实原因，不参与持久化分类。
        original: Option<String>,
    },
    #[error("turn execution failed during {stage} ({cause}); terminalization failed ({failure})")]
    TurnTerminalization {
        stage: TurnFailureStage,
        cause: TurnFailureCause,
        failure: TurnTerminalizationFailure,
        /// 原始失败文本；仅在 RPC 边界用于透出真实原因，不参与持久化分类。
        original: Option<String>,
    },
}

/// `AppServer` 请求处理和生命周期操作使用的结果类型。
pub type AppServerResult<T> = Result<T, AppServerError>;

/// 已进入 Running Turn 的失败阶段；仅暴露稳定分类，不携带底层错误文本。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnFailureStage {
    AgentLoop,
    TerminalOutcome,
    EventNotification,
}

impl TurnFailureStage {
    fn as_str(self) -> &'static str {
        match self {
            Self::AgentLoop => "agent_loop",
            Self::TerminalOutcome => "terminal_outcome",
            Self::EventNotification => "event_notification",
        }
    }
}

impl fmt::Display for TurnFailureStage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// 模型提供方及调用边界失败的具体分类枚举。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderFailureKind {
    RateLimited,
    QuotaExceeded,
    Network,
    Timeout,
    Auth,
    Validation,
    Overloaded,
    Cancelled,
    ContextOverflow,
    Unknown,
}

/// 已进入 Running Turn 后失败的稳定原始原因分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnFailureCause {
    Store,
    Workspace,
    ProjectInstructions,
    Serialization,
    StoredInputUnavailable,
    /// provider/模型边界失败（限流/配额/网络/校验等，见 `ProviderFailureKind`）。
    Provider(ProviderFailureKind),
    Internal,
}

impl TurnFailureCause {
    fn as_str(self) -> &'static str {
        match self {
            Self::Store => "store",
            Self::Workspace => "workspace",
            Self::ProjectInstructions => "project_instructions",
            Self::Serialization => "serialization",
            Self::StoredInputUnavailable => "stored_input_unavailable",
            Self::Provider(kind) => match kind {
                ProviderFailureKind::RateLimited => "provider_rate_limited",
                ProviderFailureKind::QuotaExceeded => "provider_quota_exceeded",
                ProviderFailureKind::Network => "provider_network",
                ProviderFailureKind::Timeout => "provider_timeout",
                ProviderFailureKind::Auth => "provider_auth",
                ProviderFailureKind::Validation => "provider_validation",
                ProviderFailureKind::Overloaded => "provider_overloaded",
                ProviderFailureKind::Cancelled => "provider_cancelled",
                ProviderFailureKind::ContextOverflow => "provider_context_overflow",
                ProviderFailureKind::Unknown => "provider_unknown",
            },
            Self::Internal => "internal",
        }
    }
}

impl fmt::Display for TurnFailureCause {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// 终态补偿失败的稳定分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnTerminalizationFailure {
    Store,
    StateChanged,
    EventNotification,
}

impl fmt::Display for TurnTerminalizationFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Store => "store",
            Self::StateChanged => "state_changed",
            Self::EventNotification => "event_notification",
        })
    }
}

/// 一次 agent 运行的稳定生命周期状态（app-server 本地枚举）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentStatus {
    Running,
    CancelRequested,
    Completed,
    Cancelled,
    Failed,
}

impl AgentStatus {
    /// 返回稳定的生命周期状态字符串。
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::CancelRequested => "cancel_requested",
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
            Self::Failed => "failed",
        }
    }
}

/// agent 运行终态（app-server 内部类型）。
#[derive(Debug, Clone, PartialEq)]
pub struct RunStatus {
    pub status: AgentStatus,
    pub final_answer: Option<String>,
    pub model_turns: u32,
    pub model_usage: ModelUsage,
    pub usage_complete: bool,
    pub error: Option<String>,
}

impl RunStatus {
    pub fn failed(message: impl Into<String>) -> Self {
        Self {
            status: AgentStatus::Failed,
            final_answer: None,
            model_turns: 0,
            model_usage: ModelUsage::default(),
            usage_complete: false,
            error: Some(message.into()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TurnFailure {
    stage: TurnFailureStage,
    cause: TurnFailureCause,
    /// 携带到 RPC 边界的原始失败文本；无原文时为 `None`。
    original: Option<String>,
}

/// 将 provider 层聚合 usage 投影为协议线格式。
pub fn usage_to_wire(usage: &ModelUsage) -> singularity_protocol::TurnModelUsage {
    usage_to_wire_with_completeness(usage, true)
}

/// 将 provider 聚合 usage 与其完整性投影为协议线格式。
pub fn usage_to_wire_with_completeness(
    usage: &ModelUsage,
    usage_complete: bool,
) -> singularity_protocol::TurnModelUsage {
    singularity_protocol::TurnModelUsage {
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        total_tokens: usage.total_tokens,
        cached_input_tokens: usage.cached_input_tokens,
        reasoning_tokens: usage.reasoning_tokens,
        usage_present: usage.usage_present,
        usage_complete,
    }
}

impl From<TurnFailureStage> for TurnFailure {
    fn from(stage: TurnFailureStage) -> Self {
        Self {
            stage,
            cause: TurnFailureCause::Internal,
            original: None,
        }
    }
}

/// AppServer 交给 stdout transport 的消息。
pub type AppServerOutput = Value;

use events::AssistantItemEventState;
use events::project_public_history;
pub use paths::rebuild_session_index_from_jsonl;
pub use paths::thread_from_record;
use paths::{canonical_thread_cwd, refresh_session_index_from_open_session, workspace_path};
pub use state::{AppServer, AppServerCancellationHandle, AppServerControlHandle};

#[cfg(test)]
mod tests;
