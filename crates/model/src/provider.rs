//! 模型调用的结果、流事件、端点拼接、续接判断与公共响应校验。

use std::collections::{HashMap, HashSet};

use serde_json::Value;

use crate::config::selection::SelectedModel;
use crate::error::{ModelErrorKind, ProviderError};
use crate::types::{Message, ModelTurnResponse, ModelUsage, ProviderMessageMetadata};
use crate::{MESSAGES_PATH, MODELS_PATH, RESPONSES_PATH};

/// 流式解码过程中发给调用方的增量事件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderStreamEvent {
    /// 工具调用名称或参数的生成增量，仅用于记录生成开始时间。
    ToolCallDelta,
    /// 模型响应里对外可见的正文增量。
    OutputTextDelta { delta: String },
    /// 模型公开的思考文本增量（Messages 的 thinking 增量或 Responses 的 reasoning summary 增量），不含加密推理等提供方私有字段。
    ReasoningTextDelta { delta: String },
}

/// 去掉地址首尾空白和结尾的斜杠，其余部分（包括自定义路径）保持不变。
pub(crate) fn canonical_base_url(value: &str) -> &str {
    value.trim().trim_end_matches('/')
}

pub(crate) fn messages_endpoint(base_url: &str) -> String {
    format!("{}{MESSAGES_PATH}", canonical_base_url(base_url))
}

pub(crate) fn responses_endpoint(base_url: &str) -> String {
    format!("{}{RESPONSES_PATH}", canonical_base_url(base_url))
}

pub(crate) fn models_endpoint(base_url: &str) -> String {
    format!("{}{MODELS_PATH}", canonical_base_url(base_url))
}

/// 消息里提供方私有的续接数据（加密思考、签名等），只有提供方、模型、协议和请求地址
/// 都与生成时相同才发送；不同时返回 `None`，调用方只省略这部分，正文照常发送。
pub(crate) fn message_metadata_for<'a>(
    message: &'a Message,
    selection: &SelectedModel,
    provider_name: &str,
    endpoint: &str,
) -> Option<&'a ProviderMessageMetadata> {
    message.provider_metadata().filter(|replay| {
        replay.is_for_model(provider_name, &selection.model_name, selection.protocol, endpoint)
    })
}

pub(crate) struct ReasoningWireDecision<'a> {
    pub(crate) enabled: Option<bool>,
    pub(crate) effort: Option<&'a str>,
}

pub(crate) fn reasoning_wire_decision(selection: &SelectedModel) -> ReasoningWireDecision<'_> {
    ReasoningWireDecision {
        enabled: selection.reasoning_variant.as_ref().map(|variant| variant != "off"),
        effort: selection.reasoning_variant.as_deref().filter(|variant| !matches!(*variant, "off" | "on")),
    }
}

pub(crate) fn image_data_url<'a>(
    images: &'a HashMap<String, String>,
    image_id: &str,
) -> Result<&'a str, ProviderError> {
    images.get(image_id).map(String::as_str).ok_or_else(|| {
        ProviderError::new(ModelErrorKind::Other, format!("missing image data for attachment {image_id}"))
    })
}

pub(crate) fn parse_tool_call_arguments(value: Option<&Value>) -> Result<Value, ProviderError> {
    let reason = match value {
        Some(Value::String(raw)) => return Ok(parse_tool_arguments(raw)),
        Some(value @ Value::Object(_)) => return Ok(value.clone()),
        None => "tool_call_arguments_missing",
        Some(_) => "tool_call_arguments_type_invalid",
    };
    Err(provider_response_validation_error(
        "provider tool arguments were missing or had an invalid type",
        &[reason],
    ))
}

/// 解析工具参数 JSON 字符串；不是合法 JSON 时保留原始字符串，交由工具参数检查报错。
pub(crate) fn parse_tool_arguments(raw: &str) -> Value {
    serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_string()))
}

/// 还原发送给提供方的参数字符串；畸形参数保留原始字符串，避免二次 JSON 转义。
pub(crate) fn serialize_tool_call_arguments(arguments: &Value) -> String {
    match arguments {
        Value::String(raw) => raw.clone(),
        other => other.to_string(),
    }
}

pub(crate) fn parse_usage(
    usage: Option<&Value>,
    input_field: &str,
    output_field: &str,
    cached_path: &str,
) -> Option<ModelUsage> {
    let usage = usage?;
    let count = |value: Option<&Value>| value.and_then(Value::as_u64);
    let input_tokens = count(usage.get(input_field))?;
    let output_tokens = count(usage.get(output_field))?;
    Some(ModelUsage {
        input_tokens,
        output_tokens,
        total_tokens: count(usage.get("total_tokens"))
            .unwrap_or_else(|| input_tokens.saturating_add(output_tokens)),
        cached_input_tokens: count(usage.pointer(cached_path)),
    })
}

pub(crate) fn provider_response_validation_error(message: &str, errors: &[impl AsRef<str>]) -> ProviderError {
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
            &errors,
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
