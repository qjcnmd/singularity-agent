use serde::{Deserialize, Serialize};
use std::time::Duration;

/// 从模型提供方边界保留下来的具体失败类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelErrorKind {
    Cancelled,
    NetworkError,
    Timeout,
    RateLimited,
    ProviderOverloaded,
    AuthError,
    InvalidRequest,
    ContextLengthExceeded,
    JsonSchemaViolation,
    ContentFilter,
    UnknownProviderError,
}

/// 供调用方决定状态和恢复行为的较粗错误类别。请求观测与持久
/// provider_attempt 的错误词形共用同一 Display 投影（serde snake_case 单源）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelErrorCategory {
    Cancelled,
    Authentication,
    Network,
    ModelConfiguration,
    InvalidRequest,
    ContextLengthExceeded,
    JsonSchema,
    ContentFilter,
    ProviderUnavailable,
    UnknownProviderError,
}

impl std::fmt::Display for ModelErrorCategory {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&singularity_protocol::wire_word(self))
    }
}

/// 模型提供方失败，包含分类、可显示诊断和自动重试约束。
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderError {
    pub kind: ModelErrorKind,
    pub message: String,
    pub code: Option<String>,
    /// provider 定向的自动重试前最小延迟。
    pub retry_after: Option<Duration>,
    /// 调用方是否可自动重发同一逻辑请求。
    pub automatic_retry_allowed: bool,
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ProviderError {}

impl ProviderError {
    /// 创建带稳定 kind 的模型提供方错误。
    pub fn new(kind: ModelErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            code: None,
            retry_after: None,
            automatic_retry_allowed: true,
        }
    }

    /// 附加供事件与诊断使用的稳定错误码。
    pub fn with_code(mut self, code: impl Into<String>) -> Self {
        self.code = Some(code.into());
        self
    }

    /// 构造分类诊断，将具体原因纳入各入口实际显示的错误文本。
    pub(crate) fn diagnostic(
        kind: ModelErrorKind,
        message: impl Into<String>,
        code: impl Into<String>,
        details: Vec<String>,
    ) -> Self {
        let mut message = message.into();
        if !details.is_empty() {
            message.push_str(": ");
            message.push_str(&details.join(", "));
        }
        Self::new(kind, message).with_code(code)
    }

    /// 归类为公共模型错误类别。
    pub fn category(&self) -> ModelErrorCategory {
        match self.kind {
            ModelErrorKind::Cancelled => ModelErrorCategory::Cancelled,
            ModelErrorKind::AuthError => ModelErrorCategory::Authentication,
            ModelErrorKind::NetworkError | ModelErrorKind::Timeout => ModelErrorCategory::Network,
            ModelErrorKind::InvalidRequest
                if matches!(
                    self.code.as_deref(),
                    Some("provider_configuration_missing" | "provider_configuration_invalid")
                ) =>
            {
                ModelErrorCategory::ModelConfiguration
            }
            ModelErrorKind::InvalidRequest => ModelErrorCategory::InvalidRequest,
            ModelErrorKind::ContextLengthExceeded => ModelErrorCategory::ContextLengthExceeded,
            ModelErrorKind::JsonSchemaViolation => ModelErrorCategory::JsonSchema,
            ModelErrorKind::ContentFilter => ModelErrorCategory::ContentFilter,
            ModelErrorKind::RateLimited | ModelErrorKind::ProviderOverloaded => {
                ModelErrorCategory::ProviderUnavailable
            }
            ModelErrorKind::UnknownProviderError => ModelErrorCategory::UnknownProviderError,
        }
    }

    /// provider 是否明确拒绝请求的上下文规模（触发强制压缩路径）。
    pub fn is_context_overflow(&self) -> bool {
        self.kind == ModelErrorKind::ContextLengthExceeded
    }

    /// 判断是否允许自动重发同一请求。
    pub fn is_retryable(&self) -> bool {
        use ModelErrorKind::*;
        self.automatic_retry_allowed
            && matches!(
                self.kind,
                RateLimited | NetworkError | Timeout | ProviderOverloaded | UnknownProviderError
            )
    }

    /// 为所属重试策略保留 provider 定向延迟。
    pub fn with_retry_after(mut self, retry_after: Option<Duration>) -> Self {
        self.retry_after = retry_after;
        self
    }

    /// 标记该失败不可自动重放。
    pub fn without_automatic_retry(mut self) -> Self {
        self.automatic_retry_allowed = false;
        self
    }
}
