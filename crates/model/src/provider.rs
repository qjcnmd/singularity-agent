//! 模型调用的结果、流事件与协议校验。

use std::collections::HashSet;

use crate::error::{ModelErrorKind, ProviderError};
use crate::types::ModelTurnResponse;

/// 流式解码过程中发给调用方的增量事件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderStreamEvent {
    /// 工具调用名称或参数的生成增量，仅用于记录生成开始时间。
    ToolCallDelta,
    /// 模型响应里对外可见的正文增量。
    OutputTextDelta { delta: String },
    /// 模型公开的思考文本增量（Chat 的 reasoning 增量或 Responses 的 reasoning summary 增量），不含加密推理等提供方私有字段。
    ReasoningTextDelta { delta: String },
}

pub(crate) fn provider_response_validation_error(message: &str, errors: Vec<String>) -> ProviderError {
    ProviderError::diagnostic(ModelErrorKind::Other, message, errors)
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
