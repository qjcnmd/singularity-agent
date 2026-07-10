#![forbid(unsafe_code)]

use std::collections::HashSet;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;
use uuid::Uuid;

const DEFAULT_MAX_TOOL_CALLS: u32 = 8;
const DEFAULT_MAX_RETRIES: u32 = 2;
pub const DEFAULT_MAX_CONTEXT_TOKENS: u32 = 128_000;
pub const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 4_096;
const STREAM_EVENT_PREFIX: &str = "stream_event";
const ENV_PROVIDER: &str = "SINGULARITY_MODEL_PROVIDER";
const ENV_MODEL: &str = "SINGULARITY_MODEL";
const ENV_BASE_URL: &str = "SINGULARITY_BASE_URL";
const ENV_API_KEY: &str = "SINGULARITY_API_KEY";
const PROJECT_ENV_FILE: &str = ".env";
const DEFAULT_PROVIDER_NAME: &str = "openai_compatible";
const CHAT_COMPLETIONS_PATH: &str = "/chat/completions";
const V1_CHAT_COMPLETIONS_PATH: &str = "/v1/chat/completions";
const PROVIDER_TIMEOUT_SECONDS: u64 = 120;
const PROVIDER_SNAPSHOT_ID_PREFIX: &str = "provider_snapshot_";
const HTTP_STATUS_UNAUTHORIZED: u16 = 401;
const HTTP_STATUS_FORBIDDEN: u16 = 403;
const HTTP_STATUS_NOT_FOUND: u16 = 404;
const HTTP_STATUS_RATE_LIMITED: u16 = 429;
const HTTP_STATUS_INTERNAL_SERVER_ERROR: u16 = 500;
const BUILTIN_TOOL_PREFIX: &str = "builtin.";
const TOOL_NAME_FALLBACK: &str = "tool";

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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ModelPurpose {
    PlanNextAction,
    FinalAnswer,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ModelMessage {
    pub role: ModelRole,
    pub content: Vec<ContentBlock>,
    pub name: Option<String>,
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ModelToolCall>,
    pub metadata: Value,
}

impl ModelMessage {
    pub fn text(role: ModelRole, content: impl Into<String>) -> Self {
        Self {
            role,
            content: vec![ContentBlock::text(content)],
            name: None,
            tool_call_id: None,
            tool_calls: Vec::new(),
            metadata: json!({}),
        }
    }

    pub fn assistant_tool_calls(tool_calls: Vec<ModelToolCall>) -> Self {
        Self {
            role: ModelRole::Assistant,
            content: Vec::new(),
            name: None,
            tool_call_id: None,
            tool_calls,
            metadata: json!({}),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ContentBlockType {
    Text,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ContentBlock {
    #[serde(rename = "type")]
    pub block_type: ContentBlockType,
    pub text: Option<String>,
    pub artifact_ref: Option<String>,
    pub metadata: Value,
}

impl ContentBlock {
    pub fn text(content: impl Into<String>) -> Self {
        Self {
            block_type: ContentBlockType::Text,
            text: Some(content.into()),
            artifact_ref: None,
            metadata: json!({}),
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
}

impl Default for ToolChoicePolicy {
    fn default() -> Self {
        Self {
            mode: ToolChoiceMode::Auto,
            tool_name: None,
            allowed_tool_names: Vec::new(),
            max_tool_calls: DEFAULT_MAX_TOOL_CALLS,
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
    pub capability_tags: Vec<String>,
    pub risk_tags: Vec<String>,
    pub metadata: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ModelToolCall {
    pub tool_call_id: String,
    pub tool_name: String,
    pub arguments: Value,
    pub raw_arguments: String,
    pub parse_status: ModelToolParseStatus,
    pub validation_errors: Vec<String>,
    pub provider_metadata: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ModelCapabilities {
    pub supports_tools: bool,
    pub supports_parallel_tool_calls: bool,
    pub supports_streaming: bool,
    pub supports_json_mode: bool,
    pub supports_structured_outputs: bool,
    pub supports_system_message: bool,
    pub supports_developer_message: bool,
    pub max_context_tokens: u32,
    pub max_output_tokens: u32,
    pub input_modalities: Vec<String>,
    pub output_modalities: Vec<String>,
}

impl Default for ModelCapabilities {
    fn default() -> Self {
        Self {
            supports_tools: true,
            supports_parallel_tool_calls: false,
            supports_streaming: false,
            supports_json_mode: false,
            supports_structured_outputs: false,
            supports_system_message: true,
            supports_developer_message: false,
            max_context_tokens: DEFAULT_MAX_CONTEXT_TOKENS,
            max_output_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
            input_modalities: vec!["text".to_string()],
            output_modalities: vec!["text".to_string()],
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ModelPreferences {
    pub provider_name: Option<String>,
    pub model_name: Option<String>,
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    pub max_output_tokens: Option<u32>,
    pub json_mode: bool,
    pub structured_output_schema: Option<Value>,
    pub stream: bool,
    pub fallback_models: Vec<String>,
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
    readiness: ModelProviderStatus,
    provider: Result<OpenAiProvider, ProviderError>,
}

impl fmt::Debug for ProviderConfigSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderConfigSnapshot")
            .field("snapshot_id", &self.snapshot_id)
            .field("source", &self.source)
            .field("redacted_config", &self.redacted_config)
            .field("readiness", &self.readiness)
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
        let mut readiness = ModelProviderStatus::from_config(&redacted_config);
        if readiness.ready
            && let Err(error) = &provider
        {
            readiness.ready = false;
            readiness.blocker = Some((&error.error.category()).into());
        }
        Self {
            snapshot_id: format!("{PROVIDER_SNAPSHOT_ID_PREFIX}{}", Uuid::new_v4().simple()),
            source,
            redacted_config,
            readiness,
            provider,
        }
    }

    pub fn source(&self) -> Option<ProviderConfigSource> {
        self.source
    }

    pub fn redacted_config(&self) -> &ModelProviderConfig {
        &self.redacted_config
    }

    pub fn readiness(&self) -> &ModelProviderStatus {
        &self.readiness
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ModelProviderStatus {
    pub ready: bool,
    pub provider_name: Option<String>,
    pub model_name: Option<String>,
    pub api_key_status: String,
    pub base_url_status: String,
    pub blocker: Option<ModelBlockerKind>,
}

impl ModelProviderStatus {
    pub fn from_config(config: &ModelProviderConfig) -> Self {
        let validation = validate_provider_config(config);
        Self {
            ready: validation.valid,
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ModelBudget {
    pub max_input_tokens: Option<u32>,
    pub max_output_tokens: Option<u32>,
    pub max_total_tokens: Option<u32>,
    pub max_retries: u32,
    pub max_latency_ms: Option<u64>,
    pub max_cost_estimate: Option<f64>,
}

impl Default for ModelBudget {
    fn default() -> Self {
        Self {
            max_input_tokens: None,
            max_output_tokens: None,
            max_total_tokens: None,
            max_retries: DEFAULT_MAX_RETRIES,
            max_latency_ms: None,
            max_cost_estimate: None,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ModelValidationResult {
    pub valid: bool,
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
    pub repaired: bool,
    pub repair_message: Option<String>,
}

impl ModelValidationResult {
    pub fn valid() -> Self {
        Self {
            valid: true,
            errors: Vec::new(),
            warnings: Vec::new(),
            repaired: false,
            repair_message: None,
        }
    }

    pub fn invalid(errors: Vec<String>) -> Self {
        Self {
            valid: false,
            errors,
            warnings: Vec::new(),
            repaired: false,
            repair_message: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ModelRetryDecision {
    pub retry: bool,
    pub next_attempt: Option<u32>,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ModelErrorKind {
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ModelError {
    pub kind: ModelErrorKind,
    pub message: String,
    pub retryable: bool,
    pub provider_name: Option<String>,
    pub model_name: Option<String>,
    pub raw_error_ref: Option<String>,
    pub metadata: Value,
}

impl ModelError {
    pub fn new(kind: ModelErrorKind, message: impl Into<String>) -> Self {
        let retryable = default_retryable(&kind);
        Self {
            kind,
            message: message.into(),
            retryable,
            provider_name: None,
            model_name: None,
            raw_error_ref: None,
            metadata: json!({}),
        }
    }

    pub fn retryable(mut self, retryable: bool) -> Self {
        self.retryable = retryable;
        self
    }

    pub fn with_provider(mut self, provider_name: impl Into<String>) -> Self {
        self.provider_name = Some(provider_name.into());
        self
    }

    pub fn with_model(mut self, model_name: impl Into<String>) -> Self {
        self.model_name = Some(model_name.into());
        self
    }

    pub fn with_raw_ref(mut self, raw_error_ref: impl Into<String>) -> Self {
        self.raw_error_ref = Some(raw_error_ref.into());
        self
    }

    pub fn category(&self) -> ModelErrorCategory {
        classify_model_error(self)
    }
}

pub fn classify_model_error(error: &ModelError) -> ModelErrorCategory {
    model_error_category(error)
}

impl From<&ModelErrorCategory> for ModelBlockerKind {
    fn from(category: &ModelErrorCategory) -> Self {
        match category {
            ModelErrorCategory::Authentication => Self::AuthenticationProviderError,
            ModelErrorCategory::Network | ModelErrorCategory::ProviderUnavailable => {
                Self::BaseUrlNetworkError
            }
            ModelErrorCategory::SandboxPermission => Self::SandboxPermissionError,
            ModelErrorCategory::ModelConfiguration => Self::ModelNameConfigError,
            _ => Self::BaseUrlNetworkError,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ModelTurnRequest {
    pub request_id: String,
    pub run_id: String,
    pub session_id: String,
    pub task_id: String,
    pub phase_id: String,
    pub action_id: String,
    pub purpose: ModelPurpose,
    pub messages: Vec<ModelMessage>,
    pub tools: Vec<ModelToolSchema>,
    pub tool_choice: ToolChoicePolicy,
    pub model_preferences: ModelPreferences,
    pub budget: ModelBudget,
    pub context_metadata: Value,
    pub policy_metadata: Value,
    pub trace_metadata: Value,
}

impl ModelTurnRequest {
    pub fn new(
        request_id: impl Into<String>,
        run_id: impl Into<String>,
        session_id: impl Into<String>,
        task_id: impl Into<String>,
        messages: Vec<ModelMessage>,
    ) -> Self {
        let request_id = request_id.into();
        Self {
            action_id: request_id.clone(),
            request_id,
            run_id: run_id.into(),
            session_id: session_id.into(),
            task_id: task_id.into(),
            phase_id: "model".to_string(),
            purpose: ModelPurpose::PlanNextAction,
            messages,
            tools: Vec::new(),
            tool_choice: ToolChoicePolicy::default(),
            model_preferences: ModelPreferences::default(),
            budget: ModelBudget::default(),
            context_metadata: json!({}),
            policy_metadata: json!({}),
            trace_metadata: json!({}),
        }
    }

    pub fn with_phase_action(
        mut self,
        phase_id: impl Into<String>,
        action_id: impl Into<String>,
    ) -> Self {
        self.phase_id = phase_id.into();
        self.action_id = action_id.into();
        self
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
    pub latency_ms: Option<u64>,
    pub trace_event_ids: Vec<String>,
    pub raw_response_ref: Option<String>,
    pub metadata: Value,
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
            latency_ms: None,
            trace_event_ids: Vec::new(),
            raw_response_ref: None,
            metadata: json!({}),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProviderStreamEventType {
    TextDelta,
    ToolCallDelta,
    ToolCallCompleted,
    UsageDelta,
    ResponseCompleted,
    Error,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ProviderStreamEvent {
    #[serde(rename = "type")]
    pub event_type: ProviderStreamEventType,
    pub text_delta: Option<String>,
    pub tool_call_id: Option<String>,
    pub tool_name: Option<String>,
    pub arguments_delta: Option<String>,
    pub usage_delta: Option<ModelUsage>,
    pub error: Option<String>,
    pub metadata: Value,
}

pub trait Provider {
    fn complete(&self, request: &ModelTurnRequest) -> Result<ModelTurnResponse, ProviderError>;
}

#[derive(Clone, PartialEq, Eq)]
pub struct OpenAiProviderConfig {
    pub provider_name: String,
    pub model_name: String,
    pub base_url: String,
    pub api_key: String,
    pub source: ProviderConfigSource,
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
        })
    }

    pub fn redacted_status(&self) -> ModelProviderStatus {
        ModelProviderStatus::from_config(&ModelProviderConfig {
            provider_name: Some(self.provider_name.clone()),
            model_name: Some(self.model_name.clone()),
            base_url_present: true,
            api_key_present: true,
        })
    }

    pub fn endpoint(&self) -> String {
        chat_completions_endpoint(&self.base_url)
    }
}

#[derive(Clone)]
pub struct OpenAiProvider {
    config: OpenAiProviderConfig,
    client: reqwest::blocking::Client,
}

impl fmt::Debug for OpenAiProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenAiProvider")
            .field("config", &self.config)
            .field("client", &"[redacted]")
            .finish()
    }
}

impl OpenAiProvider {
    pub fn new(config: OpenAiProviderConfig) -> Result<Self, ProviderError> {
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(PROVIDER_TIMEOUT_SECONDS))
            .build()
            .map_err(provider_transport_error)?;
        Ok(Self { config, client })
    }

    pub fn from_env<F>(get_env: F) -> Result<Self, ProviderError>
    where
        F: FnMut(&str) -> Option<String>,
    {
        Self::new(OpenAiProviderConfig::from_env(get_env)?)
    }
}

impl Provider for OpenAiProvider {
    fn complete(&self, request: &ModelTurnRequest) -> Result<ModelTurnResponse, ProviderError> {
        let request_validation = validate_model_request(request);
        if !request_validation.valid {
            return Err(ProviderError::from_model_error(
                ModelError::new(
                    ModelErrorKind::InvalidRequest,
                    "model request validation failed",
                )
                .with_provider(self.config.provider_name.clone())
                .with_model(self.config.model_name.clone()),
            ));
        }
        let response = self
            .client
            .post(self.config.endpoint())
            .bearer_auth(&self.config.api_key)
            .json(&openai_request_payload(request, &self.config.model_name))
            .send()
            .map_err(provider_transport_error)?;
        let status = response.status();
        if !status.is_success() {
            return Err(ProviderError::from_model_error(
                model_error_from_http_status(
                    status.as_u16(),
                    &self.config.provider_name,
                    &self.config.model_name,
                )
                .with_raw_ref(format!("provider_http_status_{}", status.as_u16())),
            ));
        }
        let payload = response.json::<Value>().map_err(|error| {
            ProviderError::from_model_error(provider_response_json_error(error))
        })?;
        parse_openai_response(request, &self.config, payload)
    }
}

#[derive(Debug, Clone, PartialEq, Error)]
#[error("{message}")]
pub struct ProviderError {
    pub message: String,
    pub error: Box<ModelError>,
}

impl ProviderError {
    pub fn from_model_error(error: ModelError) -> Self {
        Self {
            message: error.message.clone(),
            error: Box::new(error),
        }
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
        latency_ms: None,
        trace_event_ids: Vec::new(),
        raw_response_ref: None,
        metadata: json!({}),
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

pub fn provider_config_from_env<F>(get_env: F) -> ModelProviderConfig
where
    F: FnMut(&str) -> Option<String>,
{
    resolve_provider_config(get_env).config
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
                .map(openai_tool_payload)
                .collect::<Vec<_>>()
        );
        payload["tool_choice"] = openai_tool_choice_payload(request);
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

fn openai_tool_payload(tool: &ModelToolSchema) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": openai_wire_tool_name(&tool.name),
            "description": tool.description,
            "parameters": tool.parameters_schema,
        }
    })
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

fn parse_openai_response(
    request: &ModelTurnRequest,
    config: &OpenAiProviderConfig,
    payload: Value,
) -> Result<ModelTurnResponse, ProviderError> {
    let response_id = payload
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("response")
        .to_string();
    let choice = payload
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .ok_or_else(|| {
            ProviderError::from_model_error(
                ModelError::new(
                    ModelErrorKind::JsonSchemaViolation,
                    "provider response missing choices",
                )
                .with_provider(config.provider_name.clone())
                .with_model(config.model_name.clone())
                .with_raw_ref(format!("provider_response_ref:{response_id}")),
            )
        })?;
    let message = choice.get("message").unwrap_or(&Value::Null);
    let content = message
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
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
        model_name: Some(config.model_name.clone()),
        latency_ms: None,
        trace_event_ids: Vec::new(),
        raw_response_ref: Some(format!("provider_response_ref:{response_id}")),
        metadata: json!({}),
    };
    let allowed_tool_names = request
        .tools
        .iter()
        .map(|tool| tool.name.clone())
        .collect::<Vec<_>>();
    let validation = validate_model_turn_response(request, &response, &allowed_tool_names, None);
    if !validation.valid {
        response.status = ModelTurnStatus::Invalid;
        response.error = Some(
            ModelError::new(
                ModelErrorKind::JsonSchemaViolation,
                "provider response validation failed",
            )
            .with_provider(config.provider_name.clone())
            .with_model(config.model_name.clone())
            .with_raw_ref(format!("provider_response_ref:{response_id}")),
        );
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
    index: usize,
    call: &Value,
    tool_name_map: &[(String, String)],
) -> ModelToolCall {
    let function = call.get("function").unwrap_or(&Value::Null);
    let raw_arguments = function
        .get("arguments")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let (arguments, parse_status, validation_errors) = parse_tool_arguments(&raw_arguments);
    let wire_tool_name = function.get("name").and_then(Value::as_str).unwrap_or("");
    ModelToolCall {
        tool_call_id: call
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| format!("call_{index}")),
        tool_name: internal_tool_name(wire_tool_name, tool_name_map),
        arguments,
        raw_arguments,
        parse_status,
        validation_errors,
        provider_metadata: json!({}),
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
        .retryable(false),
    )
}

fn provider_source_missing_error() -> ProviderError {
    ProviderError::from_model_error(
        ModelError::new(
            ModelErrorKind::InvalidRequest,
            "provider configuration source is missing",
        )
        .retryable(false),
    )
}

fn model_error_from_http_status(status: u16, provider_name: &str, model_name: &str) -> ModelError {
    let kind = match status {
        HTTP_STATUS_UNAUTHORIZED | HTTP_STATUS_FORBIDDEN => ModelErrorKind::AuthError,
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
    ModelError::new(kind, message)
        .with_provider(provider_name.to_string())
        .with_model(model_name.to_string())
}

fn provider_transport_error(error: reqwest::Error) -> ProviderError {
    let kind = if error.is_timeout() {
        ModelErrorKind::Timeout
    } else {
        ModelErrorKind::NetworkError
    };
    ProviderError::from_model_error(
        ModelError::new(kind, "provider transport failed")
            .retryable(true)
            .with_raw_ref("provider_transport_error"),
    )
}

fn provider_response_json_error(_error: reqwest::Error) -> ModelError {
    ModelError::new(
        ModelErrorKind::JsonSchemaViolation,
        "provider response was not valid JSON",
    )
    .retryable(false)
    .with_raw_ref("provider_json_error")
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
    let mut errors = Vec::new();
    for (field, value) in [
        ("request_id_required", &request.request_id),
        ("run_id_required", &request.run_id),
        ("session_id_required", &request.session_id),
        ("task_id_required", &request.task_id),
        ("phase_id_required", &request.phase_id),
        ("action_id_required", &request.action_id),
    ] {
        if value.trim().is_empty() {
            errors.push(field.to_string());
        }
    }
    if request.messages.is_empty() {
        errors.push("messages_required".to_string());
    }
    validation_result(errors, Vec::new())
}

pub fn validate_model_turn_response(
    request: &ModelTurnRequest,
    response: &ModelTurnResponse,
    allowed_tool_names: &[String],
    capabilities: Option<&ModelCapabilities>,
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
    capabilities: Option<&ModelCapabilities>,
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

pub fn retry_decision(error: &ModelError, attempt: u32, max_retries: u32) -> ModelRetryDecision {
    if !error.retryable {
        return ModelRetryDecision {
            retry: false,
            next_attempt: None,
            reason: Some("non_retryable_model_error".to_string()),
        };
    }
    if attempt >= max_retries {
        return ModelRetryDecision {
            retry: false,
            next_attempt: None,
            reason: Some("retry_budget_exhausted".to_string()),
        };
    }
    ModelRetryDecision {
        retry: true,
        next_attempt: Some(attempt + 1),
        reason: Some("retryable_model_error".to_string()),
    }
}

pub fn validate_stream_events(events: &[ProviderStreamEvent]) -> ModelValidationResult {
    let mut errors = Vec::new();
    let mut response_completed = false;
    let mut seen_tool_calls = HashSet::new();
    for (index, event) in events.iter().enumerate() {
        if response_completed {
            errors.push(stream_error(index, "event_after_response_completed"));
            continue;
        }
        validate_stream_event(index, event, &mut errors, &mut seen_tool_calls);
        if event.event_type == ProviderStreamEventType::ResponseCompleted {
            response_completed = true;
        }
    }
    validation_result(errors, Vec::new())
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

fn default_retryable(kind: &ModelErrorKind) -> bool {
    matches!(
        kind,
        ModelErrorKind::NetworkError
            | ModelErrorKind::Timeout
            | ModelErrorKind::RateLimited
            | ModelErrorKind::ProviderOverloaded
    )
}

fn model_error_category(error: &ModelError) -> ModelErrorCategory {
    match error.kind {
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

fn validate_stream_event(
    index: usize,
    event: &ProviderStreamEvent,
    errors: &mut Vec<String>,
    seen_tool_calls: &mut HashSet<String>,
) {
    match event.event_type {
        ProviderStreamEventType::TextDelta if missing(&event.text_delta) => {
            errors.push(stream_error(index, "text_delta_required"));
        }
        ProviderStreamEventType::ToolCallDelta => {
            if missing(&event.tool_call_id) {
                errors.push(stream_error(index, "tool_call_id_required"));
            } else if let Some(tool_call_id) = &event.tool_call_id {
                seen_tool_calls.insert(tool_call_id.clone());
            }
        }
        ProviderStreamEventType::ToolCallCompleted => {
            if missing(&event.tool_call_id) {
                errors.push(stream_error(index, "tool_call_id_required"));
            } else if let Some(tool_call_id) = &event.tool_call_id
                && !seen_tool_calls.contains(tool_call_id)
            {
                errors.push(stream_error(index, "tool_call_delta_required"));
            }
        }
        ProviderStreamEventType::UsageDelta if event.usage_delta.is_none() => {
            errors.push(stream_error(index, "usage_delta_required"));
        }
        ProviderStreamEventType::Error if missing(&event.error) => {
            errors.push(stream_error(index, "error_required"));
        }
        _ => {}
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

fn stream_error(index: usize, code: &str) -> String {
    format!("{STREAM_EVENT_PREFIX}[{index}].{code}")
}

fn message_text(message: &ModelMessage) -> String {
    message
        .content
        .iter()
        .filter_map(|block| block.text.as_deref())
        .collect()
}

fn missing(value: &Option<String>) -> bool {
    value.as_deref().map(str::trim).unwrap_or("").is_empty()
}

#[derive(Default)]
struct ProviderConfigLayer {
    provider_name: Option<String>,
    model_name: Option<String>,
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
            base_url: get_env(ENV_BASE_URL),
            api_key: get_env(ENV_API_KEY),
        }
    }

    fn any_present(&self) -> bool {
        self.provider_name.is_some()
            || self.model_name.is_some()
            || self.base_url.is_some()
            || self.api_key.is_some()
    }

    fn into_values(self, source: ProviderConfigSource) -> ResolvedProviderValues {
        ResolvedProviderValues {
            source: Some(source),
            provider_name: normalized_provider_value(self.provider_name),
            model_name: normalized_provider_value(self.model_name),
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
