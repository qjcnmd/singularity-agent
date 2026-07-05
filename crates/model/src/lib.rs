#![forbid(unsafe_code)]

use std::collections::HashSet;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const DEFAULT_MAX_TOOL_CALLS: u32 = 8;
const DEFAULT_MAX_RETRIES: u32 = 2;
const DEFAULT_MAX_CONTEXT_TOKENS: u32 = 128_000;
const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 4_096;
const STREAM_EVENT_PREFIX: &str = "stream_event";

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
    pub metadata: Value,
}

impl ModelMessage {
    pub fn text(role: ModelRole, content: impl Into<String>) -> Self {
        Self {
            role,
            content: vec![ContentBlock::text(content)],
            name: None,
            tool_call_id: None,
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
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

impl Default for ModelPreferences {
    fn default() -> Self {
        Self {
            provider_name: None,
            model_name: None,
            temperature: None,
            top_p: None,
            max_output_tokens: None,
            json_mode: false,
            structured_output_schema: None,
            stream: false,
            fallback_models: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ModelProviderConfig {
    pub provider_name: Option<String>,
    pub model_name: Option<String>,
    pub base_url_present: bool,
    pub api_key_present: bool,
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ModelUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    pub cached_input_tokens: u64,
    pub reasoning_tokens: u64,
    pub cost_estimate: Option<f64>,
}

impl Default for ModelUsage {
    fn default() -> Self {
        Self {
            input_tokens: 0,
            output_tokens: 0,
            total_tokens: 0,
            cached_input_tokens: 0,
            reasoning_tokens: 0,
            cost_estimate: None,
        }
    }
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

    pub fn category(&self) -> ModelErrorCategory {
        classify_model_error(self)
    }
}

pub fn classify_model_error(error: &ModelError) -> ModelErrorCategory {
    model_error_category(error)
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
