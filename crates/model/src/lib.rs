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
pub(crate) const DEFAULT_PROVIDER_NAME: &str = "openai_compatible";
pub(crate) const CHAT_COMPLETIONS_PATH: &str = "/chat/completions";
pub(crate) const V1_CHAT_COMPLETIONS_PATH: &str = "/v1/chat/completions";
pub(crate) const RESPONSES_PATH: &str = "/responses";
pub(crate) const V1_RESPONSES_PATH: &str = "/v1/responses";
pub(crate) const USER_CONFIG_DIR_NAME: &str = ".singularity";
pub(crate) const USER_CONFIG_FILE_NAME: &str = "config.json";
/// 用户凭据唯一文件：写入走临时文件 + 同卷原子改名，读侧只认这一个文件名。
pub(crate) const USER_AUTH_FILE_NAME: &str = "auth.v1.json";
pub(crate) const USER_AUTH_SCHEMA_VERSION: u32 = 1;
pub(crate) const USER_MODELS_CACHE_FILE_NAME: &str = "models-cache.json";
pub(crate) const USER_MODELS_CACHE_SCHEMA_VERSION: u32 = 1;
pub(crate) const USER_MODELS_CACHE_TTL_SECONDS: u64 = 24 * 60 * 60;
/// models.dev 目录元数据的独立缓存文件名（投影子集，与发现缓存分离）。
pub(crate) const METADATA_CACHE_FILE_NAME: &str = "metadata-cache.json";
pub(crate) const METADATA_CACHE_SCHEMA_VERSION: u32 = 1;
/// models.dev 公开模型目录的 api.json 端点。
pub(crate) const METADATA_DIRECTORY_URL: &str = "https://models.dev/api.json";
pub(crate) const MAX_DISCOVERED_MODEL_IDS: usize = 1024;
pub(crate) const MAX_MODEL_ID_LENGTH: usize = 512;
pub(crate) const MAX_DISCOVERY_RESPONSE_BYTES: usize = 1024 * 1024;
pub(crate) const PROVIDER_TIMEOUT_SECONDS: u64 = 120;
pub(crate) const PROVIDER_CANCELLATION_POLL_MS: u64 = 25;
pub(crate) const MAX_PROVIDER_RESPONSE_BODY_BYTES: usize = 8 * 1024 * 1024;
/// 单次 provider complete 的最大 HTTP attempt 次数（首次尝试之外最多重试 5 次）。
pub(crate) const MAX_PROVIDER_ATTEMPTS: u32 = 6;
pub(crate) const PROVIDER_RETRY_BASE_BACKOFF_MS: u64 = 500;
pub(crate) const PROVIDER_RETRY_MAX_BACKOFF_MS: u64 = 60_000;
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

mod builtin_models;
mod config;
mod discovery;
mod error;
mod openai;
mod provider;
mod transport;
mod types;

pub use config::{
    AddProviderResult, ModelBlockerKind, ModelCacheStatus, ModelDiscoveryStatus,
    ModelProviderConfig, ModelSelectorParts, ProviderConfigResolution, ProviderConfigSnapshot,
    ProviderConfigSource, ProviderConfigurationStatus, UserConfigImportResult, UserModelCatalog,
    UserModelCatalogEntry, UserProviderModelCatalog, add_configured_provider,
    discover_provider_model_ids, import_env_to_user_config, read_user_model_catalog,
    refresh_model_metadata, resolve_provider_config, split_model_selector,
};
pub use error::*;
pub use openai::{
    chat_completions_endpoint, models_endpoint, provider_error_response, responses_endpoint,
};
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
    ProviderAttemptEvent, ProviderAttemptMetadata, ProviderAttemptOccurrence,
    ProviderAttemptOperationPhase, ProviderAttemptStarted, ProviderAttemptStatus,
    ProviderStreamEvent, ProviderStreamingCapability,
};
pub use transport::OpenAiProvider;
pub use types::*;
