#![deny(unsafe_code)]

//! 面向模型的消息、模型提供方能力契约和兼容 OpenAI 的传输。
//!
//! 模型提供方协商和校验位于此边界，使 AgentLoop 只执行选定模型提供方已声明或探测到的
//! 请求和 tool call。

/// 单次模型请求的工具数量上限。
pub(crate) const MAX_TOOLS_PER_REQUEST: usize = 8;
/// 默认模型上下文 token 上限。
pub(crate) const DEFAULT_MAX_CONTEXT_TOKENS: u32 = 128_000;
/// 默认模型输出 token 上限。
pub(crate) const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 4_096;
pub(crate) const MAX_CONFIGURED_CONTEXT_TOKENS: u32 = 2_000_000;
pub(crate) const MAX_CONFIGURED_OUTPUT_TOKENS: u32 = 1_000_000;
/// 默认 provider 名称；适配器回显、selector 组合与元数据落盘共用这一个事实源。
pub const DEFAULT_PROVIDER_NAME: &str = "openai_compatible";
pub(crate) const CHAT_COMPLETIONS_PATH: &str = "/chat/completions";
pub(crate) const V1_CHAT_COMPLETIONS_PATH: &str = "/v1/chat/completions";
pub(crate) const RESPONSES_PATH: &str = "/responses";
pub(crate) const V1_RESPONSES_PATH: &str = "/v1/responses";
pub(crate) const USER_CONFIG_FILE_NAME: &str = "config.json";
/// 用户凭据唯一文件：写入走临时文件 + 同卷原子改名，读侧只认这一个文件名。
pub(crate) const USER_AUTH_FILE_NAME: &str = "auth.json";
pub(crate) const USER_AUTH_SCHEMA_VERSION: u32 = 1;
pub(crate) const MAX_MODEL_ID_LENGTH: usize = 512;
/// 一次 provider 响应的空闲读界（秒）：reqwest 把它作用在每次读操作上、
/// 读到即重置，因此它不限制一次长生成的总时长，只在连接静默时 fail fast。
/// 预留首个增量前的静默思考时间，避免较慢的推理响应被误判为网络失败。
pub(crate) const PROVIDER_TIMEOUT_SECONDS: u64 = 300;
pub(crate) const MAX_PROVIDER_RESPONSE_BODY_BYTES: usize = 8 * 1024 * 1024;
/// 单次 Retry-After 等待的上限（毫秒）；重试调度由 agent 层执行，传输层单 attempt。
pub(crate) const MAX_RETRY_AFTER_MS: u64 = 60_000;
pub(crate) const HTTP_STATUS_UNAUTHORIZED: u16 = 401;
pub(crate) const HTTP_STATUS_FORBIDDEN: u16 = 403;
pub(crate) const HTTP_STATUS_REQUEST_TIMEOUT: u16 = 408;
pub(crate) const HTTP_STATUS_CONFLICT: u16 = 409;
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
    ModelConfigOwner, ModelConfigurationSnapshot, ModelSelectorParts, ProviderConfigSnapshot,
    compose_model_selector, split_model_selector,
};
pub use error::*;
pub use provider::contract::{
    ProviderApiProtocol, ThinkingWireFormat, validate_model_request, validate_model_turn_response,
};
pub use provider::policy::TurnRetryPolicy;
pub(crate) use provider::runtime::SelectedModel;
pub use provider::telemetry::{
    ProviderAttemptEvent, ProviderAttemptOccurrence, ProviderAttemptStarted, ProviderAttemptStatus,
    ProviderStreamEvent,
};
pub use provider::{Provider, ProviderCallError};
pub use transport::OpenAiProvider;
pub use types::*;

/// 确定性 Provider 替身：仅在 test-support feature 下暴露给测试消费者。
#[cfg(feature = "test-support")]
pub use provider::test_support;
