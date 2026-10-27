//! 检查提供方返回的回复，以及几类提供方错误的构造。

pub use singularity_protocol::ProviderApiProtocol;
use std::collections::HashSet;

use crate::error::{ModelErrorKind, ProviderError};
use crate::types::ModelTurnResponse;

pub(crate) fn provider_response_validation_error(message: &str, errors: Vec<String>) -> ProviderError {
    ProviderError::diagnostic(ModelErrorKind::JsonSchemaViolation, message, errors)
}

pub(crate) fn provider_content_filter_error(message: &str) -> ProviderError {
    ProviderError::new(ModelErrorKind::ContentFilter, message)
}

/// 构造流式生成过程中的网络错误（对应 finish_reason 为 "network_error"）。
pub(crate) fn provider_finish_network_error(message: &str) -> ProviderError {
    ProviderError::new(ModelErrorKind::Temporary, message)
}

/// 校验模型响应：非截断响应须有正文或工具调用，工具调用 ID 非空且不重复、工具名非空。
pub(crate) fn finalize_provider_response(mut response: ModelTurnResponse) -> ModelTurnResponse {
    let mut errors = Vec::new();
    if !response.is_length_truncated()
        && !response
            .assistant_message
            .content()
            .iter()
            .any(|block| matches!(block, crate::ContentBlock::Text { text, .. } if !text.trim().is_empty()))
        && response.assistant_message.tool_calls().next().is_none()
    {
        errors.push("empty_response".to_string());
    }

    let mut seen = HashSet::new();
    for call in response.assistant_message.tool_calls() {
        if call.tool_call_id.trim().is_empty() {
            errors.push("missing_tool_call_id".to_string());
        } else if !seen.insert(call.tool_call_id.as_str()) {
            errors.push("duplicate_tool_call_id".to_string());
        }
        if call.tool_name.trim().is_empty() {
            errors.push("missing_tool_name".to_string());
        }
    }

    errors.sort();
    errors.dedup();
    if !errors.is_empty() {
        response.stop_reason = crate::ModelStopReason::Failed(provider_response_validation_error(
            "provider response structure was invalid",
            errors,
        ));
        response.usage = None;
        if let crate::Message::Assistant { content, provider_metadata } = &mut response.assistant_message {
            content.retain(|block| {
                matches!(block, crate::ContentBlock::Text { .. } | crate::ContentBlock::Thinking { .. })
            });
            *provider_metadata = None;
        }
    }
    response
}
