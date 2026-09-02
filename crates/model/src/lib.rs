#![deny(unsafe_code)]

//! 面向模型的消息、模型提供方能力契约和兼容 OpenAI 的传输。
//!
//! 模型提供方协商和校验位于此边界，使 `AgentLoop` 只执行选定模型提供方已声明或探测到的
//! 请求和 tool call。

/// 单次模型请求的默认 tool 数量上限（模型 crate 内的默认事实源）。
pub(crate) const DEFAULT_MAX_TOOLS_PER_REQUEST: u32 = 8;
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
/// 一次 provider 响应的**空闲**读界（秒）：reqwest 把它作用在每次读操作上、
/// 读到即重置，因此它不限制一次长生成的总时长，只在连接静默时 fail fast。
/// 取值要容纳推理模型在首个增量之前的静默思考期：实测一轮最长单次尝试 95.8 秒
/// 已接近旧值 120 秒，而端点更慢时 120 秒会把一次正常长思考判成网络失败。
pub(crate) const PROVIDER_TIMEOUT_SECONDS: u64 = 300;
pub(crate) const MAX_PROVIDER_RESPONSE_BODY_BYTES: usize = 8 * 1024 * 1024;
/// 单次 Retry-After 等待的上限（毫秒）；重试调度由 agent 层执行，传输层单 attempt。
pub(crate) const MAX_RETRY_AFTER_MS: u64 = 60_000;
pub(crate) const TEXT_TOOL_CALL_ENVELOPE_ERROR: &str = "text_tool_call_envelope_not_supported";
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
    ModelConfigurationSnapshot, ModelSelectorParts, ProviderConfigSnapshot, compose_model_selector,
    split_model_selector,
};
pub use error::*;
pub use openai::{chat_completions_endpoint, responses_endpoint};
pub use provider::Provider;
pub use provider::attempt::duration_millis;
pub use provider::contract::{
    ProviderApiProtocol, ProviderProtocolContract, ThinkingWireFormat,
    validate_model_request_with_capabilities, validate_model_turn_response,
};
pub use provider::policy::TurnRetryPolicy;
pub(crate) use provider::runtime::SelectedModel;
pub use provider::telemetry::{
    ProviderAttemptEvent, ProviderAttemptOccurrence, ProviderAttemptStatus, ProviderStreamEvent,
};
pub use transport::OpenAiProvider;
pub use types::*;

/// 确定性 Provider 替身：仅在 `test-support` feature 下暴露给测试消费者。
#[cfg(feature = "test-support")]
pub use provider::test_support;
