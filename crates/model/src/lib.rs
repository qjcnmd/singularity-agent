#![deny(unsafe_code)]

//! 面向模型的消息、模型提供方能力契约和兼容 OpenAI 的传输。
//!
//! 模型提供方协商和校验位于此边界，使 `AgentLoop` 只执行选定模型提供方已声明或探测到的
//! 请求和 tool call。

use std::fmt;
use std::future::Future;
use std::sync::Arc;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use singularity_core::CancellationToken;
use thiserror::Error;

const DEFAULT_MAX_TOOL_CALLS: u32 = 1;
/// 单次模型请求的默认 tool 数量上限。
pub const DEFAULT_MAX_TOOLS_PER_REQUEST: u32 = 8;
/// 默认模型上下文 token 上限。
pub const DEFAULT_MAX_CONTEXT_TOKENS: u32 = 128_000;
/// 默认模型输出 token 上限。
pub const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 4_096;
const MAX_CONFIGURED_CONTEXT_TOKENS: u32 = 2_000_000;
// Keep the configured ceiling above the currently documented 384k-output
// models; each model still must satisfy output < context.
const MAX_CONFIGURED_OUTPUT_TOKENS: u32 = 1_000_000;
const ENV_PROVIDER: &str = "SINGULARITY_MODEL_PROVIDER";
const ENV_MODEL: &str = "SINGULARITY_MODEL";
/// Explicit path to the immutable multi-provider model configuration JSON.
pub const ENV_MODELS_CONFIG: &str = "SINGULARITY_MODELS_CONFIG";
const ENV_CONTEXT_TOKENS: &str = "SINGULARITY_MODEL_CONTEXT_TOKENS";
const ENV_MAX_OUTPUT_TOKENS: &str = "SINGULARITY_MODEL_MAX_OUTPUT_TOKENS";
const ENV_BASE_URL: &str = "SINGULARITY_BASE_URL";
const ENV_API_KEY: &str = "SINGULARITY_API_KEY";
const DEFAULT_PROVIDER_NAME: &str = "openai_compatible";
const CHAT_COMPLETIONS_PATH: &str = "/chat/completions";
const V1_CHAT_COMPLETIONS_PATH: &str = "/v1/chat/completions";
const RESPONSES_PATH: &str = "/responses";
const V1_RESPONSES_PATH: &str = "/v1/responses";
const USER_CONFIG_DIR_NAME: &str = ".singularity";
const USER_CONFIG_FILE_NAME: &str = "config.json";
const USER_AUTH_GENERATION_PREFIX: &str = "auth.v1-";
const USER_AUTH_SCHEMA_VERSION: u32 = 1;
const USER_MODELS_CACHE_FILE_NAME: &str = "models-cache.json";
const USER_MODELS_CACHE_SCHEMA_VERSION: u32 = 1;
const USER_MODELS_CACHE_TTL_SECONDS: u64 = 24 * 60 * 60;
const MAX_DISCOVERED_MODEL_IDS: usize = 1024;
const MAX_MODEL_ID_LENGTH: usize = 512;
const MAX_DISCOVERY_RESPONSE_BYTES: usize = 1024 * 1024;
const PROVIDER_TIMEOUT_SECONDS: u64 = 120;
const PROVIDER_RUNTIME_WORKER_THREADS: usize = 2;
const PROVIDER_RUNTIME_INITIALIZATION_ERROR_CODE: &str = "provider_runtime_initialization_failed";
const PROVIDER_CANCELLATION_POLL_MS: u64 = 25;
const MAX_PROVIDER_RESPONSE_BODY_BYTES: usize = 8 * 1024 * 1024;
/// 单次 provider complete 的最大 HTTP attempt 次数（首次尝试之外最多重试 5 次）。
const MAX_PROVIDER_ATTEMPTS: u32 = 6;
const PROVIDER_RETRY_BASE_BACKOFF_MS: u64 = 50;
/// Provider boundary code used when a protocol has no normalized text stream.
pub const PROVIDER_STREAMING_UNSUPPORTED_CODE: &str = "provider_streaming_unsupported";
const PROVIDER_SNAPSHOT_ID_PREFIX: &str = "provider_snapshot_";
const REQUIRED_TOOL_CHOICE_MISSING_ERROR: &str = "required_tool_call_missing";
const REQUIRED_TOOL_CHOICE_REQUIRES_TOOLS_ERROR: &str = "required_tool_choice_requires_tools";
const REQUIRED_TOOL_CHOICE_UNSUPPORTED_ERROR: &str =
    "provider_does_not_support_required_tool_choice";
const TEXT_TOOL_CALL_ENVELOPE_ERROR: &str = "text_tool_call_envelope_not_supported";
const HTTP_STATUS_UNAUTHORIZED: u16 = 401;
const HTTP_STATUS_FORBIDDEN: u16 = 403;
const HTTP_STATUS_REQUEST_TIMEOUT: u16 = 408;
const HTTP_STATUS_NOT_FOUND: u16 = 404;
const HTTP_STATUS_RATE_LIMITED: u16 = 429;
const HTTP_STATUS_INTERNAL_SERVER_ERROR: u16 = 500;

mod builtin_models;
mod config;
mod contract;
mod openai;
mod transport;

pub use builtin_models::{
    ModelCost, builtin_model_cost, estimate_cost, estimate_cost_peak, is_peak_hour_utc8,
};
pub use config::{import_env_to_user_config, read_user_model_catalog, resolve_provider_config};
pub use contract::{
    is_strict_tool_schema_compatible, validate_model_request,
    validate_model_request_with_capabilities, validate_model_response,
    validate_model_turn_response, validate_provider_config,
};
pub use openai::{
    chat_completions_endpoint, models_endpoint, provider_error_response, responses_endpoint,
};

#[cfg(test)]
use config::provider_initialization_blocker;
#[cfg(test)]
use transport::provider_runtime_error;

/// 面向模型的对话历史支持的角色。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ModelRole {
    System,
    Developer,
    User,
    Assistant,
    Tool,
}

/// 面向模型提供方的消息，包括继续 turn 所需的 tool call 元数据。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ModelMessage {
    pub role: ModelRole,
    pub content: String,
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ModelToolCall>,
}

impl ModelMessage {
    /// 创建普通文本消息。
    pub fn text(role: ModelRole, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
            tool_call_id: None,
            tool_calls: Vec::new(),
        }
    }

    /// 创建带结构化 tool calls 的 assistant 消息。
    pub fn assistant_tool_calls(tool_calls: Vec<ModelToolCall>) -> Self {
        Self {
            role: ModelRole::Assistant,
            content: String::new(),
            tool_call_id: None,
            tool_calls,
        }
    }
}

/// 控制模型提供方可以选择 tool、必须选择一个 tool，还是不得调用 tool。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ToolChoiceMode {
    Auto,
    None,
    Required,
}

/// 应用于一次模型请求的 tool 选择限制和模式严格程度。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ToolChoicePolicy {
    pub mode: ToolChoiceMode,
    pub max_tool_calls: u32,
    pub strict_tool_schema: bool,
}

impl Default for ToolChoicePolicy {
    fn default() -> Self {
        Self {
            mode: ToolChoiceMode::Auto,
            max_tool_calls: DEFAULT_MAX_TOOL_CALLS,
            strict_tool_schema: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
/// 模型 tool call 的解析状态。
pub enum ModelToolParseStatus {
    Valid,
    InvalidJson,
    SchemaMismatch,
    UnknownTool,
}

/// 一个可执行 tool 面向模型提供方暴露的模式。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ModelToolSchema {
    pub name: String,
    pub description: String,
    pub parameters_schema: Value,
}

/// 已解析的模型 tool call，以及原始参数和校验结果。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ModelToolCall {
    pub tool_call_id: String,
    pub tool_name: String,
    pub arguments: Value,
    pub raw_arguments: String,
    pub parse_status: ModelToolParseStatus,
    pub validation_errors: Vec<String>,
}

/// tool 推理内容是否符合模型提供方的 tool call 历史契约。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProviderToolReasoningMode {
    #[default]
    Unspecified,
    DisabledForToolCalls,
    /// The adapter must preserve Chat Completions `reasoning_content` on every
    /// assistant tool-call continuation.
    ReplayReasoningContent,
    /// The adapter must preserve Responses reasoning output items verbatim.
    ReplayResponsesItems,
}

/// Provider-private reasoning state that is safe to replay at the adapter
/// boundary but must never be displayed or projected into public conversation,
/// trace, Evaluation, or error schemas. The Rust type is public only because
/// the harness owns the reasoning-replay boundary between turns.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "protocol", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProviderReasoningReplay {
    Chat {
        provider_name: String,
        model_name: String,
        reasoning_effort: String,
        tool_call_ids: Vec<String>,
        reasoning_content: String,
    },
    Responses {
        provider_name: String,
        model_name: String,
        reasoning_effort: String,
        tool_call_ids: Vec<String>,
        /// The complete provider output sequence is retained verbatim.  The
        /// adapter only appends later `function_call_output` items.
        items: Vec<Value>,
    },
}

impl fmt::Debug for ProviderReasoningReplay {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut debug = formatter.debug_struct("ProviderReasoningReplay");
        match self {
            Self::Chat {
                tool_call_ids,
                reasoning_content,
                ..
            } => {
                debug
                    .field("protocol", &"chat")
                    .field("tool_call_count", &tool_call_ids.len())
                    .field("reasoning_content_len", &reasoning_content.len());
            }
            Self::Responses {
                tool_call_ids,
                items,
                ..
            } => {
                debug
                    .field("protocol", &"responses")
                    .field("tool_call_count", &tool_call_ids.len())
                    .field("output_item_count", &items.len())
                    .field("reasoning_item_present", &true);
            }
        }
        debug.finish()
    }
}

impl ProviderReasoningReplay {
    /// Validate the opaque replay at the owning provider boundary without
    /// exposing its private payload in an error.
    pub(crate) fn validate(&self) -> Result<(), &'static str> {
        match self {
            Self::Chat {
                provider_name,
                model_name,
                reasoning_effort,
                tool_call_ids,
                reasoning_content,
            } => {
                validate_replay_binding(provider_name, model_name, reasoning_effort)?;
                validate_replay_tool_call_ids(tool_call_ids)?;
                if reasoning_content.is_empty() {
                    return Err("provider reasoning replay content is empty");
                }
            }
            Self::Responses {
                provider_name,
                model_name,
                reasoning_effort,
                tool_call_ids,
                items,
            } => {
                validate_replay_binding(provider_name, model_name, reasoning_effort)?;
                validate_replay_tool_call_ids(tool_call_ids)?;
                validate_responses_replay_items(items, tool_call_ids)?;
            }
        }
        Ok(())
    }

    /// Return only the validation result at non-model boundaries.
    pub fn is_valid(&self) -> bool {
        self.validate().is_ok()
    }

    /// Validate the replay against one selected provider/model/variant and mode.
    pub(crate) fn validate_for(
        &self,
        provider_name: &str,
        model_name: &str,
        reasoning_effort: &str,
        mode: ProviderToolReasoningMode,
    ) -> Result<(), &'static str> {
        self.validate()?;
        let (replay_provider, replay_model, replay_variant) = self.binding_internal();
        if replay_provider != provider_name
            || replay_model != model_name
            || replay_variant != reasoning_effort
            || self.mode_internal() != mode
        {
            return Err("provider reasoning replay binding does not match selected model");
        }
        Ok(())
    }

    fn binding_internal(&self) -> (&str, &str, &str) {
        match self {
            Self::Chat {
                provider_name,
                model_name,
                reasoning_effort,
                ..
            }
            | Self::Responses {
                provider_name,
                model_name,
                reasoning_effort,
                ..
            } => (provider_name, model_name, reasoning_effort),
        }
    }

    /// Returns whether the replay is bound to all supplied tool-call ids in order.
    pub fn matches_tool_call_ids(&self, ids: &[String]) -> bool {
        match self {
            Self::Chat { tool_call_ids, .. } | Self::Responses { tool_call_ids, .. } => {
                tool_call_ids == ids
            }
        }
    }

    /// Return true when the replay contains a tool-call id without exposing ids.
    pub fn has_tool_call_id(&self, id: &str) -> bool {
        match self {
            Self::Chat { tool_call_ids, .. } | Self::Responses { tool_call_ids, .. } => {
                tool_call_ids.iter().any(|candidate| candidate == id)
            }
        }
    }

    /// Return true when one assistant message in the supplied model history
    /// carries exactly this replay's ordered tool-call binding.
    pub fn is_bound_to_messages(&self, messages: &[ModelMessage]) -> bool {
        self.bound_assistant_count(messages) == 1
    }

    /// Count assistant tool-call messages with this exact ordered binding.
    pub fn bound_assistant_count(&self, messages: &[ModelMessage]) -> usize {
        messages
            .iter()
            .filter(|message| {
                message.role == ModelRole::Assistant
                    && self.matches_tool_call_ids(
                        &message
                            .tool_calls
                            .iter()
                            .map(|call| call.tool_call_id.clone())
                            .collect::<Vec<_>>(),
                    )
            })
            .count()
    }

    /// Returns the protocol-specific reasoning mode inside the model crate.
    pub(crate) fn mode_internal(&self) -> ProviderToolReasoningMode {
        match self {
            Self::Chat { .. } => ProviderToolReasoningMode::ReplayReasoningContent,
            Self::Responses { .. } => ProviderToolReasoningMode::ReplayResponsesItems,
        }
    }
}

fn validate_replay_binding(
    provider_name: &str,
    model_name: &str,
    reasoning_effort: &str,
) -> Result<(), &'static str> {
    for value in [provider_name, model_name, reasoning_effort] {
        if value.is_empty()
            || value
                .chars()
                .any(|character| character.is_whitespace() || character.is_control())
        {
            return Err("provider reasoning replay binding is malformed");
        }
    }
    if reasoning_effort == "off" {
        return Err("provider reasoning replay cannot use disabled variant");
    }
    Ok(())
}

fn validate_replay_tool_call_ids(ids: &[String]) -> Result<(), &'static str> {
    if ids.is_empty()
        || ids.iter().any(|id| {
            id.is_empty()
                || id
                    .chars()
                    .any(|character| character.is_whitespace() || character.is_control())
        })
        || ids.iter().collect::<std::collections::BTreeSet<_>>().len() != ids.len()
    {
        return Err("provider reasoning replay tool-call identity is invalid");
    }
    Ok(())
}

fn validate_responses_replay_items(
    items: &[Value],
    tool_call_ids: &[String],
) -> Result<(), &'static str> {
    if items.is_empty() {
        return Err("Responses reasoning replay output is empty");
    }
    let mut reasoning_count = 0usize;
    let mut function_call_ids = Vec::new();
    for item in items {
        let object = item
            .as_object()
            .ok_or("Responses replay output item is not an object")?;
        let item_type = object
            .get("type")
            .and_then(Value::as_str)
            .ok_or("Responses replay output item type is missing")?;
        match item_type {
            "reasoning" => {
                let id = object
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())
                    .ok_or("Responses reasoning item id is missing")?;
                if id.chars().any(|character| character.is_control()) {
                    return Err("Responses reasoning item id is invalid");
                }
                reasoning_count = reasoning_count.saturating_add(1);
            }
            "message" => {}
            "function_call" => {
                let call_id = object
                    .get("call_id")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())
                    .ok_or("Responses function_call id is missing")?;
                if call_id.chars().any(|character| character.is_control()) {
                    return Err("Responses function_call id is invalid");
                }
                function_call_ids.push(call_id.to_string());
            }
            _ => return Err("Responses replay output item type is unsupported"),
        }
    }
    if reasoning_count == 0 {
        return Err("Responses reasoning replay item is missing");
    }
    if function_call_ids != tool_call_ids
        || function_call_ids
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len()
            != function_call_ids.len()
    {
        return Err("Responses replay function_call ids do not match tool calls");
    }
    Ok(())
}

/// 为模型提供方完成请求选定的线路协议。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProviderApiProtocol {
    #[default]
    Declared,
    OpenAiResponses,
    OpenAiChatCompletions,
}

/// Chat Completions reasoning fields are selected explicitly by the model
/// catalog.  No provider or model name is interpreted to choose a wire shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingWireFormat {
    /// Existing `thinking: {"type": "enabled|disabled"}` fields.
    ThinkingType,
    /// Top-level `enable_thinking` boolean used by providers that document it.
    EnableThinking,
}

/// State of one provider's `/models` discovery record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ModelDiscoveryStatus {
    Fresh,
    Stale,
    Unavailable,
    NotConfigured,
}

/// Result of reading or refreshing the optional user model discovery cache.
///
/// Cache state never changes whether the provider configuration itself is
/// usable; it only explains why discovery used live, stale, or no cached ids.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ModelCacheStatus {
    NotPresent,
    Valid,
    Invalid,
    ReadFailed,
    WriteFailed,
}

/// A discovered model id and whether an explicit capability override makes it
/// safe to select for execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct UserModelCatalogEntry {
    pub id: String,
    pub discovered: bool,
    pub explicit: bool,
    pub selectable: bool,
    pub max_context_tokens: Option<u32>,
    pub reasoning_variants: Vec<String>,
    pub default_variant: Option<String>,
}

/// Redacted user-level provider catalog returned by `sg config models`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct UserProviderModelCatalog {
    pub provider_name: String,
    pub base_url_present: bool,
    pub api_key_present: bool,
    pub discovery: ModelDiscoveryStatus,
    pub models: Vec<UserModelCatalogEntry>,
    pub error: Option<String>,
}

/// Redacted user-level model catalog.  It never contains a base URL or secret.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct UserModelCatalog {
    pub default_selector: Option<String>,
    pub cache_status: ModelCacheStatus,
    pub providers: Vec<UserProviderModelCatalog>,
}

/// Outcome of importing a dotenv file into the user-level split config.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct UserConfigImportResult {
    pub config_path: String,
    pub auth_path: String,
    pub provider_name: String,
    pub default_selector: Option<String>,
    pub selectable: bool,
}

/// 模型提供方必须遵守、用于构建请求和校验响应的能力。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProviderProtocolContract {
    pub supports_tools: bool,
    pub supports_parallel_tool_calls: bool,
    pub supports_required_tool_choice: bool,
    pub supports_strict_tool_schema: bool,
    pub tool_reasoning_mode: ProviderToolReasoningMode,
    pub max_tools_per_request: u32,
    pub supports_json_mode: bool,
    pub supports_system_message: bool,
    pub supports_developer_message: bool,
    /// 单次请求可携带的最大工具调用数（并行上限；执行侧仍逐个顺序完成）。
    pub max_parallel_tool_calls: u32,
    pub max_context_tokens: Option<u32>,
    pub max_output_tokens: u32,
}

impl Default for ProviderProtocolContract {
    fn default() -> Self {
        Self {
            supports_tools: true,
            supports_parallel_tool_calls: false,
            supports_required_tool_choice: false,
            supports_strict_tool_schema: false,
            tool_reasoning_mode: ProviderToolReasoningMode::Unspecified,
            max_tools_per_request: DEFAULT_MAX_TOOLS_PER_REQUEST,
            supports_json_mode: false,
            supports_system_message: true,
            supports_developer_message: true,
            max_parallel_tool_calls: 1,
            max_context_tokens: Some(DEFAULT_MAX_CONTEXT_TOKENS),
            max_output_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
        }
    }
}

/// 模型条目的显式能力声明（旧 probe 时代 config.json 的 `capabilities` 块）。
///
/// 全部字段可选：顶层配置字段优先，其次本声明，最后 `ProviderProtocolContract`
/// 默认值。`supports_reasoning`/`max_parallel_tool_calls` 只解析接受，当前
/// 契约没有对应消费点。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub struct ProviderCapabilityDeclaration {
    pub supports_tools: Option<bool>,
    pub supports_parallel_tool_calls: Option<bool>,
    pub supports_required_tool_choice: Option<bool>,
    pub supports_strict_tool_schema: Option<bool>,
    pub supports_json_mode: Option<bool>,
    pub supports_system_message: Option<bool>,
    pub supports_developer_message: Option<bool>,
    pub supports_reasoning: Option<bool>,
    pub max_tools_per_request: Option<u32>,
    pub max_parallel_tool_calls: Option<u32>,
    pub max_context_tokens: Option<u32>,
    pub max_output_tokens: Option<u32>,
}

/// `AgentLoop` 为完成请求提供的可选模型参数。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ModelPreferences {
    pub model_name: Option<String>,
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    pub max_output_tokens: Option<u32>,
    pub json_mode: bool,
}

/// 脱敏的模型提供方配置存在性信息；这里永不存储敏感信息。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ModelProviderConfig {
    pub provider_name: Option<String>,
    pub model_name: Option<String>,
    pub base_url_present: bool,
    pub api_key_present: bool,
}

/// 解析出有效模型提供方配置值的配置层。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderConfigSource {
    ProcessEnvironment,
    UserConfigFile,
}

impl ProviderConfigSource {
    /// 返回配置来源的稳定字符串。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ProcessEnvironment => "process_env",
            Self::UserConfigFile => "user_config",
        }
    }
}

/// 构建模型提供方前解析得到的来源和脱敏配置状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderConfigResolution {
    pub source: Option<ProviderConfigSource>,
    pub config: ModelProviderConfig,
}

/// 服务级模型提供方配置快照，包含脱敏状态和已初始化的模型提供方。
///
/// 只捕获一次，使 `AppServer` 报告和使用同一份配置，同时不暴露 API 密钥或其他原始环境值。
#[derive(Clone)]
pub struct ProviderConfigSnapshot {
    snapshot_id: String,
    source: Option<ProviderConfigSource>,
    redacted_config: ModelProviderConfig,
    configuration: ProviderConfigurationStatus,
    provider: Result<OpenAiProvider, ProviderError>,
    model_selection: Option<std::sync::Arc<config::ModelSelectionSnapshot>>,
}

/// 模型提供方初始化无法继续时报告的稳定阻塞类别。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ModelBlockerKind {
    RequiredEnvMissing,
    AuthenticationProviderError,
    BaseUrlNetworkError,
    ModelNameConfigError,
    ProviderRuntimeUnavailable,
}

impl ModelBlockerKind {
    /// 返回阻塞类别代码。
    pub fn code(&self) -> &'static str {
        match self {
            Self::RequiredEnvMissing => "required_env_missing",
            Self::AuthenticationProviderError => "authentication_provider_error",
            Self::BaseUrlNetworkError => "base_url_network_error",
            Self::ModelNameConfigError => "model_name_config_error",
            Self::ProviderRuntimeUnavailable => "provider_runtime_unavailable",
        }
    }

    /// 返回阻塞类别说明。
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::RequiredEnvMissing => "required env missing",
            Self::AuthenticationProviderError => "authentication/provider error",
            Self::BaseUrlNetworkError => "base_url/network error",
            Self::ModelNameConfigError => "model name/config error",
            Self::ProviderRuntimeUnavailable => "provider runtime unavailable",
        }
    }
}

/// 暴露给 `AppServer` 的脱敏模型提供方就绪状态和阻塞信息。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProviderConfigurationStatus {
    pub configured: bool,
    pub provider_name: Option<String>,
    pub model_name: Option<String>,
    pub api_key_status: String,
    pub base_url_status: String,
    pub blocker: Option<ModelBlockerKind>,
}

/// 从模型提供方完成中累积的令牌与成本计数器。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ModelUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    pub cached_input_tokens: u64,
    pub reasoning_tokens: u64,
    pub cost_estimate: Option<f64>,
    /// 原始 usage 对象是否存在；缺失时各计数为 0 且 `cost_estimate` 必须为
    /// None——不把"缺失"伪装成"零消费"。旧序列化数据（无此字段）按存在处理。
    #[serde(default = "default_usage_present")]
    pub usage_present: bool,
}

/// 旧版序列化数据无 `usage_present` 字段时按"存在"解释（保持历史语义）。
fn default_usage_present() -> bool {
    true
}

/// 模型侧请求或响应的校验错误和非致命警告。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ModelValidationResult {
    pub valid: bool,
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

/// 从模型提供方边界保留下来的具体失败类型。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ModelErrorKind {
    Cancelled,
    NetworkError,
    Timeout,
    RateLimited,
    ProviderOverloaded,
    AuthError,
    InvalidRequest,
    ContextLengthExceeded,
    BudgetExceeded,
    ToolCallParseError,
    JsonSchemaViolation,
    ContentFilter,
    UnsupportedCapability,
    UnknownProviderError,
}

/// 供调用方决定状态和恢复行为的较粗错误类别。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ModelErrorCategory {
    Cancelled,
    Authentication,
    Network,
    ModelConfiguration,
    InvalidRequest,
    ContextLengthExceeded,
    BudgetExceeded,
    ToolCallParse,
    JsonSchema,
    ContentFilter,
    UnsupportedCapability,
    ProviderUnavailable,
    UnknownProviderError,
}

/// 模型提供方请求或响应发生失败的阶段。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProviderErrorStage {
    ClientInitialization,
    RequestSend,
    ResponseStatus,
    ResponseBodyRead,
    ResponseJsonDecode,
    ResponseValidation,
    Cancelled,
}

/// 与模型或策略语义分开保留的传输层原因。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProviderTransportCategory {
    Timeout,
    Connect,
    Request,
    BodyRead,
    Unknown,
}

/// 带类型分类和清理后模型提供方诊断信息的模型错误。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ModelError {
    pub kind: ModelErrorKind,
    pub message: String,
    pub provider_name: Option<String>,
    pub model_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stage: Option<ProviderErrorStage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport_category: Option<ProviderTransportCategory>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_seconds: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_status: Option<u16>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub validation_errors: Vec<String>,
}

/// 模型错误中可以安全跨越模型提供方边界的诊断子集。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProviderDiagnostic {
    pub code: Option<String>,
    pub stage: Option<ProviderErrorStage>,
    pub transport_category: Option<ProviderTransportCategory>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_seconds: Option<u64>,
    pub http_status: Option<u16>,
    pub validation_errors: Vec<String>,
}

impl ModelError {
    /// 创建带稳定 kind 的模型错误。
    pub fn new(kind: ModelErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            provider_name: None,
            model_name: None,
            code: None,
            stage: None,
            transport_category: None,
            timeout_seconds: None,
            http_status: None,
            validation_errors: Vec::new(),
        }
    }

    /// 绑定 provider 名称。
    pub fn with_provider(mut self, provider_name: impl Into<String>) -> Self {
        self.provider_name = Some(provider_name.into());
        self
    }

    /// 绑定模型名称。
    pub fn with_model(mut self, model_name: impl Into<String>) -> Self {
        self.model_name = Some(model_name.into());
        self
    }

    /// 附加脱敏 provider 诊断。
    pub fn with_provider_diagnostic(
        mut self,
        code: impl Into<String>,
        stage: ProviderErrorStage,
    ) -> Self {
        self.code = Some(code.into());
        self.stage = Some(stage);
        self
    }

    /// 归类为公共模型错误类别。
    pub fn category(&self) -> ModelErrorCategory {
        classify_model_error(self)
    }

    /// 返回 provider 诊断的脱敏副本。
    pub fn provider_diagnostic(&self) -> ProviderDiagnostic {
        ProviderDiagnostic {
            code: self.code.clone(),
            stage: self.stage.clone(),
            transport_category: self.transport_category.clone(),
            timeout_seconds: self.timeout_seconds,
            http_status: self.http_status,
            validation_errors: self.validation_errors.clone(),
        }
    }
}

/// 将模型错误映射为稳定公共类别。
pub fn classify_model_error(error: &ModelError) -> ModelErrorCategory {
    contract::model_error_category(error)
}

/// 传给模型提供方的完整模型请求，包括可见 tool 和 tool 策略。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ModelTurnRequest {
    pub request_id: String,
    pub messages: Vec<ModelMessage>,
    pub tools: Vec<ModelToolSchema>,
    pub tool_choice: ToolChoicePolicy,
    pub model_preferences: ModelPreferences,
    /// Internal provider continuation state. It is deliberately omitted from
    /// all public/request schemas and is only consumed by the adapter.
    #[serde(skip)]
    #[schemars(skip)]
    pub provider_reasoning_history: Vec<ProviderReasoningReplay>,
}

impl ModelTurnRequest {
    /// 创建模型 turn 请求。
    pub fn new(request_id: impl Into<String>, messages: Vec<ModelMessage>) -> Self {
        Self {
            request_id: request_id.into(),
            messages,
            tools: Vec::new(),
            tool_choice: ToolChoicePolicy::default(),
            model_preferences: ModelPreferences::default(),
            provider_reasoning_history: Vec::new(),
        }
    }
}

/// 模型提供方 turn 产生了有效完成，还是未通过校验。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ModelTurnStatus {
    Success,
    Failed,
    Invalid,
}

/// 模型提供方完成结果及其配对的已解析 tool call、用量、校验和错误状态。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ModelTurnResponse {
    pub request_id: String,
    pub response_id: String,
    pub status: ModelTurnStatus,
    pub assistant_message: Option<ModelMessage>,
    pub tool_calls: Vec<ModelToolCall>,
    pub usage: ModelUsage,
    pub finish_reason: Option<String>,
    pub validation: Option<ModelValidationResult>,
    pub error: Option<ModelError>,
    pub provider_name: Option<String>,
    pub model_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_attempt_metadata: Option<ProviderAttemptMetadata>,
    /// 内部 opaque reasoning continuation state；never serialized to the
    /// app-server or trace/evidence projections.
    #[serde(skip)]
    #[schemars(skip)]
    pub provider_reasoning_history: Vec<ProviderReasoningReplay>,
}

impl ModelTurnResponse {
    /// 构造已完成的模型响应。
    pub fn completed(
        request_id: impl Into<String>,
        response_id: impl Into<String>,
        content: impl Into<String>,
    ) -> Self {
        Self {
            request_id: request_id.into(),
            response_id: response_id.into(),
            status: ModelTurnStatus::Success,
            assistant_message: Some(ModelMessage::text(ModelRole::Assistant, content)),
            tool_calls: Vec::new(),
            usage: ModelUsage::default(),
            finish_reason: None,
            validation: None,
            error: None,
            provider_name: None,
            model_name: None,
            provider_attempt_metadata: None,
            provider_reasoning_history: Vec::new(),
        }
    }
}

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
            ProviderApiProtocol::Declared | ProviderApiProtocol::OpenAiChatCompletions => {
                Self::Unsupported
            }
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
    /// Stable PromptAssembly parent occurrence bound by AgentLoop.
    pub parent_occurrence_id: Option<String>,
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
    fn zero() -> Self {
        Self {
            attempt_count: 0,
            retry_count: 0,
            latency_ms: 0,
            occurrences: Vec::new(),
        }
    }
}

/// `AgentLoop` 用于能力协商和完成请求的模型提供方边界。
pub trait Provider {
    /// 返回模型提供方声明的基线契约。
    fn protocol_contract(&self) -> ProviderProtocolContract;

    /// Report the typed stream capability for the protocol selected by this provider.
    ///
    /// Legacy providers default to unsupported, even if their unrelated protocol metadata uses
    /// the same enum values as the OpenAI-compatible adapter.
    fn streaming_capability(
        &self,
        _selected_protocol: ProviderApiProtocol,
    ) -> ProviderStreamingCapability {
        ProviderStreamingCapability::Unsupported
    }

    /// Stream normalized visible text when the selected protocol supports it.
    ///
    /// The callback never receives reasoning, raw provider payloads, or tool argument deltas.
    /// Providers without this protocol capability keep using `complete` unchanged.
    fn complete_stream(
        &self,
        _request: &ModelTurnRequest,
        _cancellation: &CancellationToken,
        _on_event: &mut dyn FnMut(ProviderStreamEvent),
    ) -> Result<ModelTurnResponse, ProviderError> {
        Err(provider_streaming_unsupported_error())
    }

    /// Stream visible text and expose each underlying HTTP attempt in real time.
    fn complete_stream_observed(
        &self,
        request: &ModelTurnRequest,
        cancellation: &CancellationToken,
        on_event: &mut dyn FnMut(ProviderStreamEvent),
        _on_attempt: &mut dyn FnMut(ProviderAttemptEvent) -> bool,
    ) -> Result<ModelTurnResponse, ProviderError> {
        self.complete_stream(request, cancellation, on_event)
    }

    /// 完成一个已校验请求，同时保留取消和类型化模型提供方错误。
    fn complete(
        &self,
        request: &ModelTurnRequest,
        cancellation: &CancellationToken,
    ) -> Result<ModelTurnResponse, ProviderError>;

    /// Complete a request and expose each underlying HTTP attempt in real time.
    fn complete_observed(
        &self,
        request: &ModelTurnRequest,
        cancellation: &CancellationToken,
        _on_attempt: &mut dyn FnMut(ProviderAttemptEvent) -> bool,
    ) -> Result<ModelTurnResponse, ProviderError> {
        self.complete(request, cancellation)
    }
}

/// 允许 `Arc<dyn Provider>` 作为透明代理，使测试可以注入动态 provider。
impl Provider for Arc<dyn Provider + Send + Sync> {
    fn protocol_contract(&self) -> ProviderProtocolContract {
        (**self).protocol_contract()
    }

    fn streaming_capability(
        &self,
        selected_protocol: ProviderApiProtocol,
    ) -> ProviderStreamingCapability {
        (**self).streaming_capability(selected_protocol)
    }

    fn complete_stream(
        &self,
        request: &ModelTurnRequest,
        cancellation: &CancellationToken,
        on_event: &mut dyn FnMut(ProviderStreamEvent),
    ) -> Result<ModelTurnResponse, ProviderError> {
        (**self).complete_stream(request, cancellation, on_event)
    }

    fn complete_stream_observed(
        &self,
        request: &ModelTurnRequest,
        cancellation: &CancellationToken,
        on_event: &mut dyn FnMut(ProviderStreamEvent),
        on_attempt: &mut dyn FnMut(ProviderAttemptEvent) -> bool,
    ) -> Result<ModelTurnResponse, ProviderError> {
        (**self).complete_stream_observed(request, cancellation, on_event, on_attempt)
    }

    fn complete(
        &self,
        request: &ModelTurnRequest,
        cancellation: &CancellationToken,
    ) -> Result<ModelTurnResponse, ProviderError> {
        (**self).complete(request, cancellation)
    }

    fn complete_observed(
        &self,
        request: &ModelTurnRequest,
        cancellation: &CancellationToken,
        on_attempt: &mut dyn FnMut(ProviderAttemptEvent) -> bool,
    ) -> Result<ModelTurnResponse, ProviderError> {
        (**self).complete_observed(request, cancellation, on_attempt)
    }
}

/// 已解析的兼容 OpenAI 连接设置；敏感信息仅为传输使用而保留。
#[derive(Clone, PartialEq, Eq)]
pub struct OpenAiProviderConfig {
    pub provider_name: String,
    pub model_name: String,
    pub base_url: String,
    pub api_key: String,
    pub source: ProviderConfigSource,
    pub max_context_tokens: Option<u32>,
    pub max_output_tokens: u32,
}

/// One fully resolved catalog selection.  Keeping the canonical variant,
/// enabled state and the single wire effort together prevents a second runtime
/// mapping table from silently changing the provider request.
#[derive(Clone)]
pub(crate) struct SelectedModel {
    pub(crate) model_name: String,
    pub(crate) api_protocol: ProviderApiProtocol,
    pub(crate) max_context_tokens: Option<u32>,
    pub(crate) max_output_tokens: u32,
    pub(crate) reasoning_variant: Option<String>,
    pub(crate) reasoning_enabled: bool,
    pub(crate) wire_reasoning_effort: Option<String>,
    pub(crate) thinking_wire_format: ThinkingWireFormat,
    pub(crate) tool_reasoning_mode: ProviderToolReasoningMode,
    pub(crate) supports_developer_role: bool,
    pub(crate) supports_tool_choice: bool,
    pub(crate) requires_reasoning_content_for_tool_calls: bool,
    pub(crate) requires_assistant_content_for_tool_calls: bool,
    /// 合并后的用户显式能力声明；协议契约构造时叠加到静态基线。
    pub(crate) capability_overrides: Option<ProviderCapabilityDeclaration>,
}

/// Provider transport runtime ownership: an app-server borrows its existing handle, while
/// independent consumers own a dedicated runtime shared by provider clones.
#[derive(Clone)]
enum ProviderRuntime {
    External(tokio::runtime::Handle),
    Owned(Arc<tokio::runtime::Runtime>),
}

impl ProviderRuntime {
    fn block_on<F: Future>(&self, future: F) -> F::Output {
        match self {
            Self::External(handle) => handle.block_on(future),
            Self::Owned(runtime) => runtime.block_on(future),
        }
    }
}

/// 协商能力并校验每次完成请求的兼容 OpenAI 模型提供方。
#[derive(Clone)]
pub struct OpenAiProvider {
    config: OpenAiProviderConfig,
    /// Explicit model-catalog clones carry one immutable, fully resolved selection.
    selected_model: Option<SelectedModel>,
    client: reqwest::Client,
    /// 所有 provider clone 共享同一 runtime ownership 绑定。
    runtime: Arc<ProviderRuntime>,
    request_timeout_seconds: u64,
}

impl fmt::Debug for OpenAiProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenAiProvider")
            .field("config", &self.config)
            .field("client", &"[redacted]")
            .field("runtime", &"[shared]")
            .field("request_timeout_seconds", &self.request_timeout_seconds)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Error)]
#[error("{message}")]
/// 模型提供方失败，包含类型化模型错误和尝试元数据。
pub struct ProviderError {
    pub message: String,
    pub error: Box<ModelError>,
    pub provider_attempt_metadata: Option<ProviderAttemptMetadata>,
}

impl ProviderError {
    /// 从模型错误创建 provider 错误。
    pub fn from_model_error(error: ModelError) -> Self {
        Self {
            message: error.message.clone(),
            error: Box::new(error),
            provider_attempt_metadata: None,
        }
    }

    /// 附加一次 provider attempt 的脱敏元数据。
    pub fn with_provider_attempt_metadata(mut self, metadata: ProviderAttemptMetadata) -> Self {
        self.provider_attempt_metadata = Some(metadata);
        self
    }
}

/// Build the stable unsupported result used by Chat, Declared, and legacy providers.
pub(crate) fn provider_streaming_unsupported_error() -> ProviderError {
    ProviderError::from_model_error(
        ModelError::new(
            ModelErrorKind::UnsupportedCapability,
            "provider streaming is unsupported for this protocol",
        )
        .with_provider_diagnostic(
            PROVIDER_STREAMING_UNSUPPORTED_CODE,
            ProviderErrorStage::ResponseValidation,
        ),
    )
}

#[cfg(test)]
mod transport_tests {
    use std::io::{BufRead, BufReader, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc::{self, Receiver};
    use std::thread;
    use std::time::Duration;

    use super::*;

    fn test_provider_config(base_url: String) -> OpenAiProviderConfig {
        OpenAiProviderConfig {
            provider_name: "openai_compatible".to_string(),
            model_name: "gpt-test".to_string(),
            base_url,
            api_key: "sk-secret-value".to_string(),
            source: ProviderConfigSource::ProcessEnvironment,
            max_context_tokens: Some(DEFAULT_MAX_CONTEXT_TOKENS),
            max_output_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
        }
    }

    fn read_test_provider_request(stream: &TcpStream) {
        let mut reader = BufReader::new(stream.try_clone().expect("clone provider stream"));
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .expect("read provider request line");
        assert!(line.contains("/v1/chat/completions"));
    }

    fn write_test_provider_response(stream: &mut TcpStream) {
        let body = r#"{"id":"response_1","choices":[{"message":{"role":"assistant","content":"done"},"finish_reason":"stop"}]}"#;
        write!(
            stream,
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            body.len(),
            body
        )
        .expect("write provider response");
    }

    fn concurrent_provider_server() -> (String, Receiver<usize>, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind concurrent provider");
        let address = listener.local_addr().expect("concurrent provider address");
        let (maximum_tx, maximum_rx) = mpsc::channel();
        let active = Arc::new(AtomicUsize::new(0));
        let maximum = Arc::new(AtomicUsize::new(0));
        let server = thread::spawn({
            let active = Arc::clone(&active);
            let maximum = Arc::clone(&maximum);
            move || {
                let mut workers = Vec::new();
                for _ in 0..2 {
                    let (mut stream, _) = listener.accept().expect("accept concurrent request");
                    let active = Arc::clone(&active);
                    let maximum = Arc::clone(&maximum);
                    workers.push(thread::spawn(move || {
                        read_test_provider_request(&stream);
                        let current = active.fetch_add(1, Ordering::SeqCst) + 1;
                        maximum.fetch_max(current, Ordering::SeqCst);
                        thread::sleep(Duration::from_millis(150));
                        write_test_provider_response(&mut stream);
                        active.fetch_sub(1, Ordering::SeqCst);
                    }));
                }
                for worker in workers {
                    worker.join().expect("join concurrent provider request");
                }
                maximum_tx
                    .send(maximum.load(Ordering::SeqCst))
                    .expect("send concurrent provider maximum");
            }
        });
        (format!("http://{address}"), maximum_rx, server)
    }

    fn cancellation_followup_server() -> (String, Receiver<()>, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind cancellation provider");
        let address = listener
            .local_addr()
            .expect("cancellation provider address");
        let (first_request_tx, first_request_rx) = mpsc::channel();
        let server = thread::spawn(move || {
            let (first_stream, _) = listener.accept().expect("accept cancelled request");
            read_test_provider_request(&first_stream);
            first_request_tx
                .send(())
                .expect("send cancelled request started");

            let (mut followup_stream, _) = listener.accept().expect("accept follow-up request");
            read_test_provider_request(&followup_stream);
            write_test_provider_response(&mut followup_stream);
            thread::sleep(Duration::from_millis(100));
        });
        (format!("http://{address}"), first_request_rx, server)
    }

    #[test]
    fn configured_deadline_is_reported_from_a_real_transport_timeout() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind hanging provider");
        let address = listener.local_addr().expect("provider address");
        let server = thread::spawn(move || {
            let mut streams = Vec::new();
            // 超时（挂起）不再重试：只接受 1 个连接。
            for _ in 0..1 {
                let (stream, _) = listener.accept().expect("accept provider request");
                let mut reader = BufReader::new(stream.try_clone().expect("clone provider stream"));
                let mut line = String::new();
                reader.read_line(&mut line).expect("read request line");
                streams.push(stream);
            }
            thread::sleep(Duration::from_secs(2));
        });
        let provider = OpenAiProvider::new_with_request_timeout(
            OpenAiProviderConfig {
                provider_name: "openai_compatible".to_string(),
                model_name: "gpt-test".to_string(),
                base_url: format!("http://{address}"),
                api_key: "sk-secret-value".to_string(),
                source: ProviderConfigSource::ProcessEnvironment,
                max_context_tokens: Some(DEFAULT_MAX_CONTEXT_TOKENS),
                max_output_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
            },
            1,
        )
        .expect("provider");
        let request = ModelTurnRequest::new(
            "request_timeout",
            vec![ModelMessage::text(ModelRole::User, "hello")],
        );

        let error = provider
            .complete(&request, &CancellationToken::new())
            .expect_err("provider request must time out");

        assert_eq!(error.error.kind, ModelErrorKind::Timeout);
        assert_eq!(
            error.error.code.as_deref(),
            Some("provider_request_send_failed")
        );
        assert_eq!(error.error.stage, Some(ProviderErrorStage::RequestSend));
        assert_eq!(
            error.error.transport_category,
            Some(ProviderTransportCategory::Timeout)
        );
        assert_eq!(error.error.timeout_seconds, Some(1));
        let metadata = error
            .provider_attempt_metadata
            .as_ref()
            .expect("timeout attempt metadata");
        // 超时（挂起）不再重试：单次 120s 超时即失败，避免 6 次重试拖 12 分钟。
        assert_eq!(metadata.attempt_count, 1);
        assert_eq!(metadata.retry_count, 0);
        assert_eq!(metadata.occurrences.len(), 1);
        for (index, occurrence) in metadata.occurrences.iter().enumerate() {
            assert_eq!(occurrence.attempt_index, index as u32 + 1);
            assert_eq!(occurrence.terminal_status, ProviderAttemptStatus::Error);
            assert_eq!(occurrence.error_category, Some(ModelErrorCategory::Network));
            assert_eq!(
                occurrence.error_stage,
                Some(ProviderErrorStage::RequestSend)
            );
            assert_eq!(
                occurrence.diagnostic_code.as_deref(),
                Some("provider_request_send_failed")
            );
            assert!(occurrence.request_send_to_headers_ms.is_none());
            assert!(!occurrence.retry_scheduled);
        }
        let serialized = serde_json::to_string(&error.error).expect("serialize timeout");
        for secret in ["sk-secret-value", &address.to_string(), "authorization"] {
            assert!(
                !serialized
                    .to_ascii_lowercase()
                    .contains(&secret.to_ascii_lowercase())
            );
        }
        server.join().expect("provider server");
    }

    #[test]
    fn oversized_success_body_is_rejected_before_buffering() {
        const OVERSIZED_RESPONSE_BYTES: usize = 8 * 1024 * 1024 + 1;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind oversized provider");
        let address = listener.local_addr().expect("provider address");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept provider request");
            let mut reader = BufReader::new(stream.try_clone().expect("clone provider stream"));
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).expect("read request");
                if line == "\r\n" || line.is_empty() {
                    break;
                }
            }
            write!(
                stream,
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {OVERSIZED_RESPONSE_BYTES}\r\nconnection: close\r\n\r\n"
            )
            .expect("write oversized response headers");
        });
        let provider = OpenAiProvider::new(OpenAiProviderConfig {
            provider_name: "openai_compatible".to_string(),
            model_name: "gpt-test".to_string(),
            base_url: format!("http://{address}"),
            api_key: "sk-secret-value".to_string(),
            source: ProviderConfigSource::ProcessEnvironment,
            max_context_tokens: Some(DEFAULT_MAX_CONTEXT_TOKENS),
            max_output_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
        })
        .expect("provider");
        let request = ModelTurnRequest::new(
            "request_oversized",
            vec![ModelMessage::text(ModelRole::User, "hello")],
        );

        let error = provider
            .complete(&request, &CancellationToken::new())
            .expect_err("oversized provider response must fail closed");

        assert_eq!(error.error.kind, ModelErrorKind::JsonSchemaViolation);
        assert_eq!(
            error.error.code.as_deref(),
            Some("provider_response_body_too_large")
        );
        assert_eq!(
            error.error.stage,
            Some(ProviderErrorStage::ResponseBodyRead)
        );
        assert_eq!(
            error.error.validation_errors,
            ["provider_response_body_too_large"]
        );
        let metadata = error
            .provider_attempt_metadata
            .as_ref()
            .expect("oversized response attempt metadata");
        assert_eq!(metadata.attempt_count, 1);
        assert_eq!(metadata.retry_count, 0);
        server.join().expect("provider server");
    }

    #[test]
    fn provider_clones_share_a_runtime_and_requests_progress_concurrently() {
        let (base_url, maximum_rx, server) = concurrent_provider_server();
        let provider = OpenAiProvider::new(test_provider_config(base_url)).expect("provider");
        let cloned = provider.clone();
        assert!(Arc::ptr_eq(&provider.runtime, &cloned.runtime));

        let provider = Arc::new(provider);
        let start = Arc::new(std::sync::Barrier::new(3));
        let (result_tx, result_rx) = mpsc::channel();
        let mut callers = Vec::new();
        for request_id in ["request_concurrent_a", "request_concurrent_b"] {
            let provider = Arc::clone(&provider);
            let start = Arc::clone(&start);
            let result_tx = result_tx.clone();
            callers.push(thread::spawn(move || {
                let request = ModelTurnRequest::new(
                    request_id,
                    vec![ModelMessage::text(ModelRole::User, "hello")],
                );
                start.wait();
                result_tx
                    .send(provider.complete(&request, &CancellationToken::new()))
                    .expect("send concurrent provider result");
            }));
        }
        start.wait();

        for _ in 0..2 {
            result_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("concurrent provider request completed")
                .expect("concurrent provider request succeeded");
        }
        for caller in callers {
            caller.join().expect("join concurrent provider caller");
        }
        assert_eq!(
            maximum_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("concurrent provider maximum"),
            2,
            "shared runtime must not serialize provider requests"
        );
        server.join().expect("join concurrent provider server");
    }

    #[test]
    fn cancelled_request_does_not_poison_followup_on_the_shared_runtime() {
        let (base_url, first_request_rx, server) = cancellation_followup_server();
        let provider = OpenAiProvider::new(test_provider_config(base_url)).expect("provider");
        let cancellation = CancellationToken::new();
        let worker_cancellation = cancellation.clone();
        let worker_provider = provider.clone();
        let worker = thread::spawn(move || {
            let request = ModelTurnRequest::new(
                "request_cancel_shared_runtime",
                vec![ModelMessage::text(ModelRole::User, "wait")],
            );
            worker_provider.complete(&request, &worker_cancellation)
        });

        first_request_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("cancelled request started");
        cancellation.cancel();
        let error = worker
            .join()
            .expect("join cancelled provider caller")
            .expect_err("cancelled request must fail");
        assert_eq!(error.error.kind, ModelErrorKind::Cancelled);

        let followup = ModelTurnRequest::new(
            "request_after_cancel_shared_runtime",
            vec![ModelMessage::text(ModelRole::User, "hello")],
        );
        provider
            .complete(&followup, &CancellationToken::new())
            .expect("follow-up request must still succeed");
        server.join().expect("join cancellation provider server");
    }

    #[test]
    fn timed_out_request_does_not_poison_followup_on_the_shared_runtime() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind timeout provider");
        let address = listener.local_addr().expect("timeout provider address");
        let server = thread::spawn(move || {
            let mut hanging_streams = Vec::new();
            // 超时（挂起）不再重试：只接受 1 个挂起连接。
            for _ in 0..1 {
                let (stream, _) = listener.accept().expect("accept timed-out request");
                read_test_provider_request(&stream);
                hanging_streams.push(stream);
            }

            let (mut followup_stream, _) = listener.accept().expect("accept timeout follow-up");
            read_test_provider_request(&followup_stream);
            write_test_provider_response(&mut followup_stream);
            drop(hanging_streams);
        });
        let provider = OpenAiProvider::new_with_request_timeout(
            test_provider_config(format!("http://{address}")),
            1,
        )
        .expect("provider");
        let timed_out_request = ModelTurnRequest::new(
            "request_timeout_shared_runtime",
            vec![ModelMessage::text(ModelRole::User, "wait")],
        );
        let error = provider
            .complete(&timed_out_request, &CancellationToken::new())
            .expect_err("provider request must time out");
        assert_eq!(error.error.kind, ModelErrorKind::Timeout);
        assert_eq!(
            error
                .provider_attempt_metadata
                .as_ref()
                .expect("timeout metadata")
                .attempt_count,
            // 超时（挂起）不再重试：单次超时即失败。
            1
        );

        let followup = ModelTurnRequest::new(
            "request_after_timeout_shared_runtime",
            vec![ModelMessage::text(ModelRole::User, "hello")],
        );
        provider
            .complete(&followup, &CancellationToken::new())
            .expect("follow-up request must still succeed");
        server.join().expect("join timeout provider server");
    }

    #[test]
    fn streaming_response_timeout_is_idle_not_total() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind slow streaming provider");
        let address = listener
            .local_addr()
            .expect("slow streaming provider address");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept streaming request");
            let mut reader = BufReader::new(stream.try_clone().expect("clone streaming stream"));
            let mut line = String::new();
            loop {
                line.clear();
                reader.read_line(&mut line).expect("read streaming request");
                if line == "\r\n" || line.is_empty() {
                    break;
                }
            }
            write!(
                stream,
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n"
            )
            .expect("write streaming headers");
            let events = [
                "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\",\"status\":\"in_progress\",\"output\":[]}}\n\n",
                "data: {\"type\":\"response.output_text.delta\",\"delta\":\"do\"}\n\n",
                "data: {\"type\":\"response.output_text.delta\",\"delta\":\"ne\"}\n\n",
                "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"status\":\"completed\",\"output\":[{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"done\"}]}],\"usage\":{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}}\n\n",
            ];
            for (index, event) in events.iter().enumerate() {
                if index > 0 {
                    thread::sleep(Duration::from_millis(400));
                }
                write!(stream, "{:X}\r\n{}\r\n", event.len(), event)
                    .expect("write streaming event");
                stream.flush().expect("flush streaming event");
            }
            write!(stream, "0\r\n\r\n").expect("write streaming terminator");
        });

        let provider = OpenAiProvider::new_with_request_timeout(
            test_provider_config(format!("http://{address}/v1")),
            1,
        )
        .expect("provider")
        .with_selected_model(SelectedModel {
            model_name: "gpt-test".to_string(),
            api_protocol: ProviderApiProtocol::OpenAiResponses,
            max_context_tokens: Some(DEFAULT_MAX_CONTEXT_TOKENS),
            max_output_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
            reasoning_variant: None,
            reasoning_enabled: false,
            wire_reasoning_effort: None,
            thinking_wire_format: ThinkingWireFormat::ThinkingType,
            tool_reasoning_mode: ProviderToolReasoningMode::DisabledForToolCalls,
            supports_developer_role: true,
            supports_tool_choice: true,
            requires_reasoning_content_for_tool_calls: false,
            requires_assistant_content_for_tool_calls: false,
            capability_overrides: None,
        });
        let request = ModelTurnRequest::new(
            "slow_streaming_response",
            vec![ModelMessage::text(ModelRole::User, "hello")],
        );

        let mut events = Vec::new();
        let response = provider
            .complete_stream(&request, &CancellationToken::new(), &mut |event| {
                events.push(event);
            })
            .expect("streaming response must survive a total duration above the idle timeout");

        assert_eq!(events.len(), 2);
        assert_eq!(
            response
                .assistant_message
                .as_ref()
                .expect("assistant message")
                .content,
            "done"
        );
        server.join().expect("join slow streaming provider");
    }

    #[test]
    fn runtime_initialization_failure_maps_to_a_stable_provider_blocker() {
        let error = provider_runtime_error(std::io::Error::other(
            "synthetic runtime initialization failure",
        ));
        assert_eq!(
            error.error.code.as_deref(),
            Some(PROVIDER_RUNTIME_INITIALIZATION_ERROR_CODE)
        );
        assert_eq!(
            provider_initialization_blocker(&error.error),
            Some(ModelBlockerKind::ProviderRuntimeUnavailable)
        );
    }

    /// 契约透传：选中模型的 tool_reasoning_mode 必须反映到 protocol_contract()
    /// （修复前硬编码 Unspecified，agent 侧续接投影永远被跳过）。
    #[test]
    fn protocol_contract_exposes_selected_tool_reasoning_mode() {
        let config = test_provider_config("http://127.0.0.1:1/v1".to_string());
        let provider = OpenAiProvider::new(config)
            .expect("provider")
            .with_selected_model(SelectedModel {
                model_name: "gpt-test".to_string(),
                api_protocol: ProviderApiProtocol::OpenAiChatCompletions,
                max_context_tokens: Some(DEFAULT_MAX_CONTEXT_TOKENS),
                max_output_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
                reasoning_variant: Some("on".to_string()),
                reasoning_enabled: true,
                wire_reasoning_effort: None,
                thinking_wire_format: ThinkingWireFormat::ThinkingType,
                tool_reasoning_mode: ProviderToolReasoningMode::ReplayReasoningContent,
                supports_developer_role: true,
                supports_tool_choice: true,
                requires_reasoning_content_for_tool_calls: true,
                requires_assistant_content_for_tool_calls: false,
                capability_overrides: None,
            });
        assert_eq!(
            provider.protocol_contract().tool_reasoning_mode,
            ProviderToolReasoningMode::ReplayReasoningContent
        );
    }
}
