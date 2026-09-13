//! 模型请求、响应和 provider capability contract 的本地校验与能力声明。

use serde::{Deserialize, Serialize};
pub use singularity_protocol::ProviderApiProtocol;
use std::collections::HashSet;

use crate::MAX_TOOLS_PER_REQUEST;
use crate::error::{ModelErrorKind, ProviderError};
use crate::types::{ModelRole, ModelTurnRequest, ModelTurnResponse};

/// Chat Completions reasoning 字段由模型目录显式选择；不解释任何
/// provider 或模型名来决定 wire 形状。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingWireFormat {
    /// 既有 thinking: {"type": "enabled|disabled"} 字段。
    ThinkingType,
    /// 文档化此能力的 provider 使用顶层 enable_thinking 布尔。
    EnableThinking,
    /// 思考开关无独立 wire 字段：仅发送 reasoning_effort（部分
    /// OpenAI 兼容网关的 Chat 形状）。
    ReasoningEffort,
}

pub(crate) fn request_uses_tool_protocol(request: &ModelTurnRequest) -> bool {
    !request.tools.is_empty()
        || request
            .messages
            .iter()
            .any(|message| message.role == ModelRole::Tool || !message.tool_calls.is_empty())
}

pub(crate) fn provider_request_validation_error(errors: Vec<String>) -> ProviderError {
    ProviderError::diagnostic(
        ModelErrorKind::InvalidRequest,
        "model request validation failed",
        "provider_request_invalid",
        errors,
    )
}

pub(crate) fn provider_response_validation_error(
    message: &str,
    errors: Vec<String>,
) -> ProviderError {
    ProviderError::diagnostic(
        ModelErrorKind::JsonSchemaViolation,
        message,
        "provider_response_invalid",
        errors,
    )
}

pub(crate) fn provider_content_filter_error(message: &str) -> ProviderError {
    ProviderError::new(ModelErrorKind::ContentFilter, message).with_code("content_filter")
}

/// Chat 兼容端点的 finish_reason: "network_error" 表示生成期网络故障。
pub(crate) fn provider_finish_network_error(message: &str) -> ProviderError {
    ProviderError::new(ModelErrorKind::NetworkError, message).with_code("network_error")
}

/// 校验带 provider 能力约束的模型请求。
pub fn validate_model_request(
    request: &ModelTurnRequest,
    max_output_tokens: u32,
) -> Result<(), Vec<String>> {
    let mut errors = Vec::new();
    if request.request_id.trim().is_empty() {
        errors.push("request_id_required".to_string());
    }
    if request.messages.is_empty() {
        errors.push("messages_required".to_string());
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
    if let Some(requested_output_tokens) = request.model_preferences.max_output_tokens
        && requested_output_tokens > max_output_tokens
    {
        errors.push("requested_output_tokens_exceed_provider_limit".to_string());
    }
    if request.tools.len() > MAX_TOOLS_PER_REQUEST {
        errors.push("requested_tools_exceed_provider_limit".to_string());
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

/// 根据对应请求校验完整的模型提供方响应。
pub fn validate_model_turn_response(
    request: &ModelTurnRequest,
    response: &ModelTurnResponse,
) -> Result<(), Vec<String>> {
    let mut errors = Vec::new();
    let tool_calls = response.tool_calls();
    match &response.assistant_message {
        message if message.role != ModelRole::Assistant => {
            errors.push("non_assistant_response".to_string());
        }
        message
            if tool_calls.is_empty()
                && request_uses_tool_protocol(request)
                && is_text_tool_call_envelope(&message.content) =>
        {
            errors.push("text_tool_call_envelope_not_supported".to_string());
        }
        message if message.content.trim().is_empty() && tool_calls.is_empty() => {
            errors.push("empty_response".to_string());
        }
        _ => {}
    }

    if tool_calls
        .iter()
        .map(|call| call.tool_name.as_str())
        .any(|name| !name.trim().is_empty() && !is_portable_tool_name(name))
    {
        errors.push("tool_name_not_provider_portable".to_string());
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
        } else if !request.tools.iter().any(|tool| tool.name == call.tool_name) {
            errors.push("unknown_tool".to_string());
        }
        if !call.arguments.is_object() {
            errors.push("tool_call_arguments_must_be_object".to_string());
        }
        errors.extend(call.validation_errors.iter().cloned());
    }

    validation_result(errors)
}

fn validation_result(mut errors: Vec<String>) -> Result<(), Vec<String>> {
    errors.sort();
    errors.dedup();
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

fn is_text_tool_call_envelope(text: &str) -> bool {
    text.find("<tool_call>")
        .is_some_and(|start| text[start + "<tool_call>".len()..].contains("</tool_call>"))
}
