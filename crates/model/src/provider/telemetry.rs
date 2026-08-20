use crate::{ModelErrorCategory, ModelUsage, ProviderApiProtocol, ProviderErrorStage};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Normalized provider stream data safe for the `AgentLoop` boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderStreamEvent {
    /// A visible text delta from the Responses `response.output_text.delta` event.
    OutputTextDelta { delta: String },
}

/// One safe runtime boundary event for a real provider HTTP attempt.
#[derive(Debug, Clone, PartialEq)]
pub enum ProviderAttemptEvent {
    /// Emitted immediately before the HTTP request is sent.
    Started(ProviderAttemptStarted),
    /// Emitted once when that same request reaches a terminal outcome.
    Finished(Box<ProviderAttemptOccurrence>),
}

/// The stable, non-sensitive fields known when a provider HTTP attempt starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderAttemptStarted {
    pub operation_phase: ProviderAttemptOperationPhase,
    pub provider_name: String,
    pub model_name: String,
    pub actual_api_protocol: ProviderApiProtocol,
    pub attempt_index: u32,
    pub started_at_unix_ms: u64,
}

/// Typed normalized text-stream capability for one selected provider protocol.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProviderStreamingCapability {
    /// The selected protocol has no normalized text stream at this boundary.
    #[default]
    Unsupported,
    /// The selected protocol emits ordered visible text deltas.
    OutputTextDelta,
}

impl ProviderStreamingCapability {
    /// Resolve the normalized stream capability from the actual selected protocol.
    pub const fn for_protocol(protocol: ProviderApiProtocol) -> Self {
        match protocol {
            ProviderApiProtocol::OpenAiResponses => Self::OutputTextDelta,
            ProviderApiProtocol::OpenAiChatCompletions => Self::OutputTextDelta,
            ProviderApiProtocol::Declared => Self::Unsupported,
        }
    }
}

/// 一次真实 provider HTTP attempt 所属的运行期操作阶段。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProviderAttemptOperationPhase {
    /// A caller-requested model completion.
    Completion,
}

/// 一次真实 provider HTTP attempt 的终态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProviderAttemptStatus {
    /// The attempt produced a valid provider response.
    Ok,
    /// The attempt ended with a non-cancellation error.
    Error,
    /// The attempt ended because cancellation was observed.
    Cancelled,
}

/// 一次真实 provider HTTP attempt 的脱敏运行期观测。
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderAttemptOccurrence {
    pub operation_phase: ProviderAttemptOperationPhase,
    pub provider_name: String,
    pub model_name: String,
    pub actual_api_protocol: ProviderApiProtocol,
    /// 该 aggregate 内按真实 HTTP attempt 顺序排列的 1-based 索引。
    pub attempt_index: u32,
    pub terminal_status: ProviderAttemptStatus,
    /// The wall-clock timestamp captured when this transport attempt was created.
    pub started_at_unix_ms: u64,
    /// The wall-clock timestamp captured when this transport attempt was terminalized.
    pub ended_at_unix_ms: u64,
    /// 从 attempt 创建到响应解析或失败终结的墙钟时长，不含 retry backoff。
    pub attempt_duration_ms: u64,
    /// 从发送请求到收到响应 headers；未收到 headers 时不可用。
    pub request_send_to_headers_ms: Option<u64>,
    /// 当前 transport 没有 admission queue，因此保持不可用。
    pub queue_duration_ms: Option<u64>,
    /// 仅流式 Responses 首个非空 output_text delta 的真实到达时长。
    pub time_to_first_text_delta_ms: Option<u64>,
    /// 该失败是否触发了有界 retry 调度。
    pub retry_scheduled: bool,
    /// retry 被调度时使用的独立 backoff，不计入发送时长。
    pub retry_backoff_ms: Option<u64>,
    pub error_category: Option<ModelErrorCategory>,
    pub error_stage: Option<ProviderErrorStage>,
    pub diagnostic_code: Option<String>,
    /// 成功响应明确提供 usage 时才存在。
    pub usage: Option<ModelUsage>,
    /// Bound by AgentLoop at the model-response ownership boundary.
    pub model_turn_ordinal: Option<u32>,
}

/// 一次模型提供方操作记录的 aggregate 尝试次数、重试次数和运行期 occurrences。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ProviderAttemptMetadata {
    pub attempt_count: u32,
    pub retry_count: u32,
    pub latency_ms: u64,
    /// 运行期 typed observation；不进入公共 schema。
    #[serde(skip)]
    #[schemars(skip)]
    pub occurrences: Vec<ProviderAttemptOccurrence>,
}
impl ProviderAttemptMetadata {
    pub(crate) fn zero() -> Self {
        Self {
            attempt_count: 0,
            retry_count: 0,
            latency_ms: 0,
            occurrences: Vec::new(),
        }
    }
}

/// Build the stable unsupported result used by Chat, Declared, and legacy providers.
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
