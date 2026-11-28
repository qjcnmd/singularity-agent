pub(crate) const ANTHROPIC_VERSION: &str = "2023-06-01";
pub(crate) const USER_CONFIG_FILE_NAME: &str = "config.json";
pub(crate) const PROVIDER_TIMEOUT_SECONDS: u64 = 300;

mod client;
pub(crate) mod config;
mod error;
mod messages;
#[cfg(test)]
mod property_tests;
mod provider;
mod responses;
mod transport;
mod types;
mod web;

pub use client::ModelClient;
pub use config::{ModelConfigManager, ModelConfigurationSnapshot};
pub use error::{ModelErrorKind, ProviderError};
pub use provider::ProviderStreamEvent;
pub(crate) use types::ModelStopReason;
pub use types::{
    ContentBlock, Message, ModelToolCall, ModelTurnRequest, ModelTurnResponse, ModelUsage,
    ProviderBlockMetadata, ProviderMessageMetadata, ToolDefinition, estimate_tool_tokens,
};
pub use web::{WebSearchAccess, validate_fetch_url};
