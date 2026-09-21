#![deny(unsafe_code)]

//! 面向模型的消息、模型提供方能力契约，以及与 OpenAI 兼容的传输。
//!
//! 提供方的协商和校验都放在这个边界上，AgentLoop 因此只执行选定提供方已声明或探测到的请求
//! 和工具调用。

/// 单次模型请求最多能带多少个工具。
pub(crate) const MAX_TOOLS_PER_REQUEST: usize = 8;
pub(crate) const DEFAULT_MAX_CONTEXT_TOKENS: u32 = 128_000;
pub(crate) const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 4_096;
pub(crate) const MAX_CONFIGURED_CONTEXT_TOKENS: u32 = 2_000_000;
pub(crate) const MAX_CONFIGURED_OUTPUT_TOKENS: u32 = 1_000_000;
/// 默认的提供方名称；适配器回显、selector 拼接和元数据落盘都用这一个来源。
pub const DEFAULT_PROVIDER_NAME: &str = "openai_compatible";
pub(crate) const CHAT_COMPLETIONS_PATH: &str = "/chat/completions";
pub(crate) const RESPONSES_PATH: &str = "/responses";
pub(crate) const MODELS_PATH: &str = "/models";
pub(crate) const USER_CONFIG_FILE_NAME: &str = "config.json";
/// 用户凭据唯一的文件：写入先落临时文件、再在同卷内原子改名，读取只认这个名字。
pub(crate) const USER_AUTH_FILE_NAME: &str = "auth.json";
pub(crate) const USER_AUTH_SCHEMA_VERSION: u32 = 1;
pub(crate) const MAX_MODEL_ID_LENGTH: usize = 512;
/// 一次提供方响应的空闲读超时（秒）：reqwest 把它用在每次读操作上，读到数据
/// 就重置，所以它不限制一次长生成的总时长，只在连接静默时快速失败。
/// 这个值留出了首个增量到来前的静默思考时间，免得较慢的推理响应被当成网络失败。
pub(crate) const PROVIDER_TIMEOUT_SECONDS: u64 = 300;
pub(crate) const MAX_PROVIDER_RESPONSE_BODY_BYTES: usize = 8 * 1024 * 1024;
/// 单次 Retry-After 等待的上限（毫秒）；重试调度在 agent 层做，传输层只管一次尝试。
pub(crate) const MAX_RETRY_AFTER_MS: u64 = 60_000;
pub(crate) const HTTP_STATUS_UNAUTHORIZED: u16 = 401;
pub(crate) const HTTP_STATUS_FORBIDDEN: u16 = 403;
pub(crate) const HTTP_STATUS_REQUEST_TIMEOUT: u16 = 408;
pub(crate) const HTTP_STATUS_CONFLICT: u16 = 409;
pub(crate) const HTTP_STATUS_RATE_LIMITED: u16 = 429;

pub(crate) mod config;
mod error;
mod openai;
mod provider;
mod transport;
mod types;

pub use config::{
    ModelConfigManager, ModelConfigurationSnapshot, ModelSelectorParts, ProviderConfigSnapshot,
    compose_model_selector, discover_models, split_model_selector,
};
pub use error::*;
pub use openai::OpenAiProvider;
pub use provider::contract::ProviderApiProtocol;
pub use provider::telemetry::{
    ProviderAttemptEvent, ProviderAttemptOccurrence, ProviderAttemptStarted, ProviderAttemptStatus,
    ProviderStreamEvent,
};
pub use provider::{Provider, ProviderCallError};
pub use types::*;

/// 行为确定的 Provider 替身：只在 test-support feature 下暴露给测试使用。
#[cfg(feature = "test-support")]
pub use provider::test_support;
