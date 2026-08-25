#![forbid(unsafe_code)]

//! 在进程边界负责 session/turn 准入、协议对象转换与事件投影的 stdio JSON-RPC
//! 应用服务。
//!
//! 职责边界：Thread/Turn 执行语义（Agent 构造、Provider 准备、会话单写者、
//! 事件顺序、usage 聚合、终态落盘、取消、steer/followUp、设置时序）由
//! [`singularity_runtime`] 唯一拥有；本 crate 只做 JSON-RPC 解析、索引投影、
//! 协议对象转换，并把 [`singularity_runtime::TurnEvent`] 一一映射为协议通知。
//! JSONL rollout 是会话正文的唯一持久事实源；进程内 `SessionIndex` 只缓存
//! 定位与展示元数据并在启动时从 JSONL 重建。

mod delete;
mod dispatch;
mod events;
mod lifecycle;
pub mod paths;
mod session_index;
mod state;

use std::fmt;
use std::path::Path;
use std::sync::Arc;

use serde_json::{Value, json};
use singularity_core::user_singularity_home;
use singularity_model::ModelUsage;
use singularity_protocol::{
    AppEvent, ErrorCode, HistoryItem, InitializeParams, InitializeResult, JsonRpcId,
    JsonRpcMessage, Method, MethodKind, ProviderConfigurationStatus, ServerShutdownResult,
    SessionDeleteResult, SessionIdParams, Thread, ThreadListResult, ThreadReadParams,
    ThreadReadResult, ThreadSettingsParams, ThreadSettingsResult, ThreadStartParams,
    ThreadStartResult, ThreadStatus, ThreadTurn, Turn, TurnIdParams, TurnInjectionParams,
    TurnInjectionResult, TurnInterruptResult, TurnStartParams, TurnStatus,
};
use thiserror::Error;

pub use session_index::{
    SessionIndex, SessionIndexError, SessionMetadataUpdate, SessionRecord, SessionStatus, now_iso,
};

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
    Store(#[from] SessionIndexError),
    #[error("session error: {0}")]
    Session(#[from] singularity_agent::session::SessionError),
    #[error("workspace error: {0}")]
    Workspace(String),
    /// 共享核心的 turn 执行失败：分类来自 runtime 的失败 taxonomy，
    /// original 仅在 RPC 边界用于透出真实原因，不参与持久化分类。
    #[error("turn execution failed during {stage} ({cause})")]
    TurnExecution {
        stage: TurnFailureStage,
        cause: TurnFailureCause,
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
}

impl TurnFailureStage {
    fn as_str(self) -> &'static str {
        match self {
            Self::AgentLoop => "agent_loop",
            Self::TerminalOutcome => "terminal_outcome",
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
            Self::Provider(kind) => match kind {
                ProviderFailureKind::RateLimited => "provider_rate_limited",
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

impl From<singularity_runtime::ProviderFailureKind> for ProviderFailureKind {
    fn from(kind: singularity_runtime::ProviderFailureKind) -> Self {
        use singularity_runtime::ProviderFailureKind as R;
        match kind {
            R::RateLimited => Self::RateLimited,
            R::Network => Self::Network,
            R::Timeout => Self::Timeout,
            R::Auth => Self::Auth,
            R::Validation => Self::Validation,
            R::Overloaded => Self::Overloaded,
            R::Cancelled => Self::Cancelled,
            R::ContextOverflow => Self::ContextOverflow,
            R::Unknown => Self::Unknown,
        }
    }
}

impl From<singularity_runtime::TurnFailureCause> for TurnFailureCause {
    fn from(cause: singularity_runtime::TurnFailureCause) -> Self {
        use singularity_runtime::TurnFailureCause as R;
        match cause {
            R::Store => Self::Store,
            R::Workspace => Self::Workspace,
            R::ProjectInstructions => Self::ProjectInstructions,
            R::Serialization => Self::Serialization,
            R::Provider(kind) => Self::Provider(kind.into()),
            R::Internal => Self::Internal,
        }
    }
}

impl From<singularity_runtime::TurnFailureStage> for TurnFailureStage {
    fn from(stage: singularity_runtime::TurnFailureStage) -> Self {
        match stage {
            singularity_runtime::TurnFailureStage::AgentLoop => Self::AgentLoop,
            singularity_runtime::TurnFailureStage::TerminalOutcome => Self::TerminalOutcome,
        }
    }
}

/// 终态补偿失败的稳定分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnTerminalizationFailure {
    Store,
}

impl fmt::Display for TurnTerminalizationFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Store => "store",
        })
    }
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

/// AppServer 交给 stdout transport 的消息。
pub type AppServerOutput = Value;

pub use dispatch::{TurnClaim, TurnStartClaim};
use events::project_turn_history;
pub use paths::thread_from_record;
pub use state::{AppServer, AppServerCancellationHandle, AppServerControlHandle};

#[cfg(test)]
mod tests;
