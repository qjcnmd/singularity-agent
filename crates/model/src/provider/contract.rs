//! 模型请求、模型响应与 provider 能力契约的本地校验和能力声明。

pub use singularity_protocol::ProviderApiProtocol;
use std::collections::HashSet;

use crate::MAX_TOOLS_PER_REQUEST;
use crate::error::{ModelErrorKind, ProviderError};
use crate::types::{ModelRole, ModelTurnRequest, ModelTurnResponse};

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

/// Chat 兼容端点的 finish_reason 为 "network_error" 时，表示生成过程中网络出了故障。
pub(crate) fn provider_finish_network_error(message: &str) -> ProviderError {
    ProviderError::new(ModelErrorKind::NetworkError, message).with_code("network_error")
}

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

/// 校验模型响应的结构性事实：角色、正文非空、工具名非空、调用 ID 非空且唯一。工具是否
/// 存在、参数是否有效由 preflight 判定并以模型可见的失败结果回到主循环，协议层不提前终结。
pub fn validate_model_turn_response(response: &ModelTurnResponse) -> Result<(), Vec<String>> {
    let mut errors = Vec::new();
    let tool_calls = response.tool_calls();
    match &response.assistant_message {
        message if message.role != ModelRole::Assistant => {
            errors.push("non_assistant_response".to_string());
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
        }
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
