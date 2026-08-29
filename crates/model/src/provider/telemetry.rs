use crate::{ModelErrorCategory, ModelUsage, ProviderApiProtocol};
use serde::{Deserialize, Serialize};

/// 面向 `AgentLoop` 边界的规范化、安全的 provider 流数据。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderStreamEvent {
    /// 来自 Responses `response.output_text.delta` 事件的可见文本增量。
    OutputTextDelta { delta: String },
}

/// 一次真实 provider HTTP attempt 的安全运行时边界事件。
#[derive(Debug, Clone, PartialEq)]
pub enum ProviderAttemptEvent {
    /// 在 HTTP 请求发送前立即发射。
    Started(ProviderAttemptStarted),
    /// 同一请求到达终态时发射一次。
    Finished(Box<ProviderAttemptOccurrence>),
}

/// provider HTTP attempt 开始时已知的稳定、非敏感字段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderAttemptStarted {
    pub provider_name: String,
    pub model_name: String,
    pub actual_api_protocol: ProviderApiProtocol,
}

/// 某个所选 provider 协议的类型化规范化文本流能力。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderStreamingCapability {
    /// 该协议在本边界没有规范化文本流。
    #[default]
    Unsupported,
    /// 该协议按序发射可见文本增量。
    OutputTextDelta,
}

impl ProviderStreamingCapability {
    /// 从实际所选协议解析规范化流能力。
    pub const fn for_protocol(protocol: ProviderApiProtocol) -> Self {
        match protocol {
            ProviderApiProtocol::OpenAiResponses => Self::OutputTextDelta,
            ProviderApiProtocol::OpenAiChatCompletions => Self::OutputTextDelta,
            ProviderApiProtocol::Declared => Self::Unsupported,
        }
    }
}

/// 一次真实 provider HTTP attempt 的终态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderAttemptStatus {
    /// attempt 产出了有效 provider 响应。
    Ok,
    /// attempt 以非取消错误结束。
    Error,
    /// attempt 因观察到取消而结束。
    Cancelled,
}

/// 一次真实 provider HTTP attempt 的脱敏运行期观测。
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderAttemptOccurrence {
    pub provider_name: String,
    pub model_name: String,
    pub actual_api_protocol: ProviderApiProtocol,
    pub terminal_status: ProviderAttemptStatus,
    /// 从 attempt 创建到响应解析或失败终结的墙钟时长。
    pub attempt_duration_ms: u64,
    pub error_category: Option<ModelErrorCategory>,
    pub diagnostic_code: Option<String>,
    /// 成功响应明确提供 usage 时才存在。
    pub usage: Option<ModelUsage>,
}

/// 构建 Chat、Declared 与 legacy provider 使用的稳定 unsupported 结果。
pub(crate) fn provider_streaming_unsupported_error() -> crate::error::ProviderError {
    crate::error::ProviderError::from_model_error(
        crate::error::ModelError::new(
            crate::error::ModelErrorKind::UnsupportedCapability,
            "provider streaming is unsupported for this protocol",
        )
        .with_provider_diagnostic(
            crate::PROVIDER_STREAMING_UNSUPPORTED_CODE,
            crate::error::ProviderErrorStage::ResponseValidation,
        ),
    )
}
