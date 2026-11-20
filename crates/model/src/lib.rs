//! 模型消息类型、模型配置，以及 Messages（Anthropic 兼容）和 OpenAI Responses 协议请求。
//!
//! 提供方返回的数据在这里检查和转换，Agent 拿到的是已经检查过的回复和工具调用。

pub(crate) const MESSAGES_PATH: &str = "/messages";
pub(crate) const RESPONSES_PATH: &str = "/responses";
pub(crate) const MODELS_PATH: &str = "/models";
pub(crate) const ANTHROPIC_VERSION: &str = "2023-06-01";
pub(crate) const USER_CONFIG_FILE_NAME: &str = "config.json";
/// 读取提供方响应时的空闲超时（秒）：每读到数据就重新计时，不限制长回复的总时长。
/// 留出足够时间给模型在第一个增量之前思考，慢的推理模型不会被误判为网络故障。
pub(crate) const PROVIDER_TIMEOUT_SECONDS: u64 = 300;
pub(crate) const HTTP_STATUS_REQUEST_TIMEOUT: u16 = 408;
pub(crate) const HTTP_STATUS_CONFLICT: u16 = 409;
pub(crate) const HTTP_STATUS_RATE_LIMITED: u16 = 429;

mod client;
pub(crate) mod config;
mod error;
mod messages;
mod provider;
mod responses;
mod transport;
mod types;

pub use client::ModelClient;
pub use config::{ModelConfigManager, ModelConfigurationSnapshot, discover_models};
pub use error::*;
pub use provider::ProviderStreamEvent;
pub use types::*;
