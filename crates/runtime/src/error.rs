//! Turn 失败分类与运行错误。
//!
//! 分类与 app-server 协议层的失败 taxonomy 保持一一对应：stage 描述失败发生
//! 的管线阶段，cause 描述失败来源，original 保留脱敏前的真实原因（对外输出
//! 前必须经过敏感文本边界）。

use singularity_model::ModelErrorKind;
pub use singularity_protocol::{TurnFailureCause, TurnFailureStage};
use thiserror::Error;

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

impl ProviderFailureKind {
    /// 基础词形（展示/内部文本）：由 [`Self::wire_str`] 派生，去掉 `provider_`
    /// 前缀即得，避免第二份手写词形表。
    pub fn as_str(self) -> &'static str {
        self.wire_str()
            .strip_prefix("provider_")
            .unwrap_or_else(|| self.wire_str())
    }

    /// 协议线格式的稳定 cause 词形（`provider_` 前缀）——app-server 与
    /// JSON-RPC 边界共用这一个定义，杜绝跨 crate 词表漂移。
    pub const fn wire_str(self) -> &'static str {
        match self {
            Self::RateLimited => "provider_rate_limited",
            Self::Network => "provider_network",
            Self::Timeout => "provider_timeout",
            Self::Auth => "provider_auth",
            Self::Validation => "provider_validation",
            Self::Overloaded => "provider_overloaded",
            Self::Cancelled => "provider_cancelled",
            Self::ContextOverflow => "provider_context_overflow",
            Self::Unknown => "provider_unknown",
        }
    }

    pub fn from_model_error_kind(kind: &ModelErrorKind) -> Self {
        use ModelErrorKind::*;
        match kind {
            RateLimited => Self::RateLimited,
            NetworkError => Self::Network,
            Timeout => Self::Timeout,
            AuthError => Self::Auth,
            InvalidRequest | JsonSchemaViolation | ContentFilter => Self::Validation,
            ProviderOverloaded => Self::Overloaded,
            Cancelled => Self::Cancelled,
            ContextLengthExceeded => Self::ContextOverflow,
            UnknownProviderError | UnsupportedCapability => Self::Unknown,
        }
    }
}

impl From<ProviderFailureKind> for TurnFailureCause {
    fn from(kind: ProviderFailureKind) -> Self {
        match kind {
            ProviderFailureKind::RateLimited => Self::ProviderRateLimited,
            ProviderFailureKind::Network => Self::ProviderNetwork,
            ProviderFailureKind::Timeout => Self::ProviderTimeout,
            ProviderFailureKind::Auth => Self::ProviderAuth,
            ProviderFailureKind::Validation => Self::ProviderValidation,
            ProviderFailureKind::Overloaded => Self::ProviderOverloaded,
            ProviderFailureKind::Cancelled => Self::ProviderCancelled,
            ProviderFailureKind::ContextOverflow => Self::ProviderContextOverflow,
            ProviderFailureKind::Unknown => Self::ProviderUnknown,
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
