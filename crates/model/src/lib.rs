#![deny(unsafe_code)]

//! 面向模型的消息、模型提供方能力契约和兼容 OpenAI 的传输。
//!
//! 模型提供方协商和校验位于此边界，使 `AgentLoop` 只执行选定模型提供方已声明或探测到的
//! 请求和 tool call。

pub const DEFAULT_MAX_TOOL_CALLS: u32 = 1;
/// 单次模型请求的默认 tool 数量上限。
pub const DEFAULT_MAX_TOOLS_PER_REQUEST: u32 = 8;
/// 默认模型上下文 token 上限。
pub const DEFAULT_MAX_CONTEXT_TOKENS: u32 = 128_000;
/// 默认模型输出 token 上限。
pub const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 4_096;
pub const MAX_CONFIGURED_CONTEXT_TOKENS: u32 = 2_000_000;
pub const MAX_CONFIGURED_OUTPUT_TOKENS: u32 = 1_000_000;
pub const ENV_PROVIDER: &str = "SINGULARITY_MODEL_PROVIDER";
pub const ENV_MODEL: &str = "SINGULARITY_MODEL";
/// Explicit path to the immutable multi-provider model configuration JSON.
pub const ENV_MODELS_CONFIG: &str = "SINGULARITY_MODELS_CONFIG";
pub const ENV_CONTEXT_TOKENS: &str = "SINGULARITY_MODEL_CONTEXT_TOKENS";
pub const ENV_MAX_OUTPUT_TOKENS: &str = "SINGULARITY_MODEL_MAX_OUTPUT_TOKENS";
pub const ENV_BASE_URL: &str = "SINGULARITY_BASE_URL";
pub const ENV_API_KEY: &str = "SINGULARITY_API_KEY";
pub const DEFAULT_PROVIDER_NAME: &str = "openai_compatible";
pub const MAX_DISCOVERED_MODEL_IDS: usize = 1024;
pub const MAX_MODEL_ID_LENGTH: usize = 512;
pub const MAX_DISCOVERY_RESPONSE_BYTES: usize = 1024 * 1024;
pub const PROVIDER_TIMEOUT_SECONDS: u64 = 120;
pub const PROVIDER_RUNTIME_WORKER_THREADS: usize = 2;
pub const PROVIDER_STREAMING_UNSUPPORTED_CODE: &str = "provider_streaming_unsupported";
pub const MAX_PROVIDER_ATTEMPTS: u32 = 6;
pub const PROVIDER_RETRY_BASE_BACKOFF_MS: u64 = 500;
pub const PROVIDER_RETRY_MAX_BACKOFF_MS: u64 = 60_000;
pub const PROVIDER_SNAPSHOT_ID_PREFIX: &str = "provider_snapshot_";
pub const PROVIDER_RUNTIME_INITIALIZATION_ERROR_CODE: &str = "provider_runtime_initialization_failed";
pub const USER_CONFIG_DIR_NAME: &str = ".singularity";

pub mod builtin_models;
pub mod config;
pub mod discovery;
pub mod error;
pub mod openai;
pub mod provider;
pub mod transport;
pub mod types;

pub use config::{
    ModelBlockerKind, ModelCacheStatus, ModelDiscoveryStatus, ModelProviderConfig,
    ProviderConfigResolution, ProviderConfigSnapshot, ProviderConfigSource,
    ProviderConfigurationStatus, USER_AUTH_GENERATION_PREFIX, USER_AUTH_SCHEMA_VERSION,
    USER_CONFIG_FILE_NAME, USER_MODELS_CACHE_FILE_NAME, USER_MODELS_CACHE_SCHEMA_VERSION,
    USER_MODELS_CACHE_TTL_SECONDS, UserConfigImportResult, UserModelCatalog,
    UserModelCatalogEntry, UserProviderModelCatalog, import_env_to_user_config,
    read_user_model_catalog, resolve_provider_config,
};
pub use discovery::discover_provider_models;
pub use error::*;
pub use openai::{
    RESPONSES_PATH, chat_completions_endpoint, models_endpoint, provider_error_response,
    responses_endpoint,
};
pub use provider::contract::*;
pub use provider::runtime::{OpenAiProviderConfig, SelectedModel};
pub use provider::telemetry::*;
pub use provider::Provider;
pub use transport::OpenAiProvider;
pub use types::*;
