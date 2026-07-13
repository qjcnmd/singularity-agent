#![forbid(unsafe_code)]

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use singularity_core::CancellationToken;
use thiserror::Error;
use uuid::Uuid;

const DEFAULT_MAX_TOOL_CALLS: u32 = 1;
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
const PROVIDER_TIMEOUT_SECONDS: u64 = 120;
const PROVIDER_CANCELLATION_POLL_MS: u64 = 25;
const MAX_PROVIDER_ATTEMPTS: u32 = 3;
const PROVIDER_RETRY_BASE_BACKOFF_MS: u64 = 50;
const PROVIDER_SNAPSHOT_ID_PREFIX: &str = "provider_snapshot_";
const PROVIDER_TOOL_CALL_LIMIT_EXCEEDED_CODE: &str = "provider_tool_call_limit_exceeded";
const HTTP_STATUS_UNAUTHORIZED: u16 = 401;
const HTTP_STATUS_FORBIDDEN: u16 = 403;
const HTTP_STATUS_REQUEST_TIMEOUT: u16 = 408;
const HTTP_STATUS_NOT_FOUND: u16 = 404;
const HTTP_STATUS_BAD_REQUEST: u16 = 400;
const HTTP_STATUS_UNPROCESSABLE_ENTITY: u16 = 422;
const HTTP_STATUS_RATE_LIMITED: u16 = 429;
const HTTP_STATUS_INTERNAL_SERVER_ERROR: u16 = 500;
const BUILTIN_TOOL_PREFIX: &str = "builtin.";
const TOOL_NAME_FALLBACK: &str = "tool";
const CAPABILITY_PROBE_REQUEST_ID: &str = "singularity_capability_probe";
const CAPABILITY_PROBE_TOOL_A: &str = "singularity_capability_probe_a";
const CAPABILITY_PROBE_TOOL_B: &str = "singularity_capability_probe_b";

const PERMISSION_DENIED_MARKERS: [&str; 4] = [
    "winerror 10013",
    "permission denied",
    "operation not permitted",
    "access is denied",
];
const MODEL_CONFIG_MARKERS: [&str; 6] = [
    "model",
    "not found",
    "does not exist",
    "invalid model",
    "unknown model",
    "unsupported model",
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ModelRole {
    System,
    Developer,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ModelMessage {
    pub role: ModelRole,
    pub content: String,
    pub name: Option<String>,
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ModelToolCall>,
}

impl ModelMessage {
    pub fn text(role: ModelRole, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
            name: None,
            tool_call_id: None,
            tool_calls: Vec::new(),
        }
    }

    pub fn assistant_tool_calls(tool_calls: Vec<ModelToolCall>) -> Self {
        Self {
            role: ModelRole::Assistant,
            content: String::new(),
            name: None,
            tool_call_id: None,
            tool_calls,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ToolChoiceMode {
    Auto,
    None,
    Required,
    SpecificTool,
    AllowedTools,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ToolChoicePolicy {
    pub mode: ToolChoiceMode,
    pub tool_name: Option<String>,
    pub allowed_tool_names: Vec<String>,
    pub max_tool_calls: u32,
    pub strict_tool_schema: bool,
}

impl Default for ToolChoicePolicy {
    fn default() -> Self {
        Self {
            mode: ToolChoiceMode::Auto,
            tool_name: None,
            allowed_tool_names: Vec::new(),
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ModelToolSchema {
    pub name: String,
    pub description: String,
    pub parameters_schema: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ModelToolCall {
    pub tool_call_id: String,
    pub tool_name: String,
    pub arguments: Value,
    pub raw_arguments: String,
    pub parse_status: ModelToolParseStatus,
    pub validation_errors: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProviderProtocolContract {
    pub supports_tools: bool,
    pub supports_strict_tool_schema: bool,
    pub max_tool_calls_per_turn: u32,
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
            supports_strict_tool_schema: false,
            max_tool_calls_per_turn: DEFAULT_MAX_TOOL_CALLS,
            supports_json_mode: false,
            supports_system_message: true,
            supports_developer_message: true,
            max_context_tokens: DEFAULT_MAX_CONTEXT_TOKENS,
            max_output_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProviderCapabilityProfile {
    Declared,
    StrictParallel,
    StrictSingle,
    NonStrictParallel,
    NonStrictSingle,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ModelPreferences {
    pub model_name: Option<String>,
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    pub max_output_tokens: Option<u32>,
    pub json_mode: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ModelProviderConfig {
    pub provider_name: Option<String>,
    pub model_name: Option<String>,
    pub base_url_present: bool,
    pub api_key_present: bool,
}

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderConfigResolution {
    pub source: Option<ProviderConfigSource>,
    pub config: ModelProviderConfig,
}

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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ModelBlockerKind {
    RequiredEnvMissing,
    AuthenticationProviderError,
    BaseUrlNetworkError,
    ModelNameConfigError,
    SandboxPermissionError,
}

impl ModelBlockerKind {
    pub fn code(&self) -> &'static str {
        match self {
            Self::RequiredEnvMissing => "required_env_missing",
            Self::AuthenticationProviderError => "authentication_provider_error",
            Self::BaseUrlNetworkError => "base_url_network_error",
            Self::ModelNameConfigError => "model_name_config_error",
            Self::SandboxPermissionError => "sandbox_permission_error",
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::RequiredEnvMissing => "required env missing",
            Self::AuthenticationProviderError => "authentication/provider error",
            Self::BaseUrlNetworkError => "base_url/network error",
            Self::ModelNameConfigError => "model name/config error",
            Self::SandboxPermissionError => "sandbox/permission error",
        }
    }
}

fn provider_initialization_blocker(category: &ModelErrorCategory) -> Option<ModelBlockerKind> {
    match category {
        ModelErrorCategory::Authentication => Some(ModelBlockerKind::AuthenticationProviderError),
        ModelErrorCategory::Network | ModelErrorCategory::ProviderUnavailable => {
            Some(ModelBlockerKind::BaseUrlNetworkError)
        }
        ModelErrorCategory::SandboxPermission => Some(ModelBlockerKind::SandboxPermissionError),
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

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ModelUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    pub cached_input_tokens: u64,
    pub reasoning_tokens: u64,
    pub cost_estimate: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ProviderCapabilityMetadata {
    pub profile: ProviderCapabilityProfile,
    pub cache_hit: bool,
    pub profile_attempts: u32,
    pub fallback_count: u32,
    pub probe_usage: ModelUsage,
    pub probe_attempt_metadata: ProviderAttemptMetadata,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ProviderProtocolNegotiation {
    pub contract: ProviderProtocolContract,
    pub metadata: ProviderCapabilityMetadata,
}

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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ModelErrorCategory {
    Cancelled,
    Authentication,
    Network,
    SandboxPermission,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProviderTransportCategory {
    Timeout,
    Connect,
    Request,
    BodyRead,
    Unknown,
}

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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ModelTurnStatus {
    Success,
    Failed,
    Invalid,
}

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

pub trait Provider {
    fn protocol_contract(&self) -> ProviderProtocolContract;

    fn negotiate_tool_capabilities(
        &self,
        _model_preferences: &ModelPreferences,
        _cancellation: &CancellationToken,
    ) -> Result<ProviderProtocolNegotiation, ProviderError> {
        Ok(ProviderProtocolNegotiation::declared(
            self.protocol_contract(),
        ))
    }

    fn complete(
        &self,
        request: &ModelTurnRequest,
        cancellation: &CancellationToken,
    ) -> Result<ModelTurnResponse, ProviderError>;
}

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

    pub fn protocol_contract(&self) -> ProviderProtocolContract {
        ProviderProtocolContract {
            supports_tools: true,
            supports_strict_tool_schema: false,
            max_tool_calls_per_turn: DEFAULT_MAX_TOOL_CALLS,
            supports_json_mode: true,
            supports_system_message: true,
            supports_developer_message: true,
            max_context_tokens: self.max_context_tokens,
            max_output_tokens: self.max_output_tokens,
        }
    }
}

#[derive(Clone)]
pub struct OpenAiProvider {
    config: OpenAiProviderConfig,
    client: reqwest::Client,
    request_timeout_seconds: u64,
    tool_capability_cache: Arc<Mutex<HashMap<String, ProviderProtocolNegotiation>>>,
    tool_capability_probe_in_progress: Arc<Mutex<HashSet<String>>>,
}

impl fmt::Debug for OpenAiProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenAiProvider")
            .field("config", &self.config)
            .field("client", &"[redacted]")
            .field("tool_capability_cache", &"[redacted]")
            .field("tool_capability_probe_in_progress", &"[redacted]")
            .finish()
    }
}

struct CapabilityProbeGate {
    in_progress: Arc<Mutex<HashSet<String>>>,
    model_name: String,
}

impl Drop for CapabilityProbeGate {
    fn drop(&mut self) {
        if let Ok(mut in_progress) = self.in_progress.lock() {
            in_progress.remove(&self.model_name);
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
            tool_capability_probe_in_progress: Arc::new(Mutex::new(HashSet::new())),
        })
    }

    pub fn from_env<F>(get_env: F) -> Result<Self, ProviderError>
    where
        F: FnMut(&str) -> Option<String>,
    {
        Self::new(OpenAiProviderConfig::from_env(get_env)?)
    }

    fn negotiate_openai_tool_capabilities(
        &self,
        model_name: &str,
        cancellation: &CancellationToken,
    ) -> Result<ProviderProtocolNegotiation, ProviderError> {
        loop {
            if cancellation.is_cancelled() {
                return Err(provider_cancelled_error().with_capability_metadata(
                    capability_probe_metadata(
                        ProviderCapabilityProfile::Declared,
                        0,
                        0,
                        &ModelUsage::default(),
                        &ProviderAttemptMetadata::zero(),
                    ),
                ));
            }
            let cached = self
                .tool_capability_cache
                .lock()
                .map_err(|_| provider_capability_cache_error())?
                .as_ref()
                .filter(|cached| cached.model_name == model_name)
                .map(|cached| cached.negotiation.clone());
            if let Some(cached) = cached {
                return Ok(cache_hit_negotiation(cached));
            }
            let mut in_progress = self
                .tool_capability_probe_in_progress
                .lock()
                .map_err(|_| provider_capability_cache_error())?;
            if in_progress.insert(model_name.to_string()) {
                break;
            }
            drop(in_progress);
            std::thread::sleep(Duration::from_millis(PROVIDER_CANCELLATION_POLL_MS));
        }
        let _probe_gate = CapabilityProbeGate {
            in_progress: Arc::clone(&self.tool_capability_probe_in_progress),
            model_name: model_name.to_string(),
        };

        let cached = self
            .tool_capability_cache
            .lock()
            .map_err(|_| provider_capability_cache_error())?
            .as_ref()
            .filter(|cached| cached.model_name == model_name)
            .map(|cached| cached.negotiation.clone());
        if let Some(cached) = cached {
            return Ok(cache_hit_negotiation(cached));
        }

        let mut probe_usage = ModelUsage::default();
        let mut probe_attempt_metadata = ProviderAttemptMetadata::zero();
        let profiles = capability_probe_profiles(&self.config, model_name);
        let profile_count = profiles.len();

        for (index, profile) in profiles.into_iter().enumerate() {
            if cancellation.is_cancelled() {
                return Err(provider_cancelled_error().with_capability_metadata(
                    capability_probe_metadata(
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
                        profile.profile,
                        index as u32,
                        index as u32,
                        &probe_usage,
                        &probe_attempt_metadata,
                    )));
            }
            let response = match self.complete_with_contract(
                &profile.request,
                cancellation,
                &profile.contract,
                model_name,
            ) {
                Ok(response) => {
                    if let Some(metadata) = &response.provider_attempt_metadata {
                        add_provider_attempt_metadata(&mut probe_attempt_metadata, metadata);
                    }
                    add_model_usage(&mut probe_usage, &response.usage);
                    response
                }
                Err(error) if is_capability_probe_profile_rejection(&error) => {
                    if let Some(metadata) = &error.provider_attempt_metadata {
                        add_provider_attempt_metadata(&mut probe_attempt_metadata, metadata);
                    }
                    if index + 1 == profile_count {
                        return Err(capability_probe_failure(
                            error,
                            profile.profile,
                            index as u32 + 1,
                            index as u32,
                            &probe_usage,
                            &probe_attempt_metadata,
                            "capability_profiles_exhausted",
                        ));
                    }
                    continue;
                }
                Err(error) if is_capability_probe_validation_mismatch(&error) => {
                    if let Some(metadata) = &error.provider_attempt_metadata {
                        add_provider_attempt_metadata(&mut probe_attempt_metadata, metadata);
                    }
                    if index + 1 == profile_count {
                        return Err(capability_probe_failure(
                            error,
                            profile.profile,
                            index as u32 + 1,
                            index as u32,
                            &probe_usage,
                            &probe_attempt_metadata,
                            "capability_profiles_exhausted",
                        ));
                    }
                    continue;
                }
                Err(error) => {
                    if let Some(metadata) = &error.provider_attempt_metadata {
                        add_provider_attempt_metadata(&mut probe_attempt_metadata, metadata);
                    }
                    return Err(error.with_capability_metadata(capability_probe_metadata(
                        profile.profile,
                        index as u32 + 1,
                        index as u32,
                        &probe_usage,
                        &probe_attempt_metadata,
                    )));
                }
            };
            let single_call_fallback =
                profile.expected_tool_calls == 2 && capability_probe_single_call_matches(&response);
            if single_call_fallback
                || capability_probe_response_matches(&response, profile.expected_tool_calls)
            {
                let mut contract = profile.contract;
                let negotiated_profile = if single_call_fallback {
                    contract.max_tool_calls_per_turn = 1;
                    match profile.profile {
                        ProviderCapabilityProfile::StrictParallel => {
                            ProviderCapabilityProfile::StrictSingle
                        }
                        ProviderCapabilityProfile::NonStrictParallel => {
                            ProviderCapabilityProfile::NonStrictSingle
                        }
                        profile => profile,
                    }
                } else {
                    profile.profile
                };
                let negotiation = ProviderProtocolNegotiation {
                    contract,
                    metadata: capability_probe_metadata(
                        negotiated_profile,
                        index as u32 + 1,
                        index as u32,
                        &probe_usage,
                        &probe_attempt_metadata,
                    ),
                };
                self.tool_capability_cache
                    .lock()
                    .map_err(|_| provider_capability_cache_error())?
                    .insert(model_name.to_string(), negotiation.clone());
                return Ok(negotiation);
            }
            if index + 1 == profile_count {
                return Err(
                    capability_probe_response_error(&response).with_capability_metadata(
                        capability_probe_metadata(
                            profile.profile,
                            index as u32 + 1,
                            index as u32,
                            &probe_usage,
                            &probe_attempt_metadata,
                        ),
                    ),
                );
            }
        }

        Err(capability_probe_unsupported_error(ModelError::new(
            ModelErrorKind::UnsupportedCapability,
            "provider does not support native structured tool calls",
        )))
    }

    fn complete_with_contract(
        &self,
        request: &ModelTurnRequest,
        cancellation: &CancellationToken,
        capabilities: &ProviderProtocolContract,
        model_name: &str,
    ) -> Result<ModelTurnResponse, ProviderError> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(provider_runtime_error)?;
        let started_at = Instant::now();
        let mut metadata = ProviderAttemptMetadata::zero();
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
                            .post(self.config.endpoint())
                            .bearer_auth(&self.config.api_key)
                            .json(&openai_request_payload(request, model_name))
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
                match block_on_provider_future(
                    &runtime,
                    cancellation,
                    "provider_response_body_read_failed",
                    ProviderErrorStage::ResponseBodyRead,
                    self.request_timeout_seconds,
                    || response.bytes(),
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
            return parse_openai_response(request, &self.config, payload, capabilities, model_name)
                .map(|mut response| {
                    response.provider_attempt_metadata =
                        Some(provider_attempt_metadata(&metadata, started_at));
                    response
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
        let (capabilities, capability_metadata) = if request.tools.is_empty() {
            (self.protocol_contract(), None)
        } else {
            let effective_model_name = request
                .model_preferences
                .model_name
                .as_deref()
                .unwrap_or(&self.config.model_name);
            let negotiation =
                self.negotiate_openai_tool_capabilities(effective_model_name, cancellation)?;
            (negotiation.contract, Some(negotiation.metadata))
        };
        let request_validation =
            validate_model_request_with_capabilities(request, Some(&capabilities));
        if !request_validation.valid {
            let kind = if validation_is_unsupported_capability(&request_validation) {
                ModelErrorKind::UnsupportedCapability
            } else {
                ModelErrorKind::InvalidRequest
            };
            let mut error = ModelError::new(
                kind,
                format!(
                    "model request validation failed: {}",
                    request_validation.errors.join(", ")
                ),
            )
            .with_provider(self.config.provider_name.clone())
            .with_model(self.config.model_name.clone())
            .with_provider_diagnostic("provider_request_invalid", ProviderErrorStage::RequestSend);
            error.validation_errors = request_validation.errors;
            let provider_error = ProviderError::from_model_error(error)
                .with_provider_attempt_metadata(ProviderAttemptMetadata::zero());
            return Err(
                capability_metadata.map_or(provider_error.clone(), |metadata| {
                    provider_error.with_capability_metadata(metadata)
                }),
            );
        }
        let effective_model_name = request
            .model_preferences
            .model_name
            .as_deref()
            .unwrap_or(&self.config.model_name);
        self.complete_with_contract(request, cancellation, &capabilities, effective_model_name)
            .map_err(|error| {
                capability_metadata.map_or(error.clone(), |metadata| {
                    error.with_capability_metadata(metadata)
                })
            })
    }
}

#[derive(Debug, Clone, PartialEq, Error)]
#[error("{message}")]
pub struct ProviderError {
    pub message: String,
    pub error: Box<ModelError>,
    pub provider_attempt_metadata: Option<ProviderAttemptMetadata>,
    pub capability_metadata: Option<ProviderCapabilityMetadata>,
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
        self.capability_metadata = Some(metadata);
        self
    }
}

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

pub fn resolve_provider_config<F>(get_env: F) -> ProviderConfigResolution
where
    F: FnMut(&str) -> Option<String>,
{
    let project_dir = std::env::current_dir().ok();
    let values = resolve_provider_values(get_env, project_dir.as_deref());
    provider_config_resolution(&values)
}

fn openai_request_payload(request: &ModelTurnRequest, model_name: &str) -> Value {
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
    payload
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
    if let Some(name) = &message.name {
        payload["name"] = json!(openai_wire_tool_name(name));
    }
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
            "name": openai_wire_tool_name(&tool_call.tool_name),
            "arguments": tool_call.raw_arguments,
        }
    })
}

fn openai_tool_payload(tool: &ModelToolSchema, strict_tool_schema: bool) -> Value {
    let mut payload = json!({
        "type": "function",
        "function": {
            "name": openai_wire_tool_name(&tool.name),
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
        ToolChoiceMode::SpecificTool => match &request.tool_choice.tool_name {
            Some(name) => {
                json!({"type": "function", "function": {"name": openai_wire_tool_name(name)}})
            }
            None => json!("auto"),
        },
        ToolChoiceMode::Auto | ToolChoiceMode::AllowedTools => json!("auto"),
    }
}

struct CapabilityProbeProfile {
    profile: ProviderCapabilityProfile,
    contract: ProviderProtocolContract,
    request: ModelTurnRequest,
    expected_tool_calls: usize,
}

fn capability_probe_profiles(
    config: &OpenAiProviderConfig,
    model_name: &str,
) -> Vec<CapabilityProbeProfile> {
    let base = config.protocol_contract();
    let strict_schema = json!({
        "type": "object",
        "properties": {},
        "required": [],
        "additionalProperties": false
    });
    let tool_a = ModelToolSchema {
        name: CAPABILITY_PROBE_TOOL_A.to_string(),
        description: "Fixed capability probe tool A; no external side effect.".to_string(),
        parameters_schema: strict_schema.clone(),
    };
    let tool_b = ModelToolSchema {
        name: CAPABILITY_PROBE_TOOL_B.to_string(),
        description: "Fixed capability probe tool B; no external side effect.".to_string(),
        parameters_schema: strict_schema,
    };
    let make_request = |tools: Vec<ModelToolSchema>,
                        mode: ToolChoiceMode,
                        max_tool_calls: u32,
                        strict: bool,
                        tool_name: Option<&str>,
                        instruction: &str| {
        let mut request = ModelTurnRequest::new(
            CAPABILITY_PROBE_REQUEST_ID,
            vec![ModelMessage::text(ModelRole::User, instruction)],
        );
        request.model_preferences.model_name = Some(model_name.to_string());
        request.tools = tools;
        request.tool_choice = ToolChoicePolicy {
            mode,
            tool_name: tool_name.map(str::to_string),
            allowed_tool_names: Vec::new(),
            max_tool_calls,
            strict_tool_schema: strict,
        };
        request
    };
    let make_contract = |strict: bool, max_tool_calls_per_turn: u32| ProviderProtocolContract {
        supports_strict_tool_schema: strict,
        max_tool_calls_per_turn,
        ..base.clone()
    };
    vec![
        CapabilityProbeProfile {
            profile: ProviderCapabilityProfile::StrictParallel,
            contract: make_contract(true, 2),
            request: make_request(
                vec![tool_a.clone(), tool_b.clone()],
                ToolChoiceMode::Required,
                2,
                true,
                None,
                "Call singularity_capability_probe_a and singularity_capability_probe_b exactly once each with arguments {}.",
            ),
            expected_tool_calls: 2,
        },
        CapabilityProbeProfile {
            profile: ProviderCapabilityProfile::NonStrictParallel,
            contract: make_contract(false, 2),
            request: make_request(
                vec![tool_a.clone(), tool_b],
                ToolChoiceMode::Required,
                2,
                false,
                None,
                "Call singularity_capability_probe_a and singularity_capability_probe_b exactly once each with arguments {}.",
            ),
            expected_tool_calls: 2,
        },
        CapabilityProbeProfile {
            profile: ProviderCapabilityProfile::NonStrictSingle,
            contract: make_contract(false, 1),
            request: make_request(
                vec![tool_a],
                ToolChoiceMode::SpecificTool,
                1,
                false,
                Some(CAPABILITY_PROBE_TOOL_A),
                "Call singularity_capability_probe_a exactly once with arguments {}.",
            ),
            expected_tool_calls: 1,
        },
    ]
}

fn capability_probe_response_matches(
    response: &ModelTurnResponse,
    expected_tool_calls: usize,
) -> bool {
    if response.status != ModelTurnStatus::Success
        || response.tool_calls.len() != expected_tool_calls
    {
        return false;
    }
    if expected_tool_calls == 1 {
        return response.tool_calls[0].tool_name == CAPABILITY_PROBE_TOOL_A
            && response.tool_calls[0].parse_status == ModelToolParseStatus::Valid
            && response.tool_calls[0].arguments == json!({});
    }
    let mut names = response
        .tool_calls
        .iter()
        .filter(|call| call.parse_status == ModelToolParseStatus::Valid)
        .filter(|call| call.arguments == json!({}))
        .map(|call| call.tool_name.as_str())
        .collect::<Vec<_>>();
    names.sort_unstable();
    names == [CAPABILITY_PROBE_TOOL_A, CAPABILITY_PROBE_TOOL_B]
}

fn capability_probe_single_call_matches(response: &ModelTurnResponse) -> bool {
    response.status == ModelTurnStatus::Success
        && response.tool_calls.len() == 1
        && response.tool_calls[0].parse_status == ModelToolParseStatus::Valid
        && response.tool_calls[0].arguments == json!({})
        && matches!(
            response.tool_calls[0].tool_name.as_str(),
            CAPABILITY_PROBE_TOOL_A | CAPABILITY_PROBE_TOOL_B
        )
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

fn openai_wire_tool_name(name: &str) -> String {
    let public_name = name.strip_prefix(BUILTIN_TOOL_PREFIX).unwrap_or(name);
    let sanitized = public_name
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-') {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    let trimmed = sanitized.trim_matches('_');
    if trimmed.is_empty() {
        TOOL_NAME_FALLBACK.to_string()
    } else {
        trimmed.to_string()
    }
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
    let tool_calls = parse_openai_tool_calls(request, message);
    let assistant_message = Some(ModelMessage {
        tool_calls: tool_calls.clone(),
        ..ModelMessage::text(ModelRole::Assistant, content)
    });
    let finish_reason = choice
        .get("finish_reason")
        .and_then(Value::as_str)
        .map(str::to_string);
    let mut response = ModelTurnResponse {
        request_id: request.request_id.clone(),
        response_id: response_id.clone(),
        status: ModelTurnStatus::Success,
        assistant_message,
        tool_calls,
        usage: parse_openai_usage(payload.get("usage")),
        finish_reason,
        validation: None,
        error: None,
        provider_name: Some(config.provider_name.clone()),
        model_name: Some(model_name.to_string()),
        provider_attempt_metadata: None,
    };
    let allowed_tool_names = request
        .tools
        .iter()
        .map(|tool| tool.name.clone())
        .collect::<Vec<_>>();
    let validation =
        validate_model_turn_response(request, &response, &allowed_tool_names, Some(capabilities));
    if !validation.valid {
        response.status = ModelTurnStatus::Invalid;
        let tool_call_limit_exceeded =
            response.tool_calls.len() > request.tool_choice.max_tool_calls as usize;
        let (kind, message, diagnostic_code) = if tool_call_limit_exceeded {
            (
                ModelErrorKind::UnsupportedCapability,
                format!(
                    "provider returned more tool calls than the negotiated limit of {}",
                    request.tool_choice.max_tool_calls
                ),
                PROVIDER_TOOL_CALL_LIMIT_EXCEEDED_CODE,
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

fn parse_openai_tool_calls(request: &ModelTurnRequest, message: &Value) -> Vec<ModelToolCall> {
    let tool_name_map = openai_wire_tool_name_map(request);
    message
        .get("tool_calls")
        .and_then(Value::as_array)
        .map(|calls| {
            calls
                .iter()
                .enumerate()
                .map(|(index, call)| parse_openai_tool_call(index, call, &tool_name_map))
                .collect()
        })
        .unwrap_or_default()
}

fn parse_openai_tool_call(
    _index: usize,
    call: &Value,
    tool_name_map: &[(String, String)],
) -> ModelToolCall {
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
        tool_name: internal_tool_name(wire_tool_name, tool_name_map),
        arguments,
        raw_arguments,
        parse_status,
        validation_errors,
    }
}

fn openai_wire_tool_name_map(request: &ModelTurnRequest) -> Vec<(String, String)> {
    request
        .tools
        .iter()
        .map(|tool| (openai_wire_tool_name(&tool.name), tool.name.clone()))
        .collect()
}

fn internal_tool_name(wire_name: &str, tool_name_map: &[(String, String)]) -> String {
    tool_name_map
        .iter()
        .find(|(wire, _internal)| wire == wire_name)
        .map(|(_wire, internal)| internal.clone())
        .unwrap_or_else(|| wire_name.to_string())
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
    let message = if status == HTTP_STATUS_NOT_FOUND {
        format!("Provider returned HTTP {status}; model not found.")
    } else {
        format!("Provider returned HTTP {status}.")
    };
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
    profile: ProviderCapabilityProfile,
    profile_attempts: u32,
    fallback_count: u32,
    probe_usage: &ModelUsage,
    probe_attempt_metadata: &ProviderAttemptMetadata,
) -> ProviderCapabilityMetadata {
    ProviderCapabilityMetadata {
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
    profile: ProviderCapabilityProfile,
    profile_attempts: u32,
    fallback_count: u32,
    probe_usage: &ModelUsage,
    probe_attempt_metadata: &ProviderAttemptMetadata,
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
    let provider_error = provider_attempt_metadata.map_or(provider_error.clone(), |metadata| {
        provider_error.with_provider_attempt_metadata(metadata)
    });
    provider_error.with_capability_metadata(capability_probe_metadata(
        profile,
        profile_attempts,
        fallback_count,
        probe_usage,
        probe_attempt_metadata,
    ))
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
    if error.validation_errors.is_empty() {
        error
            .validation_errors
            .push("capability_probe_native_tool_calls_missing".to_string());
    }
    capability_probe_unsupported_error(error)
}

fn is_capability_probe_profile_rejection(error: &ProviderError) -> bool {
    error.error.stage == Some(ProviderErrorStage::ResponseStatus)
        && matches!(
            error.error.http_status,
            Some(HTTP_STATUS_BAD_REQUEST | HTTP_STATUS_UNPROCESSABLE_ENTITY)
        )
}

fn is_capability_probe_validation_mismatch(error: &ProviderError) -> bool {
    error.error.stage == Some(ProviderErrorStage::ResponseValidation)
}

fn validation_is_unsupported_capability(validation: &ModelValidationResult) -> bool {
    !validation.errors.is_empty()
        && validation.errors.iter().all(|error| {
            matches!(
                error.as_str(),
                "provider_does_not_support_tools"
                    | "provider_does_not_support_strict_tool_schema"
                    | "requested_tool_calls_exceed_provider_limit"
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
            && request.tool_choice.max_tool_calls > capabilities.max_tool_calls_per_turn
        {
            errors.push("requested_tool_calls_exceed_provider_limit".to_string());
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

pub fn validate_model_turn_response(
    request: &ModelTurnRequest,
    response: &ModelTurnResponse,
    allowed_tool_names: &[String],
    capabilities: Option<&ProviderProtocolContract>,
) -> ModelValidationResult {
    let mut result = validate_model_response(
        response.assistant_message.as_ref(),
        &response.tool_calls,
        &request.tool_choice,
        allowed_tool_names,
        capabilities,
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

pub fn validate_model_response(
    assistant_message: Option<&ModelMessage>,
    tool_calls: &[ModelToolCall],
    tool_choice: &ToolChoicePolicy,
    allowed_tool_names: &[String],
    capabilities: Option<&ProviderProtocolContract>,
) -> ModelValidationResult {
    let mut errors = Vec::new();
    let mut warnings = Vec::new();

    match assistant_message {
        Some(message) if message.role != ModelRole::Assistant => {
            errors.push("non_assistant_response".to_string());
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
        ToolChoiceMode::Required if tool_calls.is_empty() => {
            errors.push("tool_choice_required".to_string());
        }
        _ => {}
    }
    if tool_calls.len() > tool_choice.max_tool_calls as usize {
        errors.push("max_tool_calls_exceeded".to_string());
    }
    if let Some(capabilities) = capabilities {
        if !tool_calls.is_empty() && !capabilities.supports_tools {
            errors.push("provider_does_not_support_tools".to_string());
        }
        if tool_calls.len() > capabilities.max_tool_calls_per_turn as usize {
            errors.push("provider_tool_call_limit_exceeded".to_string());
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
        if !allowed_tool_names
            .iter()
            .any(|name| name == &call.tool_name)
        {
            errors.push("unknown_tool".to_string());
        }
        validate_tool_choice_name(call, tool_choice, &mut errors);
        match call.parse_status {
            ModelToolParseStatus::InvalidJson => errors.push("invalid_json".to_string()),
            ModelToolParseStatus::SchemaMismatch => errors.push("schema_mismatch".to_string()),
            ModelToolParseStatus::UnknownTool => errors.push("unknown_tool".to_string()),
            ModelToolParseStatus::Valid => {}
        }
        warnings.extend(call.validation_errors.iter().cloned());
    }

    validation_result(errors, warnings)
}

fn validate_tool_choice_name(
    call: &ModelToolCall,
    tool_choice: &ToolChoicePolicy,
    errors: &mut Vec<String>,
) {
    match tool_choice.mode {
        ToolChoiceMode::SpecificTool
            if tool_choice.tool_name.as_deref() != Some(call.tool_name.as_str()) =>
        {
            errors.push("specific_tool_required".to_string());
        }
        ToolChoiceMode::AllowedTools
            if !tool_choice
                .allowed_tool_names
                .iter()
                .any(|name| name == &call.tool_name) =>
        {
            errors.push("tool_not_allowed_by_choice".to_string());
        }
        _ => {}
    }
}

fn model_error_category(error: &ModelError) -> ModelErrorCategory {
    match error.kind {
        ModelErrorKind::Cancelled => ModelErrorCategory::Cancelled,
        ModelErrorKind::AuthError => ModelErrorCategory::Authentication,
        ModelErrorKind::NetworkError | ModelErrorKind::Timeout => {
            if contains_any_marker(&error.message, &PERMISSION_DENIED_MARKERS) {
                ModelErrorCategory::SandboxPermission
            } else {
                ModelErrorCategory::Network
            }
        }
        ModelErrorKind::InvalidRequest | ModelErrorKind::UnsupportedCapability
            if looks_like_model_config_error(&error.message) =>
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

fn looks_like_model_config_error(message: &str) -> bool {
    let lowered = message.to_ascii_lowercase();
    lowered.contains(MODEL_CONFIG_MARKERS[0])
        && MODEL_CONFIG_MARKERS[1..]
            .iter()
            .any(|marker| lowered.contains(marker))
}

fn contains_any_marker(message: &str, markers: &[&str]) -> bool {
    let lowered = message.to_ascii_lowercase();
    markers.iter().any(|marker| lowered.contains(marker))
}

#[cfg(test)]
mod transport_tests {
    use std::io::{BufRead, BufReader};
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
}
