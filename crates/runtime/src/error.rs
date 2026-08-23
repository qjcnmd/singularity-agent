//! Turn 失败分类与运行错误。
//!
//! 分类与 app-server 协议层的失败 taxonomy 保持一一对应：stage 描述失败发生
//! 的管线阶段，cause 描述失败来源，original 保留脱敏前的真实原因（对外输出
//! 前必须经过敏感文本边界）。

use singularity_model::ModelErrorKind;
use thiserror::Error;

/// 失败发生的管线阶段。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnFailureStage {
    AgentLoop,
    TerminalOutcome,
    EventNotification,
}

impl TurnFailureStage {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AgentLoop => "agent_loop",
            Self::TerminalOutcome => "terminal_outcome",
            Self::EventNotification => "event_notification",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnFailureCause {
    Store,
    ProjectInstructions,
    Workspace,
    Provider(ProviderFailureKind),
    Serialization,
    Internal,
}

impl TurnFailureCause {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Store => "store",
            Self::ProjectInstructions => "project_instructions",
            Self::Workspace => "workspace",
            Self::Provider(kind) => kind.as_str(),
            Self::Serialization => "serialization",
            Self::Internal => "internal",
        }
    }
}

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

impl ProviderFailureKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RateLimited => "rate_limited",
            Self::QuotaExceeded => "quota_exceeded",
            Self::Network => "network",
            Self::Timeout => "timeout",
            Self::Auth => "auth",
            Self::Validation => "validation",
            Self::Overloaded => "overloaded",
            Self::Cancelled => "cancelled",
            Self::ContextOverflow => "context_overflow",
            Self::Unknown => "unknown",
        }
    }

    pub fn from_model_error_kind(kind: &ModelErrorKind) -> Self {
        use ModelErrorKind::*;
        match kind {
            RateLimited => Self::RateLimited,
            BudgetExceeded => Self::QuotaExceeded,
            NetworkError => Self::Network,
            Timeout => Self::Timeout,
            AuthError => Self::Auth,
            InvalidRequest | ToolCallParseError | JsonSchemaViolation | ContentFilter => {
                Self::Validation
            }
            ProviderOverloaded => Self::Overloaded,
            Cancelled => Self::Cancelled,
            ContextLengthExceeded => Self::ContextOverflow,
            UnknownProviderError | UnsupportedCapability => Self::Unknown,
        }
    }
}

/// 一次可归因的 turn 失败事实。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnFailure {
    pub stage: TurnFailureStage,
    pub cause: TurnFailureCause,
    /// 真实原因文本；对外可见前必须经过敏感文本检查。
    pub original: Option<String>,
}

/// [`crate::TurnRunner::run`] 的三类失败：
/// 准备阶段失败（无 turn 痕迹）、执行阶段失败（终态已收敛）、
/// 终态化失败（terminal metadata 无法落盘的 fatal 存储错误）。
#[derive(Debug, Error)]
pub enum TurnRunError {
    #[error("{message}")]
    Preparation {
        /// 失败来源分类；turn 未留下任何痕迹。
        cause: TurnFailureCause,
        message: String,
    },
    #[error("turn failed: {0:?}")]
    Execution(TurnFailure),
    #[error("terminalization failed: {0:?}")]
    Terminalization(TurnFailure),
}
