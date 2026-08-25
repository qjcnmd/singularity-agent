#![deny(unsafe_code)]

//! 面向模型的消息、模型提供方能力契约和兼容 OpenAI 的传输。
//!
//! 模型提供方协商和校验位于此边界，使 `AgentLoop` 只执行选定模型提供方已声明或探测到的
//! 请求和 tool call。

pub(crate) const DEFAULT_MAX_TOOL_CALLS: u32 = 1;
/// 单次模型请求的默认 tool 数量上限。
pub const DEFAULT_MAX_TOOLS_PER_REQUEST: u32 = 8;
/// 默认模型上下文 token 上限。
pub const DEFAULT_MAX_CONTEXT_TOKENS: u32 = 128_000;
/// 默认模型输出 token 上限。
pub const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 4_096;
pub(crate) const MAX_CONFIGURED_CONTEXT_TOKENS: u32 = 2_000_000;
pub(crate) const MAX_CONFIGURED_OUTPUT_TOKENS: u32 = 1_000_000;
pub(crate) const ENV_PROVIDER: &str = "SINGULARITY_MODEL_PROVIDER";
pub(crate) const ENV_MODEL: &str = "SINGULARITY_MODEL";
pub(crate) const ENV_CONTEXT_TOKENS: &str = "SINGULARITY_MODEL_CONTEXT_TOKENS";
pub(crate) const ENV_MAX_OUTPUT_TOKENS: &str = "SINGULARITY_MODEL_MAX_OUTPUT_TOKENS";
pub(crate) const ENV_BASE_URL: &str = "SINGULARITY_BASE_URL";
pub(crate) const ENV_API_KEY: &str = "SINGULARITY_API_KEY";
/// 默认 provider 名称；适配器回显、selector 组合与元数据落盘共用这一个事实源。
pub const DEFAULT_PROVIDER_NAME: &str = "openai_compatible";
pub(crate) const CHAT_COMPLETIONS_PATH: &str = "/chat/completions";
pub(crate) const V1_CHAT_COMPLETIONS_PATH: &str = "/v1/chat/completions";
pub(crate) const RESPONSES_PATH: &str = "/responses";
pub(crate) const V1_RESPONSES_PATH: &str = "/v1/responses";
pub(crate) const USER_CONFIG_DIR_NAME: &str = ".singularity";
pub(crate) const USER_CONFIG_FILE_NAME: &str = "config.json";
/// 用户凭据唯一文件：写入走临时文件 + 同卷原子改名，读侧只认这一个文件名。
pub(crate) const USER_AUTH_FILE_NAME: &str = "auth.json";
pub(crate) const USER_AUTH_SCHEMA_VERSION: u32 = 1;
pub(crate) const MAX_MODEL_ID_LENGTH: usize = 512;
pub(crate) const MAX_CONFIG_AUTH_FILE_BYTES: usize = 1024 * 1024;
pub(crate) const PROVIDER_TIMEOUT_SECONDS: u64 = 120;
pub(crate) const MAX_PROVIDER_RESPONSE_BODY_BYTES: usize = 8 * 1024 * 1024;
/// 单次 provider complete 的最大 HTTP attempt 次数（首次尝试之外最多重试 5 次）。
pub(crate) const MAX_RETRY_AFTER_MS: u64 = 60_000;
/// Provider boundary code used when a protocol has no normalized text stream.
pub const PROVIDER_STREAMING_UNSUPPORTED_CODE: &str = "provider_streaming_unsupported";
pub(crate) const PROVIDER_SNAPSHOT_ID_PREFIX: &str = "provider_snapshot_";
pub(crate) const TEXT_TOOL_CALL_ENVELOPE_ERROR: &str = "text_tool_call_envelope_not_supported";
pub(crate) const HTTP_STATUS_UNAUTHORIZED: u16 = 401;
pub(crate) const HTTP_STATUS_FORBIDDEN: u16 = 403;
pub(crate) const HTTP_STATUS_REQUEST_TIMEOUT: u16 = 408;
pub(crate) const HTTP_STATUS_NOT_FOUND: u16 = 404;
pub(crate) const HTTP_STATUS_RATE_LIMITED: u16 = 429;
pub(crate) const HTTP_STATUS_INTERNAL_SERVER_ERROR: u16 = 500;

pub(crate) mod catalog;
mod config;
mod error;
mod openai;
mod provider;
mod transport;
mod types;

pub use config::{
    ModelBlockerKind, ModelProviderConfig, ModelSelectorParts, ProviderConfigSnapshot,
    ProviderConfigSource, ProviderConfigurationStatus, compose_model_selector,
    split_model_selector,
};
pub use error::*;
pub use openai::{chat_completions_endpoint, provider_error_response, responses_endpoint};
pub use provider::Provider;
pub use provider::contract::{
    ProviderApiProtocol, ProviderProtocolContract, ThinkingWireFormat,
    is_strict_tool_schema_compatible, validate_model_request,
    validate_model_request_with_capabilities, validate_model_response,
    validate_model_turn_response, validate_provider_config,
};
pub use provider::runtime::OpenAiProviderConfig;
pub(crate) use provider::runtime::SelectedModel;
pub use provider::telemetry::{
    ProviderAttemptEvent, ProviderAttemptOccurrence, ProviderAttemptOperationPhase,
    ProviderAttemptStarted, ProviderAttemptStatus, ProviderStreamEvent,
    ProviderStreamingCapability,
};
pub use transport::OpenAiProvider;
pub use types::*;
