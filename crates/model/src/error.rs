use crate::provider::contract;
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// 从模型提供方边界保留下来的具体失败类型。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    UnsupportedCapability,
    UnknownProviderError,
}

/// 供调用方决定状态和恢复行为的较粗错误类别。
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
    UnsupportedCapability,
    ProviderUnavailable,
    UnknownProviderError,
}

/// 模型提供方请求或响应发生失败的阶段。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderErrorStage {
    ClientInitialization,
    RequestSend,
    ResponseStatus,
    ResponseBodyRead,
    ResponseJsonDecode,
    ResponseValidation,
    Cancelled,
}

/// 与模型或策略语义分开保留的传输层原因。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderTransportCategory {
    Timeout,
    Connect,
    Request,
    BodyRead,
    Unknown,
}

/// 带类型分类和清理后模型提供方诊断信息的模型错误。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelError {
    pub kind: ModelErrorKind,
    pub message: String,
    pub provider_name: Option<String>,
    pub model_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stage: Option<ProviderErrorStage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport_category: Option<ProviderTransportCategory>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_seconds: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_status: Option<u16>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub validation_errors: Vec<String>,
}

impl ModelError {
    /// 创建带稳定 kind 的模型错误。
    pub fn new(kind: ModelErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            provider_name: None,
            model_name: None,
            code: None,
            stage: None,
            transport_category: None,
            timeout_seconds: None,
            http_status: None,
            validation_errors: Vec::new(),
        }
    }

    /// 绑定 provider 名称。
    pub fn with_provider(mut self, provider_name: impl Into<String>) -> Self {
        self.provider_name = Some(provider_name.into());
        self
    }

    /// 绑定模型名称。
    pub fn with_model(mut self, model_name: impl Into<String>) -> Self {
        self.model_name = Some(model_name.into());
        self
    }

    /// 附加脱敏 provider 诊断。
    pub fn with_provider_diagnostic(
        mut self,
        code: impl Into<String>,
        stage: ProviderErrorStage,
    ) -> Self {
        self.code = Some(code.into());
        self.stage = Some(stage);
        self
    }

    /// 归类为公共模型错误类别。
    pub fn category(&self) -> ModelErrorCategory {
        contract::model_error_category(self)
    }
}

#[derive(Debug, Clone, PartialEq)]
/// 模型提供方失败，包含类型化模型错误与重试合同。
///
/// 对外展示文本单一来源为 [`Self::error`] 的 `message`：Display 直接委托，
/// 调用方读取展示文案统一走 Display，杜绝顶层与内层文案分叉。
pub struct ProviderError {
    pub error: Box<ModelError>,
    /// provider 定向的自动重试前最小延迟。
    pub retry_after: Option<Duration>,
    /// 调用方是否可自动重发同一逻辑请求。
    pub automatic_retry_allowed: bool,
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.error.message)
    }
}

impl std::error::Error for ProviderError {}

impl ProviderError {
    /// 从模型错误创建 provider 错误。
    pub fn from_model_error(error: ModelError) -> Self {
        Self {
            error: Box::new(error),
            retry_after: None,
            automatic_retry_allowed: true,
        }
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
