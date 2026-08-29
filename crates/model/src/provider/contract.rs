//! 模型请求、响应和 provider capability contract 的本地校验与能力声明。

use serde::{Deserialize, Serialize};
use std::collections::HashSet;

use super::runtime::OpenAiProviderConfig;
use crate::error::{
    ModelError, ModelErrorCategory, ModelErrorKind, ProviderError, ProviderErrorStage,
};
use crate::types::{
    ModelMessage, ModelRole, ModelToolCall, ModelToolParseStatus, ModelTurnRequest,
    ModelTurnResponse, ModelTurnStatus, ModelValidationResult, ProviderToolReasoningMode,
};
use crate::{
    DEFAULT_MAX_CONTEXT_TOKENS, DEFAULT_MAX_OUTPUT_TOKENS, DEFAULT_MAX_TOOLS_PER_REQUEST,
    TEXT_TOOL_CALL_ENVELOPE_ERROR,
};

/// 为模型提供方完成请求选定的线路协议。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderApiProtocol {
    OpenAiResponses,
    OpenAiChatCompletions,
}

/// Chat Completions reasoning 字段由模型目录显式选择；不解释任何
/// provider 或模型名来决定 wire 形状。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingWireFormat {
    /// 既有 `thinking: {"type": "enabled|disabled"}` 字段。
    ThinkingType,
    /// 文档化此能力的 provider 使用顶层 `enable_thinking` 布尔。
    EnableThinking,
}

/// 模型提供方必须遵守、用于构建请求和校验响应的能力。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderProtocolContract {
    pub supports_tools: bool,
    pub tool_reasoning_mode: ProviderToolReasoningMode,
    pub max_tools_per_request: u32,
    pub supports_system_message: bool,
    pub max_context_tokens: Option<u32>,
    pub max_output_tokens: u32,
}

impl Default for ProviderProtocolContract {
    fn default() -> Self {
        Self {
            supports_tools: true,
            tool_reasoning_mode: ProviderToolReasoningMode::Unspecified,
            max_tools_per_request: DEFAULT_MAX_TOOLS_PER_REQUEST,
            supports_system_message: true,
            max_context_tokens: Some(DEFAULT_MAX_CONTEXT_TOKENS),
            max_output_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
        }
    }
}

use crate::config::schema::ModelProviderConfig;

impl ModelValidationResult {
    /// 构造通过校验的结果。
    pub fn valid() -> Self {
        Self {
            valid: true,
            errors: Vec::new(),
        }
    }

    /// 构造带错误的失败结果。
    pub fn invalid(errors: Vec<String>) -> Self {
        Self {
            valid: false,
            errors,
        }
    }
}

pub(crate) fn request_uses_tool_protocol(request: &ModelTurnRequest) -> bool {
    !request.tools.is_empty()
        || request
            .messages
            .iter()
            .any(|message| message.role == ModelRole::Tool || !message.tool_calls.is_empty())
}

pub(crate) fn provider_request_validation_error(
    validation: ModelValidationResult,
    config: &OpenAiProviderConfig,
) -> ProviderError {
    let kind = if validation_is_unsupported_capability(&validation) {
        ModelErrorKind::UnsupportedCapability
    } else {
        ModelErrorKind::InvalidRequest
    };
    ProviderError::from_model_error(
        ModelError::diagnostic(
            kind,
            format!(
                "model request validation failed: {}",
                validation.errors.join(", ")
            ),
            "provider_request_invalid",
            ProviderErrorStage::RequestSend,
            validation.errors,
        )
        .with_provider(config.provider_name.clone())
        .with_model(config.model_name.clone()),
    )
}

pub(crate) fn provider_response_validation_error(
    config: &OpenAiProviderConfig,
    model_name: &str,
    message: &str,
    validation_errors: Vec<String>,
) -> ProviderError {
    ProviderError::from_model_error(
        ModelError::diagnostic(
            ModelErrorKind::JsonSchemaViolation,
            message,
            "provider_response_invalid",
            ProviderErrorStage::ResponseValidation,
            validation_errors,
        )
        .with_provider(config.provider_name.clone())
        .with_model(model_name.to_string()),
    )
}

pub(crate) fn provider_content_filter_error(
    config: &OpenAiProviderConfig,
    model_name: &str,
    message: &str,
) -> ProviderError {
    ProviderError::from_model_error(
        ModelError::diagnostic(
            ModelErrorKind::ContentFilter,
            message,
            "content_filter",
            ProviderErrorStage::ResponseValidation,
            vec!["content_filter".to_string()],
        )
        .with_provider(config.provider_name.clone())
        .with_model(model_name.to_string()),
    )
}

fn validation_is_unsupported_capability(validation: &ModelValidationResult) -> bool {
    !validation.errors.is_empty()
        && validation
            .errors
            .iter()
            .all(|error| error.as_str() == "provider_does_not_support_tools")
}

/// 检查脱敏模型提供方配置是否包含全部必需值。
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
    validation_result(errors)
}

/// 校验带可选 provider 能力约束的模型请求。
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
    if let Some(capabilities) = capabilities {
        if !request.tools.is_empty() && !capabilities.supports_tools {
            errors.push("provider_does_not_support_tools".to_string());
        }
        if request
            .messages
            .iter()
            .any(|message| message.role == ModelRole::System)
            && !capabilities.supports_system_message
        {
            errors.push("provider_does_not_support_system_messages".to_string());
        }
        if let Some(requested_output_tokens) = request.model_preferences.max_output_tokens
            && requested_output_tokens > capabilities.max_output_tokens
        {
            errors.push("requested_output_tokens_exceed_provider_limit".to_string());
        }
        if request.tools.len() > capabilities.max_tools_per_request as usize {
            errors.push("requested_tools_exceed_provider_limit".to_string());
        }
    }
    validation_result(errors)
}

fn is_portable_tool_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
}

/// 根据对应请求和协商能力校验完整的模型提供方 turn。
pub fn validate_model_turn_response(
    request: &ModelTurnRequest,
    response: &ModelTurnResponse,
    capabilities: Option<&ProviderProtocolContract>,
) -> ModelValidationResult {
    let mut result = validate_model_response_with_protocol_context(
        response.assistant_message.as_ref(),
        response.tool_calls(),
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
    result.errors.sort();
    result.errors.dedup();
    result.valid = result.errors.is_empty();
    result
}

fn validate_model_response_with_protocol_context(
    assistant_message: Option<&ModelMessage>,
    tool_calls: &[ModelToolCall],
    capabilities: Option<&ProviderProtocolContract>,
    tool_protocol_active: bool,
) -> ModelValidationResult {
    let mut errors = Vec::new();

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

    if tool_calls
        .iter()
        .chain(
            assistant_message
                .iter()
                .flat_map(|message| message.tool_calls.iter()),
        )
        .map(|call| call.tool_name.as_str())
        .any(|name| !name.trim().is_empty() && !is_portable_tool_name(name))
    {
        errors.push("tool_name_not_provider_portable".to_string());
    }

    if let Some(capabilities) = capabilities
        && !tool_calls.is_empty()
        && !capabilities.supports_tools
    {
        errors.push("provider_does_not_support_tools".to_string());
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
        match call.parse_status {
            ModelToolParseStatus::InvalidJson => errors.push("invalid_json".to_string()),
            ModelToolParseStatus::SchemaMismatch => errors.push("schema_mismatch".to_string()),
            ModelToolParseStatus::UnknownTool => {}
            ModelToolParseStatus::Valid => {}
        }
    }

    validation_result(errors)
}

pub(crate) fn model_error_category(error: &ModelError) -> ModelErrorCategory {
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
        ModelErrorKind::JsonSchemaViolation => ModelErrorCategory::JsonSchema,
        ModelErrorKind::ContentFilter => ModelErrorCategory::ContentFilter,
        ModelErrorKind::UnsupportedCapability => ModelErrorCategory::UnsupportedCapability,
        ModelErrorKind::RateLimited | ModelErrorKind::ProviderOverloaded => {
            ModelErrorCategory::ProviderUnavailable
        }
        ModelErrorKind::UnknownProviderError => ModelErrorCategory::UnknownProviderError,
    }
}

fn validation_result(mut errors: Vec<String>) -> ModelValidationResult {
    errors.sort();
    errors.dedup();
    if errors.is_empty() {
        ModelValidationResult::valid()
    } else {
        ModelValidationResult::invalid(errors)
    }
}

pub(crate) fn message_text(message: &ModelMessage) -> &str {
    &message.content
}

fn is_text_tool_call_envelope(text: &str) -> bool {
    text.find("<tool_call>")
        .is_some_and(|start| text[start + "<tool_call>".len()..].contains("</tool_call>"))
}

fn missing(value: &Option<String>) -> bool {
    value.as_deref().map(str::trim).unwrap_or("").is_empty()
}
