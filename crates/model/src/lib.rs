#![forbid(unsafe_code)]

//! Model-facing messages, provider capability contracts, and the OpenAI-compatible transport.
//!
//! Provider negotiation and validation live at this boundary so the AgentLoop can execute only
//! requests and tool calls that the selected provider has declared or probed.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use singularity_core::CancellationToken;
use thiserror::Error;
use uuid::Uuid;

const DEFAULT_MAX_TOOL_CALLS: u32 = 1;
pub const DEFAULT_MAX_TOOLS_PER_REQUEST: u32 = 8;
pub const DEFAULT_MAX_CONTEXT_TOKENS: u32 = 128_000;
pub const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 4_096;
const MAX_CONFIGURED_CONTEXT_TOKENS: u32 = 2_000_000;
const MAX_CONFIGURED_OUTPUT_TOKENS: u32 = 256_000;
const ENV_PROVIDER: &str = "SINGULARITY_MODEL_PROVIDER";
const ENV_MODEL: &str = "SINGULARITY_MODEL";
const ENV_CONTEXT_TOKENS: &str = "SINGULARITY_MODEL_CONTEXT_TOKENS";
const ENV_MAX_OUTPUT_TOKENS: &str = "SINGULARITY_MODEL_MAX_OUTPUT_TOKENS";
const ENV_BASE_URL: &str = "SINGULARITY_BASE_URL";
const ENV_API_KEY: &str = "SINGULARITY_API_KEY";
const PROJECT_ENV_FILE: &str = ".env";
const DEFAULT_PROVIDER_NAME: &str = "openai_compatible";
const CHAT_COMPLETIONS_PATH: &str = "/chat/completions";
const V1_CHAT_COMPLETIONS_PATH: &str = "/v1/chat/completions";
const RESPONSES_PATH: &str = "/responses";
const V1_RESPONSES_PATH: &str = "/v1/responses";
const PROVIDER_TIMEOUT_SECONDS: u64 = 120;
const PROVIDER_CANCELLATION_POLL_MS: u64 = 25;
const MAX_PROVIDER_RESPONSE_BODY_BYTES: usize = 8 * 1024 * 1024;
const MAX_PROVIDER_ATTEMPTS: u32 = 3;
const PROVIDER_RETRY_BASE_BACKOFF_MS: u64 = 50;
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
const HTTP_STATUS_BAD_REQUEST: u16 = 400;
const HTTP_STATUS_UNPROCESSABLE_ENTITY: u16 = 422;
const HTTP_STATUS_RATE_LIMITED: u16 = 429;
const HTTP_STATUS_INTERNAL_SERVER_ERROR: u16 = 500;
const CAPABILITY_PROBE_REQUEST_ID: &str = "singularity_capability_probe";
const CAPABILITY_PROBE_CONTINUATION_REQUEST_ID: &str = "singularity_capability_probe_continuation";
const CAPABILITY_PROBE_ROUTER: &str = "singularity_capability_router";
const CAPABILITY_PROBE_TOOL_A: &str = "singularity_capability_probe_a";
const CAPABILITY_PROBE_TOOL_B: &str = "singularity_capability_probe_b";
const CAPABILITY_PROBE_EXPECTED_LABEL: &str = "schema_sentinel_alpha";
const CAPABILITY_PROBE_ALTERNATE_LABEL: &str = "schema_sentinel_beta";
const CAPABILITY_PROBE_EXPECTED_VALUE: i64 = 7;
const CAPABILITY_PROBE_DEVELOPER_INSTRUCTION: &str =
    "Follow the fixed capability probe request using native structured tool calls.";

/// The roles supported by the model-facing conversation history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ModelRole {
    System,
    Developer,
    User,
    Assistant,
    Tool,
}

/// A provider-facing message, including tool-call metadata needed to continue a turn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ModelMessage {
    pub role: ModelRole,
    pub content: String,
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ModelToolCall>,
}

impl ModelMessage {
    pub fn text(role: ModelRole, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
            tool_call_id: None,
            tool_calls: Vec::new(),
        }
    }

    pub fn assistant_tool_calls(tool_calls: Vec<ModelToolCall>) -> Self {
        Self {
            role: ModelRole::Assistant,
            content: String::new(),
            tool_call_id: None,
            tool_calls,
        }
    }
}

/// Controls whether the provider may choose tools, must choose one, or must not call tools.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ToolChoiceMode {
    Auto,
    None,
    Required,
}

/// Tool-selection limits and schema strictness applied to one model request.
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
pub enum ModelToolParseStatus {
    Valid,
    InvalidJson,
    SchemaMismatch,
    UnknownTool,
}

/// The provider-visible schema for one executable tool.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ModelToolSchema {
    pub name: String,
    pub description: String,
    pub parameters_schema: Value,
}

/// A parsed model tool call together with raw arguments and validation outcome.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ModelToolCall {
    pub tool_call_id: String,
    pub tool_name: String,
    pub arguments: Value,
    pub raw_arguments: String,
    pub parse_status: ModelToolParseStatus,
    pub validation_errors: Vec<String>,
}

/// Whether tool reasoning content is compatible with the provider's tool-call history contract.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProviderToolReasoningMode {
    #[default]
    Unspecified,
    DisabledForToolCalls,
}

/// Describes whether tools are sent directly or through a provider-facing router envelope.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProviderToolDefinitionMode {
    #[default]
    Direct,
    Routed,
}

/// The wire protocol selected for a provider completion request.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProviderApiProtocol {
    #[default]
    Declared,
    OpenAiResponses,
    OpenAiChatCompletions,
}

/// Capabilities that the provider must honor for request construction and response validation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProviderProtocolContract {
    pub supports_tools: bool,
    pub supports_parallel_tool_calls: bool,
    pub supports_required_tool_choice: bool,
    pub supports_strict_tool_schema: bool,
    pub tool_reasoning_mode: ProviderToolReasoningMode,
    pub tool_definition_mode: ProviderToolDefinitionMode,
    pub max_tools_per_request: u32,
    pub supports_json_mode: bool,
    pub supports_system_message: bool,
    pub supports_developer_message: bool,
    pub max_context_tokens: u32,
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
            tool_definition_mode: ProviderToolDefinitionMode::Direct,
            max_tools_per_request: DEFAULT_MAX_TOOLS_PER_REQUEST,
            supports_json_mode: false,
            supports_system_message: true,
            supports_developer_message: true,
            max_context_tokens: DEFAULT_MAX_CONTEXT_TOKENS,
            max_output_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
        }
    }
}

/// The negotiated provider profile used for diagnostics and later request validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProviderCapabilityProfile {
    Declared,
    StrictParallel,
    StrictSingle,
    NonStrictParallel,
    NonStrictSingle,
    RoutedSingle,
}

/// Optional model parameters supplied by the AgentLoop for a completion request.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ModelPreferences {
    pub model_name: Option<String>,
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    pub max_output_tokens: Option<u32>,
    pub json_mode: bool,
}

/// Redacted provider configuration presence information; secrets are never stored here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ModelProviderConfig {
    pub provider_name: Option<String>,
    pub model_name: Option<String>,
    pub base_url_present: bool,
    pub api_key_present: bool,
}

/// The configuration layer from which effective provider values were resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderConfigSource {
    ProcessEnvironment,
    ProjectEnvFile,
}

impl ProviderConfigSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ProcessEnvironment => "process_env",
            Self::ProjectEnvFile => "project_env",
        }
    }
}

/// Resolved provider source and redacted configuration status before provider construction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderConfigResolution {
    pub source: Option<ProviderConfigSource>,
    pub config: ModelProviderConfig,
}

/// A server-scoped provider configuration snapshot with redacted status and initialized provider.
///
/// Capturing once lets the AppServer report and use the same configuration without exposing the
/// API key or other raw environment values.
#[derive(Clone)]
pub struct ProviderConfigSnapshot {
    snapshot_id: String,
    source: Option<ProviderConfigSource>,
    redacted_config: ModelProviderConfig,
    configuration: ProviderConfigurationStatus,
    provider: Result<OpenAiProvider, ProviderError>,
}

impl fmt::Debug for ProviderConfigSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderConfigSnapshot")
            .field("snapshot_id", &self.snapshot_id)
            .field("source", &self.source)
            .field("redacted_config", &self.redacted_config)
            .field("configuration", &self.configuration)
            .finish()
    }
}

impl ProviderConfigSnapshot {
    pub fn capture<F>(get_env: F) -> Self
    where
        F: FnMut(&str) -> Option<String>,
    {
        let project_dir = std::env::current_dir().ok();
        let values = resolve_provider_values(get_env, project_dir.as_deref());
        let source = values.source;
        let redacted_config = provider_config_resolution(&values).config;
        let provider =
            OpenAiProviderConfig::from_resolved_values(values).and_then(OpenAiProvider::new);
        let mut configuration = ProviderConfigurationStatus::from_config(&redacted_config);
        if configuration.configured
            && let Err(error) = &provider
        {
            configuration.configured = false;
            configuration.blocker = provider_initialization_blocker(&error.error.category());
        }
        Self {
            snapshot_id: format!("{PROVIDER_SNAPSHOT_ID_PREFIX}{}", Uuid::new_v4().simple()),
            source,
            redacted_config,
            configuration,
            provider,
        }
    }

    pub fn source(&self) -> Option<ProviderConfigSource> {
        self.source
    }

    pub fn redacted_config(&self) -> &ModelProviderConfig {
        &self.redacted_config
    }

    pub fn configuration(&self) -> &ProviderConfigurationStatus {
        &self.configuration
    }

    pub fn snapshot_id(&self) -> &str {
        &self.snapshot_id
    }

    pub fn provider(&self) -> Result<OpenAiProvider, ProviderError> {
        self.provider.clone()
    }
}

/// A stable blocker category reported when provider setup cannot proceed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ModelBlockerKind {
    RequiredEnvMissing,
    AuthenticationProviderError,
    BaseUrlNetworkError,
    ModelNameConfigError,
}

impl ModelBlockerKind {
    pub fn code(&self) -> &'static str {
        match self {
            Self::RequiredEnvMissing => "required_env_missing",
            Self::AuthenticationProviderError => "authentication_provider_error",
            Self::BaseUrlNetworkError => "base_url_network_error",
            Self::ModelNameConfigError => "model_name_config_error",
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::RequiredEnvMissing => "required env missing",
            Self::AuthenticationProviderError => "authentication/provider error",
            Self::BaseUrlNetworkError => "base_url/network error",
            Self::ModelNameConfigError => "model name/config error",
        }
    }
}

fn provider_initialization_blocker(category: &ModelErrorCategory) -> Option<ModelBlockerKind> {
    match category {
        ModelErrorCategory::Authentication => Some(ModelBlockerKind::AuthenticationProviderError),
        ModelErrorCategory::Network | ModelErrorCategory::ProviderUnavailable => {
            Some(ModelBlockerKind::BaseUrlNetworkError)
        }
        ModelErrorCategory::ModelConfiguration
        | ModelErrorCategory::InvalidRequest
        | ModelErrorCategory::UnsupportedCapability => Some(ModelBlockerKind::ModelNameConfigError),
        ModelErrorCategory::Cancelled
        | ModelErrorCategory::ContextLengthExceeded
        | ModelErrorCategory::BudgetExceeded
        | ModelErrorCategory::ToolCallParse
        | ModelErrorCategory::JsonSchema
        | ModelErrorCategory::ContentFilter
        | ModelErrorCategory::UnknownProviderError => None,
    }
}

/// Redacted provider readiness and blocker information exposed to the AppServer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProviderConfigurationStatus {
    pub configured: bool,
    pub provider_name: Option<String>,
    pub model_name: Option<String>,
    pub api_key_status: String,
    pub base_url_status: String,
    pub blocker: Option<ModelBlockerKind>,
}

impl ProviderConfigurationStatus {
    pub fn from_config(config: &ModelProviderConfig) -> Self {
        let validation = validate_provider_config(config);
        Self {
            configured: validation.valid,
            provider_name: config.provider_name.clone(),
            model_name: config.model_name.clone(),
            api_key_status: redacted_presence(config.api_key_present),
            base_url_status: redacted_presence(config.base_url_present),
            blocker: if validation.valid {
                None
            } else {
                Some(ModelBlockerKind::RequiredEnvMissing)
            },
        }
    }
}

/// Token and cost counters accumulated from provider completions and capability probes.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ModelUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    pub cached_input_tokens: u64,
    pub reasoning_tokens: u64,
    pub cost_estimate: Option<f64>,
}

/// Sanitized evidence describing a capability probe and the selected protocol profile.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ProviderCapabilityMetadata {
    pub api_protocol: ProviderApiProtocol,
    pub profile: ProviderCapabilityProfile,
    pub cache_hit: bool,
    pub profile_attempts: u32,
    pub fallback_count: u32,
    pub probe_usage: ModelUsage,
    pub probe_attempt_metadata: ProviderAttemptMetadata,
}

/// The contract and diagnostics returned by provider capability negotiation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ProviderProtocolNegotiation {
    pub contract: ProviderProtocolContract,
    pub metadata: ProviderCapabilityMetadata,
}

/// Validation errors and non-fatal warnings for a model-side request or response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ModelValidationResult {
    pub valid: bool,
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

impl ModelValidationResult {
    pub fn valid() -> Self {
        Self {
            valid: true,
            errors: Vec::new(),
            warnings: Vec::new(),
        }
    }

    pub fn invalid(errors: Vec<String>) -> Self {
        Self {
            valid: false,
            errors,
            warnings: Vec::new(),
        }
    }
}

/// Specific failure kinds preserved from the provider boundary.
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

/// Coarser error categories used by callers to decide status and recovery behavior.
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

/// Stage at which a provider request or response failed.
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

/// Transport-level cause retained separately from model or policy semantics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProviderTransportCategory {
    Timeout,
    Connect,
    Request,
    BodyRead,
    Unknown,
}

/// A model error with typed classification and sanitized provider diagnostics.
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

/// The diagnostic subset of a model error that can cross the provider boundary safely.
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

    pub fn with_provider(mut self, provider_name: impl Into<String>) -> Self {
        self.provider_name = Some(provider_name.into());
        self
    }

    pub fn with_model(mut self, model_name: impl Into<String>) -> Self {
        self.model_name = Some(model_name.into());
        self
    }

    pub fn with_provider_diagnostic(
        mut self,
        code: impl Into<String>,
        stage: ProviderErrorStage,
    ) -> Self {
        self.code = Some(code.into());
        self.stage = Some(stage);
        self
    }

    pub fn category(&self) -> ModelErrorCategory {
        classify_model_error(self)
    }

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

pub fn classify_model_error(error: &ModelError) -> ModelErrorCategory {
    model_error_category(error)
}

/// The complete model request passed to a provider, including visible tools and tool policy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ModelTurnRequest {
    pub request_id: String,
    pub messages: Vec<ModelMessage>,
    pub tools: Vec<ModelToolSchema>,
    pub tool_choice: ToolChoicePolicy,
    pub model_preferences: ModelPreferences,
}

impl ModelTurnRequest {
    pub fn new(request_id: impl Into<String>, messages: Vec<ModelMessage>) -> Self {
        Self {
            request_id: request_id.into(),
            messages,
            tools: Vec::new(),
            tool_choice: ToolChoicePolicy::default(),
            model_preferences: ModelPreferences::default(),
        }
    }
}

/// Whether a provider turn produced a valid completion or failed validation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ModelTurnStatus {
    Success,
    Failed,
    Invalid,
}

/// A provider completion paired with parsed tool calls, usage, validation, and error state.
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
}

impl ModelTurnResponse {
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
        }
    }
}

/// Attempt and retry counts recorded for one provider operation.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProviderAttemptMetadata {
    pub attempt_count: u32,
    pub retry_count: u32,
    pub latency_ms: u64,
}

impl ProviderAttemptMetadata {
    fn zero() -> Self {
        Self {
            attempt_count: 0,
            retry_count: 0,
            latency_ms: 0,
        }
    }
}

impl ProviderCapabilityMetadata {
    fn declared() -> Self {
        Self {
            api_protocol: ProviderApiProtocol::Declared,
            profile: ProviderCapabilityProfile::Declared,
            cache_hit: false,
            profile_attempts: 0,
            fallback_count: 0,
            probe_usage: ModelUsage::default(),
            probe_attempt_metadata: ProviderAttemptMetadata::zero(),
        }
    }
}

impl ProviderProtocolNegotiation {
    fn declared(contract: ProviderProtocolContract) -> Self {
        Self {
            contract,
            metadata: ProviderCapabilityMetadata::declared(),
        }
    }
}

/// Provider boundary used by the AgentLoop for capability negotiation and completion.
pub trait Provider {
    /// Returns the provider's declared baseline contract before dynamic negotiation.
    fn protocol_contract(&self) -> ProviderProtocolContract;

    /// Probes or resolves tool capabilities before tool-bearing requests are sent.
    fn negotiate_tool_capabilities(
        &self,
        _model_preferences: &ModelPreferences,
        _cancellation: &CancellationToken,
    ) -> Result<ProviderProtocolNegotiation, ProviderError> {
        Ok(ProviderProtocolNegotiation::declared(
            self.protocol_contract(),
        ))
    }

    /// Completes one validated request while preserving cancellation and typed provider errors.
    fn complete(
        &self,
        request: &ModelTurnRequest,
        cancellation: &CancellationToken,
    ) -> Result<ModelTurnResponse, ProviderError>;
}

/// Resolved OpenAI-compatible connection settings with secrets retained only for transport use.
#[derive(Clone, PartialEq, Eq)]
pub struct OpenAiProviderConfig {
    pub provider_name: String,
    pub model_name: String,
    pub base_url: String,
    pub api_key: String,
    pub source: ProviderConfigSource,
    pub max_context_tokens: u32,
    pub max_output_tokens: u32,
}

impl fmt::Debug for OpenAiProviderConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenAiProviderConfig")
            .field("provider_name", &self.provider_name)
            .field("model_name", &self.model_name)
            .field("base_url", &"[redacted]")
            .field("api_key", &"[redacted]")
            .field("source", &self.source)
            .field("max_context_tokens", &self.max_context_tokens)
            .field("max_output_tokens", &self.max_output_tokens)
            .finish()
    }
}

impl OpenAiProviderConfig {
    pub fn from_env<F>(get_env: F) -> Result<Self, ProviderError>
    where
        F: FnMut(&str) -> Option<String>,
    {
        let project_dir = std::env::current_dir().ok();
        Self::from_resolved_values(resolve_provider_values(get_env, project_dir.as_deref()))
    }

    fn from_resolved_values(values: ResolvedProviderValues) -> Result<Self, ProviderError> {
        let source = values.source;
        let max_context_tokens = parse_provider_limit(
            values.context_tokens.as_deref(),
            ENV_CONTEXT_TOKENS,
            DEFAULT_MAX_CONTEXT_TOKENS,
            MAX_CONFIGURED_CONTEXT_TOKENS,
            source,
        )?;
        let max_output_tokens = parse_provider_limit(
            values.max_output_tokens.as_deref(),
            ENV_MAX_OUTPUT_TOKENS,
            DEFAULT_MAX_OUTPUT_TOKENS,
            MAX_CONFIGURED_OUTPUT_TOKENS,
            source,
        )?;
        if max_output_tokens >= max_context_tokens {
            return Err(ProviderError::from_model_error(
                ModelError::new(
                    ModelErrorKind::InvalidRequest,
                    format!(
                        "invalid model configuration: {ENV_MAX_OUTPUT_TOKENS} must be smaller than {ENV_CONTEXT_TOKENS}"
                    ),
                )
                .with_provider_diagnostic(
                    "provider_configuration_invalid",
                    ProviderErrorStage::ClientInitialization,
                ),
            ));
        }
        let provider_name = values
            .provider_name
            .unwrap_or_else(|| DEFAULT_PROVIDER_NAME.to_string());
        let model_name = values
            .model_name
            .ok_or_else(|| missing_provider_config_error(ENV_MODEL, source))?;
        let base_url = values
            .base_url
            .ok_or_else(|| missing_provider_config_error(ENV_BASE_URL, source))?;
        let api_key = values
            .api_key
            .ok_or_else(|| missing_provider_config_error(ENV_API_KEY, source))?;
        let source = source.ok_or_else(provider_source_missing_error)?;
        Ok(Self {
            provider_name,
            model_name,
            base_url,
            api_key,
            source,
            max_context_tokens,
            max_output_tokens,
        })
    }

    pub fn redacted_status(&self) -> ProviderConfigurationStatus {
        ProviderConfigurationStatus::from_config(&ModelProviderConfig {
            provider_name: Some(self.provider_name.clone()),
            model_name: Some(self.model_name.clone()),
            base_url_present: true,
            api_key_present: true,
        })
    }

    pub fn endpoint(&self) -> String {
        chat_completions_endpoint(&self.base_url)
    }

    fn api_protocol_candidates(&self) -> Vec<ProviderApiProtocol> {
        let base_url = self.base_url.trim().trim_end_matches('/');
        if base_url.ends_with(RESPONSES_PATH) {
            vec![ProviderApiProtocol::OpenAiResponses]
        } else if base_url.ends_with(CHAT_COMPLETIONS_PATH) {
            vec![ProviderApiProtocol::OpenAiChatCompletions]
        } else {
            vec![
                ProviderApiProtocol::OpenAiResponses,
                ProviderApiProtocol::OpenAiChatCompletions,
            ]
        }
    }

    fn completion_protocol_without_tools(&self) -> ProviderApiProtocol {
        if self
            .base_url
            .trim()
            .trim_end_matches('/')
            .ends_with(RESPONSES_PATH)
        {
            ProviderApiProtocol::OpenAiResponses
        } else {
            ProviderApiProtocol::OpenAiChatCompletions
        }
    }

    pub fn protocol_contract(&self) -> ProviderProtocolContract {
        ProviderProtocolContract {
            supports_tools: true,
            supports_parallel_tool_calls: false,
            supports_required_tool_choice: false,
            supports_strict_tool_schema: false,
            tool_reasoning_mode: ProviderToolReasoningMode::Unspecified,
            tool_definition_mode: ProviderToolDefinitionMode::Direct,
            max_tools_per_request: DEFAULT_MAX_TOOLS_PER_REQUEST,
            supports_json_mode: false,
            supports_system_message: false,
            supports_developer_message: false,
            max_context_tokens: self.max_context_tokens,
            max_output_tokens: self.max_output_tokens,
        }
    }
}

/// OpenAI-compatible provider that negotiates capabilities and validates every completion.
#[derive(Clone)]
pub struct OpenAiProvider {
    config: OpenAiProviderConfig,
    client: reqwest::Client,
    request_timeout_seconds: u64,
    tool_capability_cache: Arc<Mutex<HashMap<String, ProviderProtocolNegotiation>>>,
    tool_capability_probe_in_flight: Arc<Mutex<HashMap<String, Arc<CapabilityProbeState>>>>,
}

impl fmt::Debug for OpenAiProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenAiProvider")
            .field("config", &self.config)
            .field("client", &"[redacted]")
            .field("tool_capability_cache", &"[redacted]")
            .field("tool_capability_probe_in_flight", &"[redacted]")
            .finish()
    }
}

#[derive(Clone)]
enum CapabilityProbeCompletion {
    Result(Result<ProviderProtocolNegotiation, ProviderError>),
    OwnerCancelled,
}

struct CapabilityProbeState {
    completion: Mutex<Option<CapabilityProbeCompletion>>,
    wake: Condvar,
}

impl CapabilityProbeState {
    fn new() -> Self {
        Self {
            completion: Mutex::new(None),
            wake: Condvar::new(),
        }
    }

    fn wait(
        &self,
        cancellation: &CancellationToken,
    ) -> Result<CapabilityProbeCompletion, ProviderError> {
        let mut completion = self
            .completion
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        loop {
            if let Some(completion) = completion.as_ref() {
                return Ok(completion.clone());
            }
            if cancellation.is_cancelled() {
                return Err(capability_probe_cancelled_error());
            }
            completion = match self.wake.wait_timeout(
                completion,
                Duration::from_millis(PROVIDER_CANCELLATION_POLL_MS),
            ) {
                Ok((completion, _)) => completion,
                Err(poisoned) => poisoned.into_inner().0,
            };
        }
    }

    fn complete(&self, completion: CapabilityProbeCompletion) {
        let mut current = self
            .completion
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if current.is_none() {
            *current = Some(completion);
        }
        self.wake.notify_all();
    }
}

struct CapabilityProbeOwnerGuard {
    in_flight: Arc<Mutex<HashMap<String, Arc<CapabilityProbeState>>>>,
    model_name: String,
    state: Arc<CapabilityProbeState>,
    armed: bool,
}

impl CapabilityProbeOwnerGuard {
    fn new(
        in_flight: Arc<Mutex<HashMap<String, Arc<CapabilityProbeState>>>>,
        model_name: String,
        state: Arc<CapabilityProbeState>,
    ) -> Self {
        Self {
            in_flight,
            model_name,
            state,
            armed: true,
        }
    }

    fn finish(mut self, completion: CapabilityProbeCompletion) {
        self.state.complete(completion);
        self.remove_state();
        self.armed = false;
    }

    fn remove_state(&self) {
        let mut in_flight = self
            .in_flight
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if in_flight
            .get(&self.model_name)
            .is_some_and(|current| Arc::ptr_eq(current, &self.state))
        {
            in_flight.remove(&self.model_name);
        }
    }
}

impl Drop for CapabilityProbeOwnerGuard {
    fn drop(&mut self) {
        if self.armed {
            self.state
                .complete(CapabilityProbeCompletion::OwnerCancelled);
            self.remove_state();
        }
    }
}

impl OpenAiProvider {
    pub fn new(config: OpenAiProviderConfig) -> Result<Self, ProviderError> {
        Self::new_with_request_timeout(config, PROVIDER_TIMEOUT_SECONDS)
    }

    fn new_with_request_timeout(
        config: OpenAiProviderConfig,
        request_timeout_seconds: u64,
    ) -> Result<Self, ProviderError> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(request_timeout_seconds))
            .build()
            .map_err(provider_client_initialization_error)?;
        Ok(Self {
            config,
            client,
            request_timeout_seconds,
            tool_capability_cache: Arc::new(Mutex::new(HashMap::new())),
            tool_capability_probe_in_flight: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    pub fn from_env<F>(get_env: F) -> Result<Self, ProviderError>
    where
        F: FnMut(&str) -> Option<String>,
    {
        Self::new(OpenAiProviderConfig::from_env(get_env)?)
    }

    fn cached_tool_capability_negotiation(
        &self,
        model_name: &str,
    ) -> Result<Option<ProviderProtocolNegotiation>, ProviderError> {
        Ok(self
            .tool_capability_cache
            .lock()
            .map_err(|_| provider_capability_cache_error())?
            .get(model_name)
            .cloned()
            .map(cache_hit_negotiation))
    }

    fn negotiate_openai_tool_capabilities(
        &self,
        model_name: &str,
        cancellation: &CancellationToken,
    ) -> Result<ProviderProtocolNegotiation, ProviderError> {
        loop {
            if cancellation.is_cancelled() {
                return Err(capability_probe_cancelled_error());
            }
            let cached = self.cached_tool_capability_negotiation(model_name)?;
            if let Some(cached) = cached {
                return Ok(cached);
            }
            let mut in_flight = self
                .tool_capability_probe_in_flight
                .lock()
                .map_err(|_| provider_capability_cache_error())?;
            let (probe_state, owner) = if let Some(probe_state) = in_flight.get(model_name) {
                (Arc::clone(probe_state), false)
            } else {
                let probe_state = Arc::new(CapabilityProbeState::new());
                in_flight.insert(model_name.to_string(), Arc::clone(&probe_state));
                (probe_state, true)
            };
            drop(in_flight);

            if owner {
                let owner_guard = CapabilityProbeOwnerGuard::new(
                    Arc::clone(&self.tool_capability_probe_in_flight),
                    model_name.to_string(),
                    Arc::clone(&probe_state),
                );
                let result = self.probe_tool_capabilities(model_name, cancellation);
                let completion = if result
                    .as_ref()
                    .is_err_and(|error| error.error.kind == ModelErrorKind::Cancelled)
                {
                    CapabilityProbeCompletion::OwnerCancelled
                } else {
                    CapabilityProbeCompletion::Result(result.clone())
                };
                owner_guard.finish(completion);
                return result;
            }

            match probe_state.wait(cancellation)? {
                CapabilityProbeCompletion::Result(result) => return result,
                CapabilityProbeCompletion::OwnerCancelled => continue,
            }
        }
    }

    fn probe_tool_capabilities(
        &self,
        model_name: &str,
        cancellation: &CancellationToken,
    ) -> Result<ProviderProtocolNegotiation, ProviderError> {
        let protocols = self.config.api_protocol_candidates();
        let mut accumulated_metadata: Option<ProviderCapabilityMetadata> = None;
        for (index, api_protocol) in protocols.iter().copied().enumerate() {
            match self.probe_tool_capabilities_for_protocol(model_name, cancellation, api_protocol)
            {
                Ok(mut negotiation) => {
                    if let Some(metadata) = accumulated_metadata.take() {
                        merge_capability_metadata(&mut negotiation.metadata, &metadata);
                    }
                    negotiation.metadata.fallback_count = negotiation
                        .metadata
                        .fallback_count
                        .saturating_add(index as u32);
                    self.tool_capability_cache
                        .lock()
                        .map_err(|_| provider_capability_cache_error())?
                        .insert(model_name.to_string(), negotiation.clone());
                    return Ok(negotiation);
                }
                Err(mut error)
                    if index + 1 < protocols.len()
                        && provider_protocol_fallback_allowed(&error) =>
                {
                    if let Some(metadata) = error.capability_metadata.take().map(|value| *value) {
                        match accumulated_metadata.as_mut() {
                            Some(accumulated) => merge_capability_metadata(accumulated, &metadata),
                            None => accumulated_metadata = Some(metadata),
                        }
                    }
                }
                Err(mut error) => {
                    if let Some(accumulated) = accumulated_metadata {
                        match error.capability_metadata.as_mut() {
                            Some(metadata) => merge_capability_metadata(metadata, &accumulated),
                            None => error.capability_metadata = Some(Box::new(accumulated)),
                        }
                    }
                    return Err(error);
                }
            }
        }
        Err(capability_probe_unsupported_error(ModelError::new(
            ModelErrorKind::UnsupportedCapability,
            "provider does not support native structured tool calls",
        )))
    }

    fn probe_tool_capabilities_for_protocol(
        &self,
        model_name: &str,
        cancellation: &CancellationToken,
        api_protocol: ProviderApiProtocol,
    ) -> Result<ProviderProtocolNegotiation, ProviderError> {
        let mut probe_usage = ModelUsage::default();
        let mut probe_attempt_metadata = ProviderAttemptMetadata::zero();
        let profiles = capability_probe_profiles(&self.config, model_name, api_protocol);
        let profile_count = profiles.len();

        for (index, profile) in profiles.into_iter().enumerate() {
            if cancellation.is_cancelled() {
                return Err(provider_cancelled_error().with_capability_metadata(
                    capability_probe_metadata(
                        api_protocol,
                        profile.profile,
                        index as u32,
                        index as u32,
                        &probe_usage,
                        &probe_attempt_metadata,
                    ),
                ));
            }
            let local_validation = validate_model_request(&profile.request);
            if !local_validation.valid {
                return Err(capability_probe_definition_error(local_validation.errors)
                    .with_capability_metadata(capability_probe_metadata(
                        api_protocol,
                        profile.profile,
                        index as u32,
                        index as u32,
                        &probe_usage,
                        &probe_attempt_metadata,
                    )));
            }
            let mut completion = match self.complete_capability_probe(
                &profile.request,
                cancellation,
                &profile.contract,
                api_protocol,
                &mut probe_usage,
                &mut probe_attempt_metadata,
            ) {
                Ok(completion) => completion,
                Err(error) if is_capability_probe_profile_rejection(&error) => {
                    if index + 1 == profile_count {
                        return Err(capability_probe_failure(
                            error,
                            capability_probe_metadata(
                                api_protocol,
                                profile.profile,
                                index as u32 + 1,
                                index as u32,
                                &probe_usage,
                                &probe_attempt_metadata,
                            ),
                            "capability_profiles_exhausted",
                        ));
                    }
                    continue;
                }
                Err(error) => {
                    return Err(error.with_capability_metadata(capability_probe_metadata(
                        api_protocol,
                        profile.profile,
                        index as u32 + 1,
                        index as u32,
                        &probe_usage,
                        &probe_attempt_metadata,
                    )));
                }
            };
            let mut negotiated_profile =
                capability_probe_profile_match(&completion.response, &profile);
            let mut contract = profile.contract.clone();
            if negotiated_profile.is_some() && completion.reasoning_content_present {
                contract.tool_reasoning_mode = ProviderToolReasoningMode::DisabledForToolCalls;
                completion = match self.complete_capability_probe(
                    &profile.request,
                    cancellation,
                    &contract,
                    api_protocol,
                    &mut probe_usage,
                    &mut probe_attempt_metadata,
                ) {
                    Ok(completion) => completion,
                    Err(error) => {
                        let error = if is_capability_probe_profile_rejection(&error) {
                            capability_probe_tool_reasoning_rejection(error)
                        } else {
                            error
                        };
                        return Err(capability_probe_failure(
                            error,
                            capability_probe_metadata(
                                api_protocol,
                                profile.profile,
                                index as u32 + 1,
                                index as u32,
                                &probe_usage,
                                &probe_attempt_metadata,
                            ),
                            "tool_reasoning_disable_unsupported",
                        ));
                    }
                };
                if completion.reasoning_content_present {
                    return Err(capability_probe_tool_reasoning_error(
                        &completion.response,
                        "tool_reasoning_disable_not_honored",
                    )
                    .with_capability_metadata(capability_probe_metadata(
                        api_protocol,
                        profile.profile,
                        index as u32 + 1,
                        index as u32,
                        &probe_usage,
                        &probe_attempt_metadata,
                    )));
                }
                negotiated_profile = capability_probe_profile_match(&completion.response, &profile);
                if negotiated_profile.is_none() {
                    return Err(capability_probe_tool_reasoning_error(
                        &completion.response,
                        "tool_reasoning_disabled_profile_invalid",
                    )
                    .with_capability_metadata(capability_probe_metadata(
                        api_protocol,
                        profile.profile,
                        index as u32 + 1,
                        index as u32,
                        &probe_usage,
                        &probe_attempt_metadata,
                    )));
                }
            }
            if let Some(negotiated_profile) = negotiated_profile {
                if negotiated_profile != profile.profile {
                    contract.supports_parallel_tool_calls = false;
                }
                let continuation_request =
                    capability_probe_continuation_request(&profile, &completion.response);
                let continuation_validation = validate_model_request_with_capabilities(
                    &continuation_request,
                    Some(&contract),
                );
                if !continuation_validation.valid {
                    return Err(
                        capability_probe_definition_error(continuation_validation.errors)
                            .with_capability_metadata(capability_probe_metadata(
                                api_protocol,
                                profile.profile,
                                index as u32 + 1,
                                index as u32,
                                &probe_usage,
                                &probe_attempt_metadata,
                            )),
                    );
                }
                let continuation = match self.complete_capability_probe(
                    &continuation_request,
                    cancellation,
                    &contract,
                    api_protocol,
                    &mut probe_usage,
                    &mut probe_attempt_metadata,
                ) {
                    Ok(completion) => completion,
                    Err(error) if is_capability_probe_profile_rejection(&error) => {
                        if index + 1 == profile_count {
                            return Err(capability_probe_failure(
                                error,
                                capability_probe_metadata(
                                    api_protocol,
                                    profile.profile,
                                    index as u32 + 1,
                                    index as u32,
                                    &probe_usage,
                                    &probe_attempt_metadata,
                                ),
                                "capability_probe_multi_turn_tool_calls_unsupported",
                            ));
                        }
                        continue;
                    }
                    Err(error) => {
                        return Err(error.with_capability_metadata(capability_probe_metadata(
                            api_protocol,
                            profile.profile,
                            index as u32 + 1,
                            index as u32,
                            &probe_usage,
                            &probe_attempt_metadata,
                        )));
                    }
                };
                if continuation.reasoning_content_present {
                    let error = capability_probe_tool_reasoning_error(
                        &continuation.response,
                        "tool_reasoning_content_present_after_tool_result",
                    );
                    if index + 1 == profile_count {
                        return Err(error.with_capability_metadata(capability_probe_metadata(
                            api_protocol,
                            profile.profile,
                            index as u32 + 1,
                            index as u32,
                            &probe_usage,
                            &probe_attempt_metadata,
                        )));
                    }
                    continue;
                }
                if !capability_probe_continuation_matches(&continuation.response, &profile) {
                    let error = capability_probe_continuation_error(&continuation.response);
                    if index + 1 == profile_count {
                        return Err(error.with_capability_metadata(capability_probe_metadata(
                            api_protocol,
                            profile.profile,
                            index as u32 + 1,
                            index as u32,
                            &probe_usage,
                            &probe_attempt_metadata,
                        )));
                    }
                    continue;
                }
                let negotiation = ProviderProtocolNegotiation {
                    contract,
                    metadata: capability_probe_metadata(
                        api_protocol,
                        negotiated_profile,
                        index as u32 + 1,
                        index as u32,
                        &probe_usage,
                        &probe_attempt_metadata,
                    ),
                };
                return Ok(negotiation);
            }
            if index + 1 == profile_count {
                return Err(capability_probe_response_error(&completion.response)
                    .with_capability_metadata(capability_probe_metadata(
                        api_protocol,
                        profile.profile,
                        index as u32 + 1,
                        index as u32,
                        &probe_usage,
                        &probe_attempt_metadata,
                    )));
            }
        }

        Err(capability_probe_unsupported_error(ModelError::new(
            ModelErrorKind::UnsupportedCapability,
            "provider does not support native structured tool calls",
        )))
    }

    fn complete_capability_probe(
        &self,
        request: &ModelTurnRequest,
        cancellation: &CancellationToken,
        contract: &ProviderProtocolContract,
        api_protocol: ProviderApiProtocol,
        probe_usage: &mut ModelUsage,
        probe_attempt_metadata: &mut ProviderAttemptMetadata,
    ) -> Result<OpenAiCompletion, ProviderError> {
        let model_name = request
            .model_preferences
            .model_name
            .as_deref()
            .unwrap_or(&self.config.model_name);
        let result = self.complete_with_contract_details(
            request,
            cancellation,
            contract,
            api_protocol,
            model_name,
        );
        match &result {
            Ok(completion) => {
                add_model_usage(probe_usage, &completion.response.usage);
                if let Some(metadata) = &completion.response.provider_attempt_metadata {
                    add_provider_attempt_metadata(probe_attempt_metadata, metadata);
                }
            }
            Err(error) => {
                if let Some(metadata) = &error.provider_attempt_metadata {
                    add_provider_attempt_metadata(probe_attempt_metadata, metadata);
                }
            }
        }
        result
    }

    fn complete_with_contract(
        &self,
        request: &ModelTurnRequest,
        cancellation: &CancellationToken,
        capabilities: &ProviderProtocolContract,
        api_protocol: ProviderApiProtocol,
        model_name: &str,
    ) -> Result<ModelTurnResponse, ProviderError> {
        let completion = self.complete_with_contract_details(
            request,
            cancellation,
            capabilities,
            api_protocol,
            model_name,
        )?;
        if request_uses_tool_protocol(request) && completion.reasoning_content_present {
            return Err(provider_tool_reasoning_history_error(
                &completion.response,
                capabilities.tool_reasoning_mode,
            ));
        }
        Ok(completion.response)
    }

    fn complete_with_contract_details(
        &self,
        request: &ModelTurnRequest,
        cancellation: &CancellationToken,
        capabilities: &ProviderProtocolContract,
        api_protocol: ProviderApiProtocol,
        model_name: &str,
    ) -> Result<OpenAiCompletion, ProviderError> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(provider_runtime_error)?;
        let started_at = Instant::now();
        let mut metadata = ProviderAttemptMetadata::zero();
        let endpoint = match api_protocol {
            ProviderApiProtocol::OpenAiResponses => responses_endpoint(&self.config.base_url),
            ProviderApiProtocol::Declared | ProviderApiProtocol::OpenAiChatCompletions => {
                self.config.endpoint()
            }
        };
        let request_payload = match api_protocol {
            ProviderApiProtocol::OpenAiResponses => {
                openai_responses_request_payload(request, model_name, capabilities)
            }
            ProviderApiProtocol::Declared | ProviderApiProtocol::OpenAiChatCompletions => {
                openai_request_payload(request, model_name, capabilities)
            }
        };
        loop {
            if cancellation.is_cancelled() {
                return Err(provider_cancelled_error().with_provider_attempt_metadata(
                    provider_attempt_metadata(&metadata, started_at),
                ));
            }
            metadata.attempt_count += 1;
            let response =
                match block_on_provider_future(
                    &runtime,
                    cancellation,
                    "provider_request_send_failed",
                    ProviderErrorStage::RequestSend,
                    self.request_timeout_seconds,
                    || {
                        self.client
                            .post(&endpoint)
                            .bearer_auth(&self.config.api_key)
                            .json(&request_payload)
                            .send()
                    },
                ) {
                    Ok(response) => response,
                    Err(error)
                        if metadata.attempt_count < MAX_PROVIDER_ATTEMPTS
                            && provider_error_is_retryable(&error) =>
                    {
                        metadata.retry_count += 1;
                        wait_provider_backoff(
                            &runtime,
                            cancellation,
                            provider_retry_backoff(metadata.retry_count),
                        )
                        .map_err(|cancelled| {
                            cancelled.with_provider_attempt_metadata(provider_attempt_metadata(
                                &metadata, started_at,
                            ))
                        })?;
                        continue;
                    }
                    Err(error) => {
                        return Err(error.with_provider_attempt_metadata(
                            provider_attempt_metadata(&metadata, started_at),
                        ));
                    }
                };
            let status = response.status();
            if !status.is_success() {
                let error = ProviderError::from_model_error(model_error_from_http_status(
                    status.as_u16(),
                    &self.config.provider_name,
                    model_name,
                ));
                if metadata.attempt_count < MAX_PROVIDER_ATTEMPTS
                    && http_status_is_retryable(status.as_u16())
                {
                    metadata.retry_count += 1;
                    wait_provider_backoff(
                        &runtime,
                        cancellation,
                        provider_retry_backoff(metadata.retry_count),
                    )
                    .map_err(|cancelled| {
                        cancelled.with_provider_attempt_metadata(provider_attempt_metadata(
                            &metadata, started_at,
                        ))
                    })?;
                    continue;
                }
                return Err(
                    error.with_provider_attempt_metadata(provider_attempt_metadata(
                        &metadata, started_at,
                    )),
                );
            }
            let body =
                match read_bounded_provider_response_body(
                    &runtime,
                    cancellation,
                    self.request_timeout_seconds,
                    response,
                ) {
                    Ok(body) => body,
                    Err(error)
                        if metadata.attempt_count < MAX_PROVIDER_ATTEMPTS
                            && provider_error_is_retryable(&error) =>
                    {
                        metadata.retry_count += 1;
                        wait_provider_backoff(
                            &runtime,
                            cancellation,
                            provider_retry_backoff(metadata.retry_count),
                        )
                        .map_err(|cancelled| {
                            cancelled.with_provider_attempt_metadata(provider_attempt_metadata(
                                &metadata, started_at,
                            ))
                        })?;
                        continue;
                    }
                    Err(error) => {
                        return Err(error.with_provider_attempt_metadata(
                            provider_attempt_metadata(&metadata, started_at),
                        ));
                    }
                };
            let payload = serde_json::from_slice::<Value>(&body).map_err(|_| {
                ProviderError::from_model_error(provider_response_json_error())
                    .with_provider_attempt_metadata(provider_attempt_metadata(
                        &metadata, started_at,
                    ))
            })?;
            let reasoning_content_present = match api_protocol {
                ProviderApiProtocol::OpenAiResponses => {
                    openai_responses_reasoning_content_present(&payload)
                }
                ProviderApiProtocol::Declared | ProviderApiProtocol::OpenAiChatCompletions => {
                    openai_reasoning_content_present(&payload)
                }
            };
            let parsed = match api_protocol {
                ProviderApiProtocol::OpenAiResponses => parse_openai_responses_response(
                    request,
                    &self.config,
                    payload,
                    capabilities,
                    model_name,
                ),
                ProviderApiProtocol::Declared | ProviderApiProtocol::OpenAiChatCompletions => {
                    parse_openai_response(request, &self.config, payload, capabilities, model_name)
                }
            };
            return parsed
                .map(|mut response| {
                    response.provider_attempt_metadata =
                        Some(provider_attempt_metadata(&metadata, started_at));
                    OpenAiCompletion {
                        response,
                        reasoning_content_present,
                    }
                })
                .map_err(|error| {
                    error.with_provider_attempt_metadata(provider_attempt_metadata(
                        &metadata, started_at,
                    ))
                });
        }
    }
}

impl Provider for OpenAiProvider {
    fn protocol_contract(&self) -> ProviderProtocolContract {
        self.config.protocol_contract()
    }

    fn negotiate_tool_capabilities(
        &self,
        model_preferences: &ModelPreferences,
        cancellation: &CancellationToken,
    ) -> Result<ProviderProtocolNegotiation, ProviderError> {
        self.negotiate_openai_tool_capabilities(
            model_preferences
                .model_name
                .as_deref()
                .unwrap_or(&self.config.model_name),
            cancellation,
        )
    }

    fn complete(
        &self,
        request: &ModelTurnRequest,
        cancellation: &CancellationToken,
    ) -> Result<ModelTurnResponse, ProviderError> {
        if cancellation.is_cancelled() {
            return Err(provider_cancelled_error()
                .with_provider_attempt_metadata(ProviderAttemptMetadata::zero()));
        }
        let local_validation = validate_model_request(request);
        if !local_validation.valid {
            return Err(provider_request_validation_error(
                local_validation,
                &self.config,
            ));
        }
        let (capabilities, capability_metadata, api_protocol) =
            if !request_uses_tool_protocol(request) {
                (
                    self.protocol_contract(),
                    None,
                    self.config.completion_protocol_without_tools(),
                )
            } else {
                let effective_model_name = request
                    .model_preferences
                    .model_name
                    .as_deref()
                    .unwrap_or(&self.config.model_name);
                let negotiation =
                    self.negotiate_openai_tool_capabilities(effective_model_name, cancellation)?;
                let api_protocol = negotiation.metadata.api_protocol;
                (
                    negotiation.contract,
                    Some(negotiation.metadata),
                    api_protocol,
                )
            };
        let request_validation =
            validate_model_request_with_capabilities(request, Some(&capabilities));
        if !request_validation.valid {
            let provider_error =
                provider_request_validation_error(request_validation, &self.config);
            return Err(attach_capability_metadata(
                provider_error,
                &capability_metadata,
            ));
        }
        let effective_model_name = request
            .model_preferences
            .model_name
            .as_deref()
            .unwrap_or(&self.config.model_name);
        self.complete_with_contract(
            request,
            cancellation,
            &capabilities,
            api_protocol,
            effective_model_name,
        )
        .map_err(|error| attach_capability_metadata(error, &capability_metadata))
    }
}

fn request_uses_tool_protocol(request: &ModelTurnRequest) -> bool {
    !request.tools.is_empty()
        || request
            .messages
            .iter()
            .any(|message| message.role == ModelRole::Tool || !message.tool_calls.is_empty())
}

#[derive(Debug, Clone, PartialEq, Error)]
#[error("{message}")]
/// A provider failure with typed model error, attempt metadata, and optional capability evidence.
pub struct ProviderError {
    pub message: String,
    pub error: Box<ModelError>,
    pub provider_attempt_metadata: Option<ProviderAttemptMetadata>,
    pub capability_metadata: Option<Box<ProviderCapabilityMetadata>>,
}

fn provider_request_validation_error(
    validation: ModelValidationResult,
    config: &OpenAiProviderConfig,
) -> ProviderError {
    let kind = if validation_is_unsupported_capability(&validation) {
        ModelErrorKind::UnsupportedCapability
    } else {
        ModelErrorKind::InvalidRequest
    };
    let mut error = ModelError::new(
        kind,
        format!(
            "model request validation failed: {}",
            validation.errors.join(", ")
        ),
    )
    .with_provider(config.provider_name.clone())
    .with_model(config.model_name.clone())
    .with_provider_diagnostic("provider_request_invalid", ProviderErrorStage::RequestSend);
    error.validation_errors = validation.errors;
    ProviderError::from_model_error(error)
        .with_provider_attempt_metadata(ProviderAttemptMetadata::zero())
}

impl ProviderError {
    pub fn from_model_error(error: ModelError) -> Self {
        Self {
            message: error.message.clone(),
            error: Box::new(error),
            provider_attempt_metadata: None,
            capability_metadata: None,
        }
    }

    pub fn with_provider_attempt_metadata(mut self, metadata: ProviderAttemptMetadata) -> Self {
        self.provider_attempt_metadata = Some(metadata);
        self
    }

    pub fn with_capability_metadata(mut self, metadata: ProviderCapabilityMetadata) -> Self {
        self.capability_metadata = Some(Box::new(metadata));
        self
    }
}

fn attach_capability_metadata(
    mut error: ProviderError,
    metadata: &Option<ProviderCapabilityMetadata>,
) -> ProviderError {
    if let Some(metadata) = metadata {
        error.capability_metadata = Some(Box::new(metadata.clone()));
    }
    error
}

/// Resolves a base URL to an OpenAI-compatible Chat Completions endpoint.
pub fn chat_completions_endpoint(base_url: &str) -> String {
    let trimmed = base_url.trim().trim_end_matches('/');
    if trimmed.ends_with(CHAT_COMPLETIONS_PATH) {
        trimmed.to_string()
    } else if trimmed.ends_with("/v1") {
        format!("{trimmed}{CHAT_COMPLETIONS_PATH}")
    } else {
        format!("{trimmed}{V1_CHAT_COMPLETIONS_PATH}")
    }
}

/// Resolves a base URL to an OpenAI-compatible Responses endpoint.
pub fn responses_endpoint(base_url: &str) -> String {
    let trimmed = base_url.trim().trim_end_matches('/');
    if trimmed.ends_with(RESPONSES_PATH) {
        trimmed.to_string()
    } else if let Some(prefix) = trimmed.strip_suffix(CHAT_COMPLETIONS_PATH) {
        format!("{prefix}{RESPONSES_PATH}")
    } else if trimmed.ends_with("/v1") {
        format!("{trimmed}{RESPONSES_PATH}")
    } else {
        format!("{trimmed}{V1_RESPONSES_PATH}")
    }
}

/// Converts a provider failure into the failed response shape consumed by the AgentLoop.
pub fn provider_error_response(
    request: &ModelTurnRequest,
    error: ProviderError,
) -> ModelTurnResponse {
    let provider_attempt_metadata = error.provider_attempt_metadata.clone();
    ModelTurnResponse {
        request_id: request.request_id.clone(),
        response_id: format!("{}_provider_error", request.request_id),
        status: ModelTurnStatus::Failed,
        assistant_message: None,
        tool_calls: Vec::new(),
        usage: ModelUsage::default(),
        finish_reason: None,
        validation: None,
        error: Some(*error.error),
        provider_name: None,
        model_name: request.model_preferences.model_name.clone(),
        provider_attempt_metadata,
    }
}

/// Resolves provider configuration while reporting only redacted presence and source metadata.
pub fn resolve_provider_config<F>(get_env: F) -> ProviderConfigResolution
where
    F: FnMut(&str) -> Option<String>,
{
    let project_dir = std::env::current_dir().ok();
    let values = resolve_provider_values(get_env, project_dir.as_deref());
    provider_config_resolution(&values)
}

fn openai_request_payload(
    request: &ModelTurnRequest,
    model_name: &str,
    capabilities: &ProviderProtocolContract,
) -> Value {
    let mut payload = json!({
        "model": request
            .model_preferences
            .model_name
            .as_deref()
            .unwrap_or(model_name),
        "messages": request
            .messages
            .iter()
            .map(openai_message_payload)
            .collect::<Vec<_>>(),
        "stream": false,
    });
    if let Some(max_output_tokens) = request.model_preferences.max_output_tokens {
        payload["max_tokens"] = json!(max_output_tokens);
    }
    if let Some(temperature) = request.model_preferences.temperature {
        payload["temperature"] = json!(temperature);
    }
    if let Some(top_p) = request.model_preferences.top_p {
        payload["top_p"] = json!(top_p);
    }
    if request.model_preferences.json_mode {
        payload["response_format"] = json!({"type": "json_object"});
    }
    if !request.tools.is_empty() {
        payload["tools"] = json!(
            request
                .tools
                .iter()
                .map(|tool| openai_tool_payload(tool, request.tool_choice.strict_tool_schema))
                .collect::<Vec<_>>()
        );
        payload["tool_choice"] = openai_tool_choice_payload(request);
        payload["parallel_tool_calls"] = json!(request.tool_choice.max_tool_calls > 1);
    }
    if request_uses_tool_protocol(request)
        && capabilities.tool_reasoning_mode == ProviderToolReasoningMode::DisabledForToolCalls
    {
        payload["thinking"] = json!({"type": "disabled"});
    }
    payload
}

fn openai_responses_request_payload(
    request: &ModelTurnRequest,
    model_name: &str,
    capabilities: &ProviderProtocolContract,
) -> Value {
    let (instructions, input) = openai_responses_input(&request.messages);
    let mut payload = json!({
        "model": request
            .model_preferences
            .model_name
            .as_deref()
            .unwrap_or(model_name),
        "input": input,
        "stream": false,
        "store": false,
    });
    if let Some(instructions) = instructions {
        payload["instructions"] = json!(instructions);
    }
    if let Some(max_output_tokens) = request.model_preferences.max_output_tokens {
        payload["max_output_tokens"] = json!(max_output_tokens);
    }
    if let Some(temperature) = request.model_preferences.temperature {
        payload["temperature"] = json!(temperature);
    }
    if let Some(top_p) = request.model_preferences.top_p {
        payload["top_p"] = json!(top_p);
    }
    if request.model_preferences.json_mode {
        payload["text"] = json!({"format": {"type": "json_object"}});
    }
    if !request.tools.is_empty() {
        payload["tools"] = json!(
            request
                .tools
                .iter()
                .map(|tool| {
                    json!({
                        "type": "function",
                        "name": tool.name,
                        "description": tool.description,
                        "parameters": tool.parameters_schema,
                        "strict": request.tool_choice.strict_tool_schema,
                    })
                })
                .collect::<Vec<_>>()
        );
        payload["tool_choice"] = openai_responses_tool_choice_payload(request);
        payload["parallel_tool_calls"] = json!(request.tool_choice.max_tool_calls > 1);
    }
    if request_uses_tool_protocol(request)
        && capabilities.tool_reasoning_mode == ProviderToolReasoningMode::DisabledForToolCalls
    {
        payload["reasoning"] = json!({"effort": "none"});
    }
    payload
}

fn openai_responses_input(messages: &[ModelMessage]) -> (Option<String>, Vec<Value>) {
    let instruction_count = messages
        .iter()
        .take_while(|message| matches!(message.role, ModelRole::System | ModelRole::Developer))
        .count();
    let instructions = messages[..instruction_count]
        .iter()
        .map(message_text)
        .filter(|message| !message.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    let mut items = Vec::new();
    for message in &messages[instruction_count..] {
        match message.role {
            ModelRole::Tool => {
                items.push(json!({
                    "type": "function_call_output",
                    "call_id": message.tool_call_id,
                    "output": message_text(message),
                }));
            }
            ModelRole::Assistant => {
                if !message_text(message).is_empty() {
                    items.push(json!({
                        "type": "message",
                        "role": "assistant",
                        "content": message_text(message),
                    }));
                }
                items.extend(message.tool_calls.iter().map(|call| {
                    json!({
                        "type": "function_call",
                        "call_id": call.tool_call_id,
                        "name": call.tool_name,
                        "arguments": call.raw_arguments,
                    })
                }));
            }
            ModelRole::System | ModelRole::Developer | ModelRole::User => {
                let role = match message.role {
                    ModelRole::System => "system",
                    ModelRole::Developer => "developer",
                    ModelRole::User => "user",
                    ModelRole::Assistant | ModelRole::Tool => unreachable!(),
                };
                items.push(json!({
                    "type": "message",
                    "role": role,
                    "content": message_text(message),
                }));
            }
        }
    }
    ((!instructions.is_empty()).then_some(instructions), items)
}

fn openai_responses_tool_choice_payload(request: &ModelTurnRequest) -> Value {
    match request.tool_choice.mode {
        ToolChoiceMode::None => json!("none"),
        ToolChoiceMode::Required => json!("required"),
        ToolChoiceMode::Auto => json!("auto"),
    }
}

fn openai_message_payload(message: &ModelMessage) -> Value {
    let role = serde_json::to_value(&message.role)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_else(|| "user".to_string());
    let mut payload = json!({
        "role": role,
        "content": openai_message_content(message),
    });
    if let Some(tool_call_id) = &message.tool_call_id {
        payload["tool_call_id"] = json!(tool_call_id);
    }
    if !message.tool_calls.is_empty() {
        payload["tool_calls"] = json!(
            message
                .tool_calls
                .iter()
                .map(openai_tool_call_payload)
                .collect::<Vec<_>>()
        );
    }
    payload
}

fn openai_message_content(message: &ModelMessage) -> Value {
    let text = message_text(message);
    if message.role == ModelRole::Assistant && !message.tool_calls.is_empty() && text.is_empty() {
        Value::Null
    } else {
        json!(text)
    }
}

fn openai_tool_call_payload(tool_call: &ModelToolCall) -> Value {
    json!({
        "id": tool_call.tool_call_id,
        "type": "function",
        "function": {
            "name": tool_call.tool_name,
            "arguments": tool_call.raw_arguments,
        }
    })
}

fn openai_tool_payload(tool: &ModelToolSchema, strict_tool_schema: bool) -> Value {
    let mut payload = json!({
        "type": "function",
        "function": {
            "name": tool.name,
            "description": tool.description,
            "parameters": tool.parameters_schema,
        }
    });
    if strict_tool_schema {
        payload["function"]["strict"] = json!(true);
    }
    payload
}

fn openai_tool_choice_payload(request: &ModelTurnRequest) -> Value {
    match request.tool_choice.mode {
        ToolChoiceMode::None => json!("none"),
        ToolChoiceMode::Required => json!("required"),
        ToolChoiceMode::Auto => json!("auto"),
    }
}

struct CapabilityProbeProfile {
    profile: ProviderCapabilityProfile,
    contract: ProviderProtocolContract,
    request: ModelTurnRequest,
    expected_calls: Vec<CapabilityProbeExpectedCall>,
    single_call_fallback: Option<ProviderCapabilityProfile>,
}

struct OpenAiCompletion {
    response: ModelTurnResponse,
    reasoning_content_present: bool,
}

#[derive(Debug, Clone)]
struct CapabilityProbeExpectedCall {
    tool_name: &'static str,
    allowed_arguments: Vec<Value>,
}

fn capability_probe_profiles(
    config: &OpenAiProviderConfig,
    model_name: &str,
    api_protocol: ProviderApiProtocol,
) -> Vec<CapabilityProbeProfile> {
    let base = config.protocol_contract();
    let schema_branch = |label: &str| {
        json!({
            "type": "object",
            "properties": {
                "probe": {
                    "type": "string",
                    "const": label
                },
                "values": {
                    "type": "array",
                    "minItems": 2,
                    "maxItems": 2,
                    "items": {
                        "type": "integer",
                        "const": CAPABILITY_PROBE_EXPECTED_VALUE
                    }
                }
            },
            "required": ["probe", "values"],
            "additionalProperties": false
        })
    };
    let tool_schema = json!({
        "oneOf": [
            schema_branch(CAPABILITY_PROBE_EXPECTED_LABEL),
            schema_branch(CAPABILITY_PROBE_ALTERNATE_LABEL)
        ]
    });
    let strict_arguments = json!({
        "probe": CAPABILITY_PROBE_EXPECTED_LABEL,
        "values": [CAPABILITY_PROBE_EXPECTED_VALUE, CAPABILITY_PROBE_EXPECTED_VALUE]
    });
    let alternate_strict_arguments = json!({
        "probe": CAPABILITY_PROBE_ALTERNATE_LABEL,
        "values": [CAPABILITY_PROBE_EXPECTED_VALUE, CAPABILITY_PROBE_EXPECTED_VALUE]
    });
    let tool = |name: String, parameters_schema: Value| ModelToolSchema {
        name,
        description: "Fixed capability probe tool; no external side effect.".to_string(),
        parameters_schema,
    };
    let probe_tool_name = |index: u32| match index {
        0 => CAPABILITY_PROBE_TOOL_A.to_string(),
        1 => CAPABILITY_PROBE_TOOL_B.to_string(),
        _ => format!("singularity_capability_probe_extra_{index}"),
    };
    let probe_tools = |count: u32, parameters_schema: &Value| {
        (0..count)
            .map(|index| tool(probe_tool_name(index), parameters_schema.clone()))
            .collect::<Vec<_>>()
    };
    let router_schema = json!({
        "oneOf": (0..DEFAULT_MAX_TOOLS_PER_REQUEST)
            .map(|index| {
                let target = probe_tool_name(index);
                let label = if index % 2 == 0 {
                    CAPABILITY_PROBE_EXPECTED_LABEL
                } else {
                    CAPABILITY_PROBE_ALTERNATE_LABEL
                };
                json!({
                    "type": "object",
                    "properties": {
                        "tool_name": {
                            "type": "string",
                            "const": target
                        },
                        "arguments": schema_branch(label)
                    },
                    "required": ["tool_name", "arguments"],
                    "additionalProperties": false
                })
            })
            .collect::<Vec<_>>()
    });
    let router_tool = tool(CAPABILITY_PROBE_ROUTER.to_string(), router_schema);
    let make_request = |tools: Vec<ModelToolSchema>,
                        mode: ToolChoiceMode,
                        max_tool_calls: u32,
                        strict: bool,
                        instruction: &str| {
        let mut request = ModelTurnRequest::new(
            CAPABILITY_PROBE_REQUEST_ID,
            vec![
                ModelMessage::text(ModelRole::Developer, CAPABILITY_PROBE_DEVELOPER_INSTRUCTION),
                ModelMessage::text(ModelRole::User, instruction),
            ],
        );
        request.model_preferences.model_name = Some(model_name.to_string());
        request.tools = tools;
        request.tool_choice = ToolChoicePolicy {
            mode,
            max_tool_calls,
            strict_tool_schema: strict,
        };
        request
    };
    let make_contract = |strict: bool,
                         supports_parallel_tool_calls: bool,
                         max_tools_per_request: u32,
                         tool_definition_mode: ProviderToolDefinitionMode| {
        ProviderProtocolContract {
            supports_parallel_tool_calls,
            supports_strict_tool_schema: strict,
            tool_reasoning_mode: if api_protocol == ProviderApiProtocol::OpenAiResponses {
                ProviderToolReasoningMode::DisabledForToolCalls
            } else {
                ProviderToolReasoningMode::Unspecified
            },
            tool_definition_mode,
            max_tools_per_request,
            supports_json_mode: false,
            supports_system_message: false,
            supports_developer_message: true,
            ..base.clone()
        }
    };
    let parallel_expected = |allowed_arguments: Vec<Value>| {
        vec![
            CapabilityProbeExpectedCall {
                tool_name: CAPABILITY_PROBE_TOOL_A,
                allowed_arguments: allowed_arguments.clone(),
            },
            CapabilityProbeExpectedCall {
                tool_name: CAPABILITY_PROBE_TOOL_B,
                allowed_arguments,
            },
        ]
    };
    let single_expected = |tool_name, allowed_arguments| {
        vec![CapabilityProbeExpectedCall {
            tool_name,
            allowed_arguments,
        }]
    };
    let direct_tool_count = DEFAULT_MAX_TOOLS_PER_REQUEST;
    let strict_allowed_arguments =
        vec![strict_arguments.clone(), alternate_strict_arguments.clone()];
    let mut profiles = Vec::new();
    profiles.push(CapabilityProbeProfile {
        profile: ProviderCapabilityProfile::StrictParallel,
        contract: make_contract(
            true,
            true,
            direct_tool_count,
            ProviderToolDefinitionMode::Direct,
        ),
        request: make_request(
            probe_tools(direct_tool_count, &tool_schema),
            ToolChoiceMode::Auto,
            2,
            true,
            "First call singularity_capability_probe_a and singularity_capability_probe_b once each. After both tool results, call singularity_capability_probe_a once more.",
        ),
        expected_calls: parallel_expected(strict_allowed_arguments.clone()),
        single_call_fallback: Some(ProviderCapabilityProfile::StrictSingle),
    });
    profiles.push(CapabilityProbeProfile {
        profile: ProviderCapabilityProfile::StrictSingle,
        contract: make_contract(
            true,
            false,
            direct_tool_count,
            ProviderToolDefinitionMode::Direct,
        ),
        request: make_request(
            probe_tools(direct_tool_count, &tool_schema),
            ToolChoiceMode::Auto,
            1,
            true,
            "First call singularity_capability_probe_a once. After its tool result, call singularity_capability_probe_a once more.",
        ),
        expected_calls: single_expected(CAPABILITY_PROBE_TOOL_A, strict_allowed_arguments),
        single_call_fallback: None,
    });
    profiles.push(CapabilityProbeProfile {
        profile: ProviderCapabilityProfile::NonStrictParallel,
        contract: make_contract(
            false,
            true,
            direct_tool_count,
            ProviderToolDefinitionMode::Direct,
        ),
        request: make_request(
            probe_tools(direct_tool_count, &tool_schema),
            ToolChoiceMode::Auto,
            2,
            false,
            "First call singularity_capability_probe_a and singularity_capability_probe_b once each with exactly {\"probe\":\"schema_sentinel_alpha\",\"values\":[7,7]} as each arguments object. After both tool results, call singularity_capability_probe_a once more with the same arguments.",
        ),
        expected_calls: parallel_expected(Vec::new()),
        single_call_fallback: Some(ProviderCapabilityProfile::NonStrictSingle),
    });
    profiles.push(CapabilityProbeProfile {
        profile: ProviderCapabilityProfile::NonStrictSingle,
        contract: make_contract(
            false,
            false,
            direct_tool_count,
            ProviderToolDefinitionMode::Direct,
        ),
        request: make_request(
            probe_tools(direct_tool_count, &tool_schema),
            ToolChoiceMode::Auto,
            1,
            false,
            "First call singularity_capability_probe_a once with arguments {\"probe\":\"schema_sentinel_alpha\",\"values\":[7,7]}. After its tool result, call singularity_capability_probe_a once more with the same arguments.",
        ),
        expected_calls: single_expected(CAPABILITY_PROBE_TOOL_A, Vec::new()),
        single_call_fallback: None,
    });
    profiles.push(CapabilityProbeProfile {
        profile: ProviderCapabilityProfile::RoutedSingle,
        contract: make_contract(false, false, 1, ProviderToolDefinitionMode::Routed),
        request: make_request(
            vec![router_tool],
            ToolChoiceMode::Auto,
            1,
            false,
            "First call singularity_capability_router once with tool_name singularity_capability_probe_a and arguments {\"probe\":\"schema_sentinel_alpha\",\"values\":[7,7]}. After its tool result, make the same router call once more.",
        ),
        expected_calls: single_expected(
            CAPABILITY_PROBE_ROUTER,
            vec![json!({
                "tool_name": CAPABILITY_PROBE_TOOL_A,
                "arguments": strict_arguments
            })],
        ),
        single_call_fallback: None,
    });
    profiles
}

fn capability_probe_profile_match(
    response: &ModelTurnResponse,
    profile: &CapabilityProbeProfile,
) -> Option<ProviderCapabilityProfile> {
    profile
        .single_call_fallback
        .filter(|_| capability_probe_single_call_matches(response, &profile.expected_calls))
        .or_else(|| {
            capability_probe_response_matches(response, &profile.expected_calls)
                .then_some(profile.profile)
        })
}

fn capability_probe_continuation_request(
    profile: &CapabilityProbeProfile,
    response: &ModelTurnResponse,
) -> ModelTurnRequest {
    let mut request = profile.request.clone();
    request.request_id = CAPABILITY_PROBE_CONTINUATION_REQUEST_ID.to_string();
    request.messages.push(ModelMessage::assistant_tool_calls(
        response.tool_calls.clone(),
    ));
    for call in &response.tool_calls {
        let mut message = ModelMessage::text(
            ModelRole::Tool,
            json!({
                "ok": true,
                "tool_name": call.tool_name,
                "tool_call_id": call.tool_call_id,
                "truncated": false,
                "content": {"probe": "completed"}
            })
            .to_string(),
        );
        message.tool_call_id = Some(call.tool_call_id.clone());
        request.messages.push(message);
    }
    request.tool_choice.max_tool_calls = 1;
    request
}

fn capability_probe_continuation_matches(
    response: &ModelTurnResponse,
    profile: &CapabilityProbeProfile,
) -> bool {
    profile.expected_calls.first().is_some_and(|expected| {
        capability_probe_response_matches(response, std::slice::from_ref(expected))
    })
}

fn capability_probe_response_matches(
    response: &ModelTurnResponse,
    expected_calls: &[CapabilityProbeExpectedCall],
) -> bool {
    if response.status != ModelTurnStatus::Success
        || response.tool_calls.len() != expected_calls.len()
    {
        return false;
    }

    let mut matched = vec![false; expected_calls.len()];
    for call in &response.tool_calls {
        if call.parse_status != ModelToolParseStatus::Valid {
            return false;
        }
        let Some(index) = expected_calls
            .iter()
            .enumerate()
            .find_map(|(index, expected)| {
                (!matched[index]
                    && call.tool_name == expected.tool_name
                    && (expected.allowed_arguments.is_empty()
                        || expected.allowed_arguments.contains(&call.arguments)))
                .then_some(index)
            })
        else {
            return false;
        };
        matched[index] = true;
    }
    true
}

fn capability_probe_single_call_matches(
    response: &ModelTurnResponse,
    expected_calls: &[CapabilityProbeExpectedCall],
) -> bool {
    let Some(call) = response.tool_calls.first() else {
        return false;
    };
    response.status == ModelTurnStatus::Success
        && response.tool_calls.len() == 1
        && call.parse_status == ModelToolParseStatus::Valid
        && expected_calls.iter().any(|expected| {
            call.tool_name == expected.tool_name
                && (expected.allowed_arguments.is_empty()
                    || expected.allowed_arguments.contains(&call.arguments))
        })
}

fn add_model_usage(total: &mut ModelUsage, usage: &ModelUsage) {
    total.input_tokens = total.input_tokens.saturating_add(usage.input_tokens);
    total.output_tokens = total.output_tokens.saturating_add(usage.output_tokens);
    total.total_tokens = total.total_tokens.saturating_add(usage.total_tokens);
    total.cached_input_tokens = total
        .cached_input_tokens
        .saturating_add(usage.cached_input_tokens);
    total.reasoning_tokens = total
        .reasoning_tokens
        .saturating_add(usage.reasoning_tokens);
    if let Some(cost) = usage.cost_estimate {
        total.cost_estimate = Some(total.cost_estimate.unwrap_or_default() + cost);
    }
}

fn add_provider_attempt_metadata(
    total: &mut ProviderAttemptMetadata,
    metadata: &ProviderAttemptMetadata,
) {
    total.attempt_count = total.attempt_count.saturating_add(metadata.attempt_count);
    total.retry_count = total.retry_count.saturating_add(metadata.retry_count);
    total.latency_ms = total.latency_ms.saturating_add(metadata.latency_ms);
}

fn provider_response_validation_error(
    config: &OpenAiProviderConfig,
    model_name: &str,
    message: &str,
    validation_errors: Vec<String>,
) -> ProviderError {
    let mut error = ModelError::new(ModelErrorKind::JsonSchemaViolation, message)
        .with_provider(config.provider_name.clone())
        .with_model(model_name.to_string())
        .with_provider_diagnostic(
            "provider_response_invalid",
            ProviderErrorStage::ResponseValidation,
        );
    error.validation_errors = validation_errors;
    ProviderError::from_model_error(error)
}

fn openai_reasoning_content_present(payload: &Value) -> bool {
    match payload.pointer("/choices/0/message/reasoning_content") {
        Some(Value::String(content)) => !content.is_empty(),
        Some(value) => !value.is_null(),
        None => false,
    }
}

fn openai_responses_reasoning_content_present(payload: &Value) -> bool {
    payload
        .get("output")
        .and_then(Value::as_array)
        .is_some_and(|items| {
            items.iter().any(|item| {
                item.get("type").and_then(Value::as_str) == Some("reasoning")
                    && ["content", "summary", "encrypted_content", "text"]
                        .iter()
                        .filter_map(|field| item.get(*field))
                        .any(value_has_content)
            })
        })
}

fn value_has_content(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::String(value) => !value.is_empty(),
        Value::Array(value) => !value.is_empty(),
        Value::Object(value) => !value.is_empty(),
        Value::Bool(_) | Value::Number(_) => true,
    }
}

fn parse_openai_responses_response(
    request: &ModelTurnRequest,
    config: &OpenAiProviderConfig,
    payload: Value,
    capabilities: &ProviderProtocolContract,
    model_name: &str,
) -> Result<ModelTurnResponse, ProviderError> {
    if payload.get("error").is_some_and(|error| !error.is_null()) {
        return Err(provider_response_validation_error(
            config,
            model_name,
            "provider Responses payload contained an error",
            vec!["responses_error_present".to_string()],
        ));
    }
    let status = payload.get("status").and_then(Value::as_str);
    if status != Some("completed") {
        return Err(provider_response_validation_error(
            config,
            model_name,
            "provider Responses payload was not completed",
            vec!["responses_status_not_completed".to_string()],
        ));
    }
    let output = payload
        .get("output")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            provider_response_validation_error(
                config,
                model_name,
                "provider Responses payload missing output items",
                vec!["responses_output_missing".to_string()],
            )
        })?;
    let mut content = String::new();
    let mut tool_calls = Vec::new();
    for (index, item) in output.iter().enumerate() {
        match item.get("type").and_then(Value::as_str) {
            Some("message") => {
                let message = parse_openai_responses_message(item).map_err(|evidence| {
                    provider_response_validation_error(
                        config,
                        model_name,
                        "provider Responses message content was invalid",
                        vec![evidence.to_string()],
                    )
                })?;
                content.push_str(&message);
            }
            Some("function_call") => tool_calls.push(parse_openai_responses_tool_call(index, item)),
            Some("reasoning") if !openai_responses_reasoning_item_has_content(item) => {}
            Some("reasoning") => {
                return Err(provider_response_validation_error(
                    config,
                    model_name,
                    "provider returned reasoning content that cannot be replayed safely",
                    vec!["responses_reasoning_content_present".to_string()],
                ));
            }
            _ => {
                return Err(provider_response_validation_error(
                    config,
                    model_name,
                    "provider Responses payload contained an unsupported output item",
                    vec!["responses_output_item_unsupported".to_string()],
                ));
            }
        }
    }
    let assistant_message = Some(ModelMessage {
        tool_calls: tool_calls.clone(),
        ..ModelMessage::text(ModelRole::Assistant, content)
    });
    finalize_provider_response(
        request,
        config,
        model_name,
        capabilities,
        payload
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("response")
            .to_string(),
        assistant_message,
        tool_calls,
        parse_openai_responses_usage(payload.get("usage")),
        status.map(str::to_string),
    )
}

fn openai_responses_reasoning_item_has_content(item: &Value) -> bool {
    ["content", "summary", "encrypted_content", "text"]
        .iter()
        .filter_map(|field| item.get(*field))
        .any(value_has_content)
}

fn parse_openai_responses_message(message: &Value) -> Result<String, &'static str> {
    match message.get("content") {
        Some(Value::String(text)) => Ok(text.clone()),
        Some(Value::Array(parts)) => {
            let mut content = String::new();
            for part in parts {
                let text = match part.get("type").and_then(Value::as_str) {
                    Some("output_text") | Some("text") => part.get("text").and_then(Value::as_str),
                    Some("refusal") => part.get("refusal").and_then(Value::as_str),
                    _ => return Err("responses_message_content_part_unsupported"),
                }
                .ok_or("responses_message_content_text_missing")?;
                content.push_str(text);
            }
            Ok(content)
        }
        None | Some(Value::Null) => Err("responses_message_content_missing"),
        Some(_) => Err("responses_message_content_invalid"),
    }
}

fn parse_openai_responses_tool_call(_index: usize, call: &Value) -> ModelToolCall {
    let arguments_value = call.get("arguments").unwrap_or(&Value::Null);
    let raw_arguments = match arguments_value {
        Value::String(raw) => raw.clone(),
        Value::Object(_) => serde_json::to_string(arguments_value).unwrap_or_default(),
        _ => String::new(),
    };
    let (arguments, parse_status, validation_errors) = if arguments_value.is_object() {
        (
            arguments_value.clone(),
            ModelToolParseStatus::Valid,
            Vec::new(),
        )
    } else {
        parse_tool_arguments(&raw_arguments)
    };
    ModelToolCall {
        tool_call_id: call
            .get("call_id")
            .or_else(|| call.get("id"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        tool_name: call
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        arguments,
        raw_arguments,
        parse_status,
        validation_errors,
    }
}

fn parse_openai_responses_usage(usage: Option<&Value>) -> ModelUsage {
    let Some(usage) = usage else {
        return ModelUsage::default();
    };
    ModelUsage {
        input_tokens: usage
            .get("input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or_default(),
        output_tokens: usage
            .get("output_tokens")
            .and_then(Value::as_u64)
            .unwrap_or_default(),
        total_tokens: usage
            .get("total_tokens")
            .and_then(Value::as_u64)
            .unwrap_or_default(),
        cached_input_tokens: usage
            .pointer("/input_tokens_details/cached_tokens")
            .and_then(Value::as_u64)
            .unwrap_or_default(),
        reasoning_tokens: usage
            .pointer("/output_tokens_details/reasoning_tokens")
            .and_then(Value::as_u64)
            .unwrap_or_default(),
        cost_estimate: None,
    }
}

fn parse_openai_response(
    request: &ModelTurnRequest,
    config: &OpenAiProviderConfig,
    payload: Value,
    capabilities: &ProviderProtocolContract,
    model_name: &str,
) -> Result<ModelTurnResponse, ProviderError> {
    let response_id = payload
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("response")
        .to_string();
    let choices = payload
        .get("choices")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            provider_response_validation_error(
                config,
                model_name,
                "provider response missing choices",
                vec!["response_choices_missing".to_string()],
            )
        })?;
    if choices.is_empty() {
        return Err(provider_response_validation_error(
            config,
            model_name,
            "provider response missing choices",
            vec!["response_choices_missing".to_string()],
        ));
    }
    if choices.len() != 1 {
        return Err(provider_response_validation_error(
            config,
            model_name,
            "provider response must contain exactly one choice",
            vec!["response_choices_count_invalid".to_string()],
        ));
    }
    let choice = &choices[0];
    let message = choice.get("message").unwrap_or(&Value::Null);
    let content = parse_openai_content(message.get("content"));
    let tool_calls = parse_openai_tool_calls(message);
    let assistant_message = Some(ModelMessage {
        tool_calls: tool_calls.clone(),
        ..ModelMessage::text(ModelRole::Assistant, content)
    });
    let finish_reason = choice
        .get("finish_reason")
        .and_then(Value::as_str)
        .map(str::to_string);
    finalize_provider_response(
        request,
        config,
        model_name,
        capabilities,
        response_id,
        assistant_message,
        tool_calls,
        parse_openai_usage(payload.get("usage")),
        finish_reason,
    )
}

#[allow(clippy::too_many_arguments)]
fn finalize_provider_response(
    request: &ModelTurnRequest,
    config: &OpenAiProviderConfig,
    model_name: &str,
    capabilities: &ProviderProtocolContract,
    response_id: String,
    assistant_message: Option<ModelMessage>,
    tool_calls: Vec<ModelToolCall>,
    usage: ModelUsage,
    finish_reason: Option<String>,
) -> Result<ModelTurnResponse, ProviderError> {
    let mut response = ModelTurnResponse {
        request_id: request.request_id.clone(),
        response_id,
        status: ModelTurnStatus::Success,
        assistant_message,
        tool_calls,
        usage,
        finish_reason,
        validation: None,
        error: None,
        provider_name: Some(config.provider_name.clone()),
        model_name: Some(model_name.to_string()),
        provider_attempt_metadata: None,
    };
    let available_tool_names = request
        .tools
        .iter()
        .map(|tool| tool.name.clone())
        .collect::<Vec<_>>();
    let validation = validate_model_turn_response(
        request,
        &response,
        &available_tool_names,
        Some(capabilities),
    );
    if !validation.valid {
        response.status = ModelTurnStatus::Invalid;
        let provider_rejected_parallelism = validation
            .errors
            .iter()
            .any(|error| error == "provider_does_not_support_parallel_tool_calls");
        let (kind, message, diagnostic_code) = if provider_rejected_parallelism {
            (
                ModelErrorKind::UnsupportedCapability,
                "provider does not support parallel tool calls".to_string(),
                "provider_does_not_support_parallel_tool_calls",
            )
        } else {
            (
                ModelErrorKind::JsonSchemaViolation,
                format!("provider_response_invalid: {}", validation.errors.join(",")),
                "provider_response_invalid",
            )
        };
        response.error = Some(
            ModelError::new(kind, message)
                .with_provider(config.provider_name.clone())
                .with_model(model_name.to_string())
                .with_provider_diagnostic(diagnostic_code, ProviderErrorStage::ResponseValidation),
        );
        if let Some(error) = response.error.as_mut() {
            error.validation_errors = validation.errors.clone();
        }
    }
    response.validation = Some(validation);
    Ok(response)
}

fn parse_openai_tool_calls(message: &Value) -> Vec<ModelToolCall> {
    message
        .get("tool_calls")
        .and_then(Value::as_array)
        .map(|calls| {
            calls
                .iter()
                .enumerate()
                .map(|(index, call)| parse_openai_tool_call(index, call))
                .collect()
        })
        .unwrap_or_default()
}

fn parse_openai_tool_call(_index: usize, call: &Value) -> ModelToolCall {
    let function = call.get("function").unwrap_or(&Value::Null);
    let arguments_value = function.get("arguments").unwrap_or(&Value::Null);
    let raw_arguments = match arguments_value {
        Value::String(raw) => raw.clone(),
        Value::Object(_) => serde_json::to_string(arguments_value).unwrap_or_default(),
        _ => String::new(),
    };
    let (arguments, parse_status, validation_errors) = if arguments_value.is_object() {
        (
            arguments_value.clone(),
            ModelToolParseStatus::Valid,
            Vec::new(),
        )
    } else {
        parse_tool_arguments(&raw_arguments)
    };
    let wire_tool_name = function.get("name").and_then(Value::as_str).unwrap_or("");
    ModelToolCall {
        tool_call_id: call
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_default(),
        tool_name: wire_tool_name.to_string(),
        arguments,
        raw_arguments,
        parse_status,
        validation_errors,
    }
}

fn parse_tool_arguments(raw_arguments: &str) -> (Value, ModelToolParseStatus, Vec<String>) {
    match serde_json::from_str::<Value>(raw_arguments) {
        Ok(arguments) if arguments.is_object() => {
            (arguments, ModelToolParseStatus::Valid, Vec::new())
        }
        Ok(arguments) => (
            arguments,
            ModelToolParseStatus::SchemaMismatch,
            vec!["tool_call_arguments_must_be_object".to_string()],
        ),
        Err(_) => (
            json!({}),
            ModelToolParseStatus::InvalidJson,
            vec!["invalid_json".to_string()],
        ),
    }
}

fn parse_openai_content(content: Option<&Value>) -> String {
    match content {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| {
                (part.get("type").and_then(Value::as_str) == Some("text"))
                    .then(|| part.get("text").and_then(Value::as_str))
                    .flatten()
            })
            .collect::<Vec<_>>()
            .join(""),
        Some(_) => String::new(),
    }
}

fn parse_openai_usage(usage: Option<&Value>) -> ModelUsage {
    let Some(usage) = usage else {
        return ModelUsage::default();
    };
    ModelUsage {
        input_tokens: usage
            .get("prompt_tokens")
            .and_then(Value::as_u64)
            .unwrap_or_default(),
        output_tokens: usage
            .get("completion_tokens")
            .and_then(Value::as_u64)
            .unwrap_or_default(),
        total_tokens: usage
            .get("total_tokens")
            .and_then(Value::as_u64)
            .unwrap_or_default(),
        cached_input_tokens: usage
            .pointer("/prompt_tokens_details/cached_tokens")
            .and_then(Value::as_u64)
            .unwrap_or_default(),
        reasoning_tokens: usage
            .pointer("/completion_tokens_details/reasoning_tokens")
            .and_then(Value::as_u64)
            .unwrap_or_default(),
        cost_estimate: None,
    }
}

fn missing_provider_config_error(
    name: &str,
    source: Option<ProviderConfigSource>,
) -> ProviderError {
    let source = source.map_or("unconfigured", ProviderConfigSource::as_str);
    ProviderError::from_model_error(
        ModelError::new(
            ModelErrorKind::InvalidRequest,
            format!("required provider configuration is missing: {name} (source={source})"),
        )
        .with_provider_diagnostic(
            "provider_configuration_missing",
            ProviderErrorStage::ClientInitialization,
        ),
    )
}

fn provider_source_missing_error() -> ProviderError {
    ProviderError::from_model_error(
        ModelError::new(
            ModelErrorKind::InvalidRequest,
            "provider configuration source is missing",
        )
        .with_provider_diagnostic(
            "provider_configuration_missing",
            ProviderErrorStage::ClientInitialization,
        ),
    )
}

fn parse_provider_limit(
    value: Option<&str>,
    name: &str,
    fallback: u32,
    upper_bound: u32,
    source: Option<ProviderConfigSource>,
) -> Result<u32, ProviderError> {
    let Some(value) = value else {
        return Ok(fallback);
    };
    let parsed = value.trim().parse::<u32>().ok().filter(|value| *value > 0);
    match parsed {
        Some(value) if value <= upper_bound => Ok(value),
        _ => {
            let source = source.map_or("unconfigured", ProviderConfigSource::as_str);
            Err(ProviderError::from_model_error(
                ModelError::new(
                    ModelErrorKind::InvalidRequest,
                    format!(
                        "invalid model configuration: {name} must be between 1 and {upper_bound} (source={source})"
                    ),
                )
                .with_provider_diagnostic(
                    "provider_configuration_invalid",
                    ProviderErrorStage::ClientInitialization,
                ),
            ))
        }
    }
}

fn model_error_from_http_status(status: u16, provider_name: &str, model_name: &str) -> ModelError {
    let kind = match status {
        HTTP_STATUS_UNAUTHORIZED | HTTP_STATUS_FORBIDDEN => ModelErrorKind::AuthError,
        HTTP_STATUS_REQUEST_TIMEOUT => ModelErrorKind::Timeout,
        HTTP_STATUS_NOT_FOUND => ModelErrorKind::InvalidRequest,
        HTTP_STATUS_RATE_LIMITED => ModelErrorKind::RateLimited,
        status if status >= HTTP_STATUS_INTERNAL_SERVER_ERROR => ModelErrorKind::ProviderOverloaded,
        _ => ModelErrorKind::UnknownProviderError,
    };
    let message = format!("Provider returned HTTP {status}.");
    let mut error = ModelError::new(kind, message)
        .with_provider(provider_name.to_string())
        .with_model(model_name.to_string())
        .with_provider_diagnostic("provider_http_status", ProviderErrorStage::ResponseStatus);
    error.http_status = Some(status);
    error
}

fn provider_transport_error(
    error: reqwest::Error,
    code: &'static str,
    stage: ProviderErrorStage,
    request_timeout_seconds: Option<u64>,
) -> ProviderError {
    let kind = if error.is_timeout() {
        ModelErrorKind::Timeout
    } else {
        ModelErrorKind::NetworkError
    };
    let category = if error.is_timeout() {
        ProviderTransportCategory::Timeout
    } else if error.is_connect() {
        ProviderTransportCategory::Connect
    } else if error.is_request() {
        ProviderTransportCategory::Request
    } else if error.is_body() {
        ProviderTransportCategory::BodyRead
    } else {
        ProviderTransportCategory::Unknown
    };
    let mut model_error =
        ModelError::new(kind, "provider transport failed").with_provider_diagnostic(code, stage);
    model_error.transport_category = Some(category);
    if error.is_timeout() {
        model_error.timeout_seconds = request_timeout_seconds;
    }
    ProviderError::from_model_error(model_error)
}

fn provider_runtime_error(_error: std::io::Error) -> ProviderError {
    ProviderError::from_model_error(
        ModelError::new(
            ModelErrorKind::UnknownProviderError,
            "provider runtime initialization failed",
        )
        .with_provider_diagnostic(
            "provider_runtime_initialization_failed",
            ProviderErrorStage::ClientInitialization,
        ),
    )
}

fn provider_client_initialization_error(error: reqwest::Error) -> ProviderError {
    provider_transport_error(
        error,
        "provider_client_initialization_failed",
        ProviderErrorStage::ClientInitialization,
        None,
    )
}

fn provider_cancelled_error() -> ProviderError {
    ProviderError::from_model_error(
        ModelError::new(ModelErrorKind::Cancelled, "provider request cancelled")
            .with_provider_diagnostic("provider_request_cancelled", ProviderErrorStage::Cancelled),
    )
}

fn capability_probe_cancelled_error() -> ProviderError {
    provider_cancelled_error().with_capability_metadata(capability_probe_metadata(
        ProviderApiProtocol::Declared,
        ProviderCapabilityProfile::Declared,
        0,
        0,
        &ModelUsage::default(),
        &ProviderAttemptMetadata::zero(),
    ))
}

fn provider_capability_cache_error() -> ProviderError {
    ProviderError::from_model_error(
        ModelError::new(
            ModelErrorKind::UnknownProviderError,
            "provider capability cache is unavailable",
        )
        .with_provider_diagnostic(
            "provider_capability_cache_unavailable",
            ProviderErrorStage::ClientInitialization,
        ),
    )
}

fn capability_probe_definition_error(errors: Vec<String>) -> ProviderError {
    let mut error = ModelError::new(
        ModelErrorKind::UnknownProviderError,
        "provider capability probe definition is invalid",
    )
    .with_provider_diagnostic(
        "provider_capability_probe_definition_invalid",
        ProviderErrorStage::RequestSend,
    );
    error.validation_errors = errors;
    ProviderError::from_model_error(error)
}

fn capability_probe_metadata(
    api_protocol: ProviderApiProtocol,
    profile: ProviderCapabilityProfile,
    profile_attempts: u32,
    fallback_count: u32,
    probe_usage: &ModelUsage,
    probe_attempt_metadata: &ProviderAttemptMetadata,
) -> ProviderCapabilityMetadata {
    ProviderCapabilityMetadata {
        api_protocol,
        profile,
        cache_hit: false,
        profile_attempts,
        fallback_count,
        probe_usage: probe_usage.clone(),
        probe_attempt_metadata: probe_attempt_metadata.clone(),
    }
}

fn capability_probe_failure(
    error: ProviderError,
    metadata: ProviderCapabilityMetadata,
    evidence: &str,
) -> ProviderError {
    let provider_attempt_metadata = error.provider_attempt_metadata.clone();
    let mut model_error = *error.error;
    if !model_error
        .validation_errors
        .iter()
        .any(|existing| existing == evidence)
    {
        model_error.validation_errors.push(evidence.to_string());
    }
    let provider_error = ProviderError::from_model_error(model_error);
    let provider_error = if let Some(metadata) = provider_attempt_metadata {
        provider_error.with_provider_attempt_metadata(metadata)
    } else {
        provider_error
    };
    provider_error.with_capability_metadata(metadata)
}

fn cache_hit_negotiation(
    mut negotiation: ProviderProtocolNegotiation,
) -> ProviderProtocolNegotiation {
    negotiation.metadata.cache_hit = true;
    negotiation.metadata.profile_attempts = 0;
    negotiation.metadata.fallback_count = 0;
    negotiation.metadata.probe_usage = ModelUsage::default();
    negotiation.metadata.probe_attempt_metadata = ProviderAttemptMetadata::zero();
    negotiation
}

fn merge_capability_metadata(
    target: &mut ProviderCapabilityMetadata,
    previous: &ProviderCapabilityMetadata,
) {
    target.profile_attempts = target
        .profile_attempts
        .saturating_add(previous.profile_attempts);
    target.fallback_count = target
        .fallback_count
        .saturating_add(previous.fallback_count);
    add_model_usage(&mut target.probe_usage, &previous.probe_usage);
    add_provider_attempt_metadata(
        &mut target.probe_attempt_metadata,
        &previous.probe_attempt_metadata,
    );
}

fn provider_protocol_fallback_allowed(error: &ProviderError) -> bool {
    error.error.kind == ModelErrorKind::UnsupportedCapability
        || (error.error.stage == Some(ProviderErrorStage::ResponseStatus)
            && matches!(
                error.error.http_status,
                Some(
                    HTTP_STATUS_BAD_REQUEST
                        | HTTP_STATUS_NOT_FOUND
                        | HTTP_STATUS_UNPROCESSABLE_ENTITY
                )
            ))
}

fn capability_probe_unsupported_error(mut error: ModelError) -> ProviderError {
    error.kind = ModelErrorKind::UnsupportedCapability;
    error.message = "provider does not support native structured tool calls".to_string();
    if error.code.is_none() {
        error.code = Some("provider_native_structured_tool_calls_unsupported".to_string());
    }
    if error.stage.is_none() {
        error.stage = Some(ProviderErrorStage::ResponseValidation);
    }
    ProviderError::from_model_error(error)
}

fn capability_probe_response_error(response: &ModelTurnResponse) -> ProviderError {
    let mut error = response.error.as_ref().cloned().unwrap_or_else(|| {
        ModelError::new(
            ModelErrorKind::UnsupportedCapability,
            "provider capability probe did not return native structured tool calls",
        )
        .with_provider_diagnostic(
            "provider_native_structured_tool_calls_unsupported",
            ProviderErrorStage::ResponseValidation,
        )
    });
    if let Some(validation) = &response.validation {
        error.validation_errors = validation.errors.clone();
    }
    let explicit_capability_violation = error.kind == ModelErrorKind::UnsupportedCapability
        || error.validation_errors.iter().any(|validation_error| {
            matches!(
                validation_error.as_str(),
                "provider_does_not_support_tools"
                    | "provider_does_not_support_strict_tool_schema"
                    | "provider_does_not_support_parallel_tool_calls"
                    | "max_tool_calls_exceeded"
            )
        });
    if !response.tool_calls.is_empty() && !explicit_capability_violation {
        return ProviderError::from_model_error(error);
    }
    if response.tool_calls.is_empty()
        && !error
            .validation_errors
            .iter()
            .any(|error| error == "capability_probe_native_tool_calls_missing")
    {
        error
            .validation_errors
            .push("capability_probe_native_tool_calls_missing".to_string());
    }
    capability_probe_unsupported_error(error)
}

fn capability_probe_continuation_error(response: &ModelTurnResponse) -> ProviderError {
    let mut error = capability_probe_response_error(response);
    if !error
        .error
        .validation_errors
        .iter()
        .any(|existing| existing == "capability_probe_multi_turn_tool_calls_missing")
    {
        error
            .error
            .validation_errors
            .push("capability_probe_multi_turn_tool_calls_missing".to_string());
    }
    error
}

fn capability_probe_tool_reasoning_error(
    response: &ModelTurnResponse,
    evidence: &str,
) -> ProviderError {
    let mut error = response.error.as_ref().cloned().unwrap_or_else(|| {
        ModelError::new(
            ModelErrorKind::UnsupportedCapability,
            "provider cannot stabilize native tool calls with reasoning disabled",
        )
        .with_provider_diagnostic(
            "provider_tool_reasoning_mode_unsupported",
            ProviderErrorStage::ResponseValidation,
        )
    });
    if let Some(validation) = &response.validation {
        error.validation_errors = validation.errors.clone();
    }
    if !error
        .validation_errors
        .iter()
        .any(|existing| existing == evidence)
    {
        error.validation_errors.push(evidence.to_string());
    }
    let provider_error = ProviderError::from_model_error(error);
    if let Some(metadata) = &response.provider_attempt_metadata {
        provider_error.with_provider_attempt_metadata(metadata.clone())
    } else {
        provider_error
    }
}

fn provider_tool_reasoning_history_error(
    response: &ModelTurnResponse,
    mode: ProviderToolReasoningMode,
) -> ProviderError {
    let (code, evidence) = if mode == ProviderToolReasoningMode::DisabledForToolCalls {
        (
            "provider_tool_reasoning_mode_not_honored",
            "tool_reasoning_disable_not_honored",
        )
    } else {
        (
            "provider_tool_reasoning_history_unsupported",
            "tool_reasoning_content_requires_adapter_history_support",
        )
    };
    let mut error = ModelError::new(
        ModelErrorKind::UnsupportedCapability,
        "provider returned tool reasoning that cannot be safely replayed",
    )
    .with_provider_diagnostic(code, ProviderErrorStage::ResponseValidation);
    error.validation_errors.push(evidence.to_string());
    let provider_error = ProviderError::from_model_error(error);
    if let Some(metadata) = &response.provider_attempt_metadata {
        provider_error.with_provider_attempt_metadata(metadata.clone())
    } else {
        provider_error
    }
}

fn capability_probe_tool_reasoning_rejection(error: ProviderError) -> ProviderError {
    let provider_attempt_metadata = error.provider_attempt_metadata.clone();
    let mut model_error = *error.error;
    model_error.kind = ModelErrorKind::UnsupportedCapability;
    model_error.message =
        "provider does not support disabling reasoning for native tool calls".to_string();
    model_error.code = Some("provider_tool_reasoning_mode_unsupported".to_string());
    let provider_error = ProviderError::from_model_error(model_error);
    if let Some(metadata) = provider_attempt_metadata {
        provider_error.with_provider_attempt_metadata(metadata)
    } else {
        provider_error
    }
}

fn is_capability_probe_profile_rejection(error: &ProviderError) -> bool {
    error.error.stage == Some(ProviderErrorStage::ResponseStatus)
        && matches!(
            error.error.http_status,
            Some(HTTP_STATUS_BAD_REQUEST | HTTP_STATUS_UNPROCESSABLE_ENTITY)
        )
}

fn validation_is_unsupported_capability(validation: &ModelValidationResult) -> bool {
    !validation.errors.is_empty()
        && validation.errors.iter().all(|error| {
            matches!(
                error.as_str(),
                "provider_does_not_support_tools"
                    | REQUIRED_TOOL_CHOICE_UNSUPPORTED_ERROR
                    | "provider_does_not_support_strict_tool_schema"
                    | "provider_does_not_support_parallel_tool_calls"
            )
        })
}

fn provider_attempt_metadata(
    metadata: &ProviderAttemptMetadata,
    started_at: Instant,
) -> ProviderAttemptMetadata {
    ProviderAttemptMetadata {
        attempt_count: metadata.attempt_count,
        retry_count: metadata.retry_count,
        latency_ms: started_at.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
    }
}

fn provider_error_is_retryable(error: &ProviderError) -> bool {
    matches!(
        error.error.kind,
        ModelErrorKind::NetworkError | ModelErrorKind::Timeout
    ) && !matches!(
        error.error.transport_category,
        Some(ProviderTransportCategory::Request)
    )
}

fn http_status_is_retryable(status: u16) -> bool {
    status == HTTP_STATUS_RATE_LIMITED || status >= HTTP_STATUS_INTERNAL_SERVER_ERROR
}

fn provider_retry_backoff(retry_count: u32) -> Duration {
    let shift = retry_count.saturating_sub(1).min(10);
    let multiplier = 1_u64 << shift;
    Duration::from_millis(PROVIDER_RETRY_BASE_BACKOFF_MS.saturating_mul(multiplier))
}

fn wait_provider_backoff(
    runtime: &tokio::runtime::Runtime,
    cancellation: &CancellationToken,
    duration: Duration,
) -> Result<(), ProviderError> {
    let deadline = Instant::now() + duration;
    loop {
        if cancellation.is_cancelled() {
            return Err(provider_cancelled_error());
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(());
        }
        let poll = remaining.min(Duration::from_millis(PROVIDER_CANCELLATION_POLL_MS));
        runtime.block_on(async {
            tokio::time::sleep(poll).await;
        });
    }
}

fn block_on_provider_future<C, F, T>(
    runtime: &tokio::runtime::Runtime,
    cancellation: &CancellationToken,
    error_code: &'static str,
    error_stage: ProviderErrorStage,
    request_timeout_seconds: u64,
    create_future: C,
) -> Result<T, ProviderError>
where
    C: FnOnce() -> F,
    F: Future<Output = Result<T, reqwest::Error>>,
{
    let mut future = {
        let _runtime_context = runtime.enter();
        Box::pin(create_future())
    };
    loop {
        if cancellation.is_cancelled() {
            return Err(provider_cancelled_error());
        }
        match runtime.block_on(async {
            tokio::time::timeout(
                Duration::from_millis(PROVIDER_CANCELLATION_POLL_MS),
                future.as_mut(),
            )
            .await
        }) {
            Ok(result) => {
                return result.map_err(|error| {
                    provider_transport_error(
                        error,
                        error_code,
                        error_stage.clone(),
                        Some(request_timeout_seconds),
                    )
                });
            }
            Err(_) => continue,
        }
    }
}

fn read_bounded_provider_response_body(
    runtime: &tokio::runtime::Runtime,
    cancellation: &CancellationToken,
    request_timeout_seconds: u64,
    mut response: reqwest::Response,
) -> Result<Vec<u8>, ProviderError> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_PROVIDER_RESPONSE_BODY_BYTES as u64)
    {
        return Err(provider_response_body_too_large_error());
    }
    let initial_capacity = response
        .content_length()
        .and_then(|length| usize::try_from(length).ok())
        .unwrap_or_default()
        .min(MAX_PROVIDER_RESPONSE_BODY_BYTES);
    let mut body = Vec::with_capacity(initial_capacity);
    loop {
        let chunk = block_on_provider_future(
            runtime,
            cancellation,
            "provider_response_body_read_failed",
            ProviderErrorStage::ResponseBodyRead,
            request_timeout_seconds,
            || response.chunk(),
        )?;
        let Some(chunk) = chunk else {
            return Ok(body);
        };
        if body.len().saturating_add(chunk.len()) > MAX_PROVIDER_RESPONSE_BODY_BYTES {
            return Err(provider_response_body_too_large_error());
        }
        body.extend_from_slice(&chunk);
    }
}

fn provider_response_body_too_large_error() -> ProviderError {
    let mut error = ModelError::new(
        ModelErrorKind::JsonSchemaViolation,
        "provider response body exceeded the fixed safety limit",
    )
    .with_provider_diagnostic(
        "provider_response_body_too_large",
        ProviderErrorStage::ResponseBodyRead,
    );
    error.validation_errors = vec!["provider_response_body_too_large".to_string()];
    ProviderError::from_model_error(error)
}

fn provider_response_json_error() -> ModelError {
    ModelError::new(
        ModelErrorKind::JsonSchemaViolation,
        "provider response was not valid JSON",
    )
    .with_provider_diagnostic(
        "provider_response_json_decode_failed",
        ProviderErrorStage::ResponseJsonDecode,
    )
}

/// Checks whether the redacted provider configuration contains all required values.
pub fn validate_provider_config(config: &ModelProviderConfig) -> ModelValidationResult {
    let mut errors = Vec::new();
    if missing(&config.provider_name) {
        errors.push("provider_name_required".to_string());
    }
    if missing(&config.model_name) {
        errors.push("model_name_required".to_string());
    }
    if !config.base_url_present {
        errors.push("base_url_required".to_string());
    }
    if !config.api_key_present {
        errors.push("api_key_required".to_string());
    }
    validation_result(errors, Vec::new())
}

/// Validates a model request before provider-specific capability checks are applied.
pub fn validate_model_request(request: &ModelTurnRequest) -> ModelValidationResult {
    validate_model_request_with_capabilities(request, None)
}

pub fn validate_model_request_with_capabilities(
    request: &ModelTurnRequest,
    capabilities: Option<&ProviderProtocolContract>,
) -> ModelValidationResult {
    let mut errors = Vec::new();
    if request.request_id.trim().is_empty() {
        errors.push("request_id_required".to_string());
    }
    if request.messages.is_empty() {
        errors.push("messages_required".to_string());
    }
    if !request.tools.is_empty() && request.tool_choice.max_tool_calls == 0 {
        errors.push("max_tool_calls_must_be_positive".to_string());
    }
    if request.tool_choice.mode == ToolChoiceMode::Required && request.tools.is_empty() {
        errors.push(REQUIRED_TOOL_CHOICE_REQUIRES_TOOLS_ERROR.to_string());
    }
    let request_uses_nonportable_tool_name = request
        .tools
        .iter()
        .map(|tool| tool.name.as_str())
        .chain(
            request
                .messages
                .iter()
                .flat_map(|message| message.tool_calls.iter())
                .map(|call| call.tool_name.as_str()),
        )
        .any(|name| !is_portable_tool_name(name));
    if request_uses_nonportable_tool_name {
        errors.push("tool_name_not_provider_portable".to_string());
    }
    let mut tool_names = HashSet::new();
    if request
        .tools
        .iter()
        .any(|tool| !tool_names.insert(tool.name.as_str()))
    {
        errors.push("tool_names_must_be_unique".to_string());
    }
    if request.tool_choice.strict_tool_schema
        && request
            .tools
            .iter()
            .any(|tool| !is_strict_tool_schema_compatible(&tool.parameters_schema))
    {
        errors.push("strict_tool_schema_incompatible".to_string());
    }
    if let Some(capabilities) = capabilities {
        if !request.tools.is_empty() && !capabilities.supports_tools {
            errors.push("provider_does_not_support_tools".to_string());
        }
        if request.tool_choice.mode == ToolChoiceMode::Required
            && !capabilities.supports_required_tool_choice
        {
            errors.push(REQUIRED_TOOL_CHOICE_UNSUPPORTED_ERROR.to_string());
        }
        if request.model_preferences.json_mode && !capabilities.supports_json_mode {
            errors.push("provider_does_not_support_json_mode".to_string());
        }
        if request
            .messages
            .iter()
            .any(|message| message.role == ModelRole::System)
            && !capabilities.supports_system_message
        {
            errors.push("provider_does_not_support_system_messages".to_string());
        }
        if request
            .messages
            .iter()
            .any(|message| message.role == ModelRole::Developer)
            && !capabilities.supports_developer_message
        {
            errors.push("provider_does_not_support_developer_messages".to_string());
        }
        if let Some(requested_output_tokens) = request.model_preferences.max_output_tokens
            && requested_output_tokens > capabilities.max_output_tokens
        {
            errors.push("requested_output_tokens_exceed_provider_limit".to_string());
        }
        if !request.tools.is_empty()
            && request.tool_choice.max_tool_calls > 1
            && !capabilities.supports_parallel_tool_calls
        {
            errors.push("provider_does_not_support_parallel_tool_calls".to_string());
        }
        if request.tools.len() > capabilities.max_tools_per_request as usize {
            errors.push("requested_tools_exceed_provider_limit".to_string());
        }
        if !request.tools.is_empty()
            && request.tool_choice.strict_tool_schema
            && !capabilities.supports_strict_tool_schema
        {
            errors.push("provider_does_not_support_strict_tool_schema".to_string());
        }
    }
    validation_result(errors, Vec::new())
}

fn is_portable_tool_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
}

/// Reports whether a JSON Schema can be sent under the provider's strict tool-schema contract.
pub fn is_strict_tool_schema_compatible(schema: &Value) -> bool {
    if schema.get("const").is_some() {
        return true;
    }
    if let Some(branches) = schema.get("oneOf").and_then(Value::as_array) {
        return !branches.is_empty() && branches.iter().all(is_strict_tool_schema_compatible);
    }
    if schema.get("type").and_then(Value::as_str) == Some("object") {
        let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
            return false;
        };
        let Some(required) = schema.get("required").and_then(Value::as_array) else {
            return false;
        };
        if schema.get("additionalProperties").and_then(Value::as_bool) != Some(false)
            || required.iter().any(|name| {
                name.as_str()
                    .is_none_or(|name| !properties.contains_key(name))
            })
            || properties.keys().any(|name| {
                !required
                    .iter()
                    .any(|required| required.as_str() == Some(name.as_str()))
            })
        {
            return false;
        }
        return properties.values().all(is_strict_tool_schema_compatible);
    }
    if schema.get("type").and_then(Value::as_str) == Some("array") {
        return schema
            .get("items")
            .is_some_and(is_strict_tool_schema_compatible);
    }
    match schema.get("type") {
        Some(Value::String(value_type)) => {
            matches!(
                value_type.as_str(),
                "string" | "number" | "integer" | "boolean" | "null"
            )
        }
        Some(Value::Array(value_types)) => {
            !value_types.is_empty()
                && value_types.iter().all(|value_type| {
                    value_type.as_str().is_some_and(|value_type| {
                        matches!(
                            value_type,
                            "string" | "number" | "integer" | "boolean" | "null"
                        )
                    })
                })
        }
        _ => false,
    }
}

/// Validates a complete provider turn against its request and negotiated capabilities.
pub fn validate_model_turn_response(
    request: &ModelTurnRequest,
    response: &ModelTurnResponse,
    available_tool_names: &[String],
    capabilities: Option<&ProviderProtocolContract>,
) -> ModelValidationResult {
    let mut result = validate_model_response_with_protocol_context(
        response.assistant_message.as_ref(),
        &response.tool_calls,
        &request.tool_choice,
        available_tool_names,
        capabilities,
        request_uses_tool_protocol(request),
    );
    if response.request_id != request.request_id {
        result
            .errors
            .push("response_request_id_mismatch".to_string());
    }
    if response.response_id.trim().is_empty() {
        result.errors.push("response_id_required".to_string());
    }
    if response.status == ModelTurnStatus::Success && response.error.is_some() {
        result
            .errors
            .push("successful_response_has_error".to_string());
    }
    if let Some(capabilities) = capabilities
        && response.usage.output_tokens > u64::from(capabilities.max_output_tokens)
    {
        result
            .errors
            .push("response_output_tokens_exceed_provider_limit".to_string());
    }
    result.errors.sort();
    result.errors.dedup();
    result.valid = result.errors.is_empty();
    result
}

/// Validates parsed provider content and tool calls before the AgentLoop acts on them.
pub fn validate_model_response(
    assistant_message: Option<&ModelMessage>,
    tool_calls: &[ModelToolCall],
    tool_choice: &ToolChoicePolicy,
    available_tool_names: &[String],
    capabilities: Option<&ProviderProtocolContract>,
) -> ModelValidationResult {
    validate_model_response_with_protocol_context(
        assistant_message,
        tool_calls,
        tool_choice,
        available_tool_names,
        capabilities,
        !available_tool_names.is_empty(),
    )
}

fn validate_model_response_with_protocol_context(
    assistant_message: Option<&ModelMessage>,
    tool_calls: &[ModelToolCall],
    tool_choice: &ToolChoicePolicy,
    available_tool_names: &[String],
    capabilities: Option<&ProviderProtocolContract>,
    tool_protocol_active: bool,
) -> ModelValidationResult {
    let mut errors = Vec::new();
    let mut warnings = Vec::new();

    match assistant_message {
        Some(message) if message.role != ModelRole::Assistant => {
            errors.push("non_assistant_response".to_string());
        }
        Some(message)
            if tool_calls.is_empty()
                && tool_protocol_active
                && is_text_tool_call_envelope(message_text(message)) =>
        {
            errors.push(TEXT_TOOL_CALL_ENVELOPE_ERROR.to_string());
        }
        Some(message) if message_text(message).trim().is_empty() && tool_calls.is_empty() => {
            errors.push("empty_response".to_string());
        }
        Some(_) => {}
        None => errors.push("missing_assistant_message".to_string()),
    }

    match tool_choice.mode {
        ToolChoiceMode::None if !tool_calls.is_empty() => {
            errors.push("tool_choice_none".to_string());
        }
        ToolChoiceMode::Required
            if tool_calls.is_empty()
                && capabilities
                    .is_none_or(|capabilities| capabilities.supports_required_tool_choice) =>
        {
            errors.push(REQUIRED_TOOL_CHOICE_MISSING_ERROR.to_string());
        }
        _ => {}
    }
    if tool_calls.len() > tool_choice.max_tool_calls as usize {
        errors.push("max_tool_calls_exceeded".to_string());
    }
    if let Some(capabilities) = capabilities {
        if tool_choice.mode == ToolChoiceMode::Required
            && !capabilities.supports_required_tool_choice
        {
            errors.push(REQUIRED_TOOL_CHOICE_UNSUPPORTED_ERROR.to_string());
        }
        if !tool_calls.is_empty() && !capabilities.supports_tools {
            errors.push("provider_does_not_support_tools".to_string());
        }
        if tool_calls.len() > 1 && !capabilities.supports_parallel_tool_calls {
            errors.push("provider_does_not_support_parallel_tool_calls".to_string());
        }
    }

    let mut seen = HashSet::new();
    for call in tool_calls {
        if call.tool_call_id.trim().is_empty() {
            errors.push("missing_tool_call_id".to_string());
        } else if !seen.insert(call.tool_call_id.as_str()) {
            errors.push("duplicate_tool_call_id".to_string());
        }
        if call.tool_name.trim().is_empty() {
            errors.push("missing_tool_name".to_string());
        }
        if !call.arguments.is_object() {
            errors.push("tool_call_arguments_must_be_object".to_string());
        }
        let unknown_tool = !available_tool_names
            .iter()
            .any(|name| name == &call.tool_name)
            || call.parse_status == ModelToolParseStatus::UnknownTool;
        match call.parse_status {
            ModelToolParseStatus::InvalidJson => errors.push("invalid_json".to_string()),
            ModelToolParseStatus::SchemaMismatch => errors.push("schema_mismatch".to_string()),
            ModelToolParseStatus::UnknownTool => {}
            ModelToolParseStatus::Valid => {}
        }
        if unknown_tool {
            warnings.push("unknown_tool".to_string());
        }
        warnings.extend(call.validation_errors.iter().cloned());
    }

    validation_result(errors, warnings)
}

fn model_error_category(error: &ModelError) -> ModelErrorCategory {
    match error.kind {
        ModelErrorKind::Cancelled => ModelErrorCategory::Cancelled,
        ModelErrorKind::AuthError => ModelErrorCategory::Authentication,
        ModelErrorKind::NetworkError | ModelErrorKind::Timeout => ModelErrorCategory::Network,
        ModelErrorKind::InvalidRequest
            if error.stage == Some(ProviderErrorStage::ClientInitialization)
                && matches!(
                    error.code.as_deref(),
                    Some("provider_configuration_missing" | "provider_configuration_invalid")
                ) =>
        {
            ModelErrorCategory::ModelConfiguration
        }
        ModelErrorKind::InvalidRequest => ModelErrorCategory::InvalidRequest,
        ModelErrorKind::ContextLengthExceeded => ModelErrorCategory::ContextLengthExceeded,
        ModelErrorKind::BudgetExceeded => ModelErrorCategory::BudgetExceeded,
        ModelErrorKind::ToolCallParseError => ModelErrorCategory::ToolCallParse,
        ModelErrorKind::JsonSchemaViolation => ModelErrorCategory::JsonSchema,
        ModelErrorKind::ContentFilter => ModelErrorCategory::ContentFilter,
        ModelErrorKind::UnsupportedCapability => ModelErrorCategory::UnsupportedCapability,
        ModelErrorKind::RateLimited | ModelErrorKind::ProviderOverloaded => {
            ModelErrorCategory::ProviderUnavailable
        }
        ModelErrorKind::UnknownProviderError => ModelErrorCategory::UnknownProviderError,
    }
}

fn validation_result(mut errors: Vec<String>, warnings: Vec<String>) -> ModelValidationResult {
    errors.sort();
    errors.dedup();
    if errors.is_empty() {
        ModelValidationResult {
            warnings,
            ..ModelValidationResult::valid()
        }
    } else {
        ModelValidationResult {
            warnings,
            ..ModelValidationResult::invalid(errors)
        }
    }
}

fn message_text(message: &ModelMessage) -> &str {
    &message.content
}

fn is_text_tool_call_envelope(text: &str) -> bool {
    text.find("<tool_call>")
        .is_some_and(|start| text[start + "<tool_call>".len()..].contains("</tool_call>"))
}

fn missing(value: &Option<String>) -> bool {
    value.as_deref().map(str::trim).unwrap_or("").is_empty()
}

#[derive(Default)]
struct ProviderConfigLayer {
    provider_name: Option<String>,
    model_name: Option<String>,
    context_tokens: Option<String>,
    max_output_tokens: Option<String>,
    base_url: Option<String>,
    api_key: Option<String>,
}

impl ProviderConfigLayer {
    fn from_process_env<F>(get_env: &mut F) -> Self
    where
        F: FnMut(&str) -> Option<String>,
    {
        Self {
            provider_name: get_env(ENV_PROVIDER),
            model_name: get_env(ENV_MODEL),
            context_tokens: get_env(ENV_CONTEXT_TOKENS),
            max_output_tokens: get_env(ENV_MAX_OUTPUT_TOKENS),
            base_url: get_env(ENV_BASE_URL),
            api_key: get_env(ENV_API_KEY),
        }
    }

    fn any_present(&self) -> bool {
        self.provider_name.is_some()
            || self.model_name.is_some()
            || self.context_tokens.is_some()
            || self.max_output_tokens.is_some()
            || self.base_url.is_some()
            || self.api_key.is_some()
    }

    fn into_values(self, source: ProviderConfigSource) -> ResolvedProviderValues {
        ResolvedProviderValues {
            source: Some(source),
            provider_name: normalized_provider_value(self.provider_name),
            model_name: normalized_provider_value(self.model_name),
            context_tokens: normalized_provider_value(self.context_tokens),
            max_output_tokens: normalized_provider_value(self.max_output_tokens),
            base_url: normalized_provider_value(self.base_url),
            api_key: normalized_provider_value(self.api_key),
        }
    }
}

#[derive(Clone, Default)]
struct ResolvedProviderValues {
    source: Option<ProviderConfigSource>,
    provider_name: Option<String>,
    model_name: Option<String>,
    context_tokens: Option<String>,
    max_output_tokens: Option<String>,
    base_url: Option<String>,
    api_key: Option<String>,
}

fn provider_config_resolution(values: &ResolvedProviderValues) -> ProviderConfigResolution {
    let provider_name = values.source.map(|_| {
        values
            .provider_name
            .clone()
            .unwrap_or_else(|| DEFAULT_PROVIDER_NAME.to_string())
    });
    ProviderConfigResolution {
        source: values.source,
        config: ModelProviderConfig {
            provider_name,
            model_name: values.model_name.clone(),
            base_url_present: values.base_url.is_some(),
            api_key_present: values.api_key.is_some(),
        },
    }
}

fn resolve_provider_values<F>(mut get_env: F, project_dir: Option<&Path>) -> ResolvedProviderValues
where
    F: FnMut(&str) -> Option<String>,
{
    let process_layer = ProviderConfigLayer::from_process_env(&mut get_env);
    if process_layer.any_present() {
        return process_layer.into_values(ProviderConfigSource::ProcessEnvironment);
    }
    let Some(project_dir) = project_dir else {
        return ResolvedProviderValues::default();
    };
    let project_layer = project_env_layer(project_dir);
    if project_layer.any_present() {
        project_layer.into_values(ProviderConfigSource::ProjectEnvFile)
    } else {
        ResolvedProviderValues::default()
    }
}

fn normalized_provider_value(value: Option<String>) -> Option<String> {
    value.filter(|value| !value.trim().is_empty())
}

fn project_env_layer(project_dir: &Path) -> ProviderConfigLayer {
    let Some(path) = find_project_env_file(project_dir) else {
        return ProviderConfigLayer::default();
    };
    read_project_env_layer(&path)
}

fn find_project_env_file(project_dir: &Path) -> Option<PathBuf> {
    let mut dir = project_dir.to_path_buf();
    loop {
        let path = dir.join(PROJECT_ENV_FILE);
        if path.is_file() {
            return Some(path);
        }
        if !dir.pop() {
            return None;
        }
    }
}

fn read_project_env_layer(path: &Path) -> ProviderConfigLayer {
    let Ok(text) = std::fs::read_to_string(path) else {
        return ProviderConfigLayer::default();
    };
    let mut layer = ProviderConfigLayer::default();
    for (name, value) in text.lines().filter_map(parse_env_line) {
        let target = match name.as_str() {
            ENV_PROVIDER => &mut layer.provider_name,
            ENV_MODEL => &mut layer.model_name,
            ENV_CONTEXT_TOKENS => &mut layer.context_tokens,
            ENV_MAX_OUTPUT_TOKENS => &mut layer.max_output_tokens,
            ENV_BASE_URL => &mut layer.base_url,
            ENV_API_KEY => &mut layer.api_key,
            _ => continue,
        };
        if target.is_none() {
            *target = Some(value);
        }
    }
    layer
}

fn parse_env_line(line: &str) -> Option<(String, String)> {
    let mut text = line.trim();
    if text.is_empty() || text.starts_with('#') {
        return None;
    }
    if let Some(rest) = text.strip_prefix("export ") {
        text = rest.trim_start();
    }
    let (name, value) = text.split_once('=')?;
    let name = name.trim();
    if name.is_empty()
        || name.chars().next().is_some_and(|ch| ch.is_ascii_digit())
        || !name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
    {
        return None;
    }
    let mut value = value.trim();
    if value.len() >= 2 {
        let bytes = value.as_bytes();
        if (bytes[0] == b'\'' && bytes[value.len() - 1] == b'\'')
            || (bytes[0] == b'"' && bytes[value.len() - 1] == b'"')
        {
            value = &value[1..value.len() - 1];
        }
    }
    Some((name.to_string(), value.to_string()))
}

fn redacted_presence(present: bool) -> String {
    if present {
        "present(redacted)".to_string()
    } else {
        "missing".to_string()
    }
}

#[cfg(test)]
mod transport_tests {
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::thread;
    use std::time::Duration;

    use super::*;

    #[test]
    fn configured_deadline_is_reported_from_a_real_transport_timeout() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind hanging provider");
        let address = listener.local_addr().expect("provider address");
        let server = thread::spawn(move || {
            let mut streams = Vec::new();
            for _ in 0..MAX_PROVIDER_ATTEMPTS {
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
                max_context_tokens: DEFAULT_MAX_CONTEXT_TOKENS,
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
        assert_eq!(metadata.attempt_count, MAX_PROVIDER_ATTEMPTS);
        assert_eq!(metadata.retry_count, MAX_PROVIDER_ATTEMPTS - 1);
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
            max_context_tokens: DEFAULT_MAX_CONTEXT_TOKENS,
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
}
