//! Chat 与 Responses 共用的工具参数和 Token 用量解析。
use crate::error::ProviderError;
use crate::provider::contract::provider_response_validation_error;
use crate::types::ModelUsage;
use serde_json::Value;

pub(crate) fn parse_tool_call_arguments(value: Option<&Value>) -> Result<Value, ProviderError> {
    let reason = match value {
        Some(Value::String(raw)) => return Ok(parse_tool_arguments(raw)),
        Some(value @ Value::Object(_)) => return Ok(value.clone()),
        None => "tool_call_arguments_missing",
        Some(_) => "tool_call_arguments_type_invalid",
    };
    Err(provider_response_validation_error(
        "provider tool arguments were missing or had an invalid type",
        vec![reason.into()],
    ))
}

/// 解析工具参数 JSON 字符串；不是合法 JSON 时保留原始字符串，交由工具参数检查报错。
pub(crate) fn parse_tool_arguments(raw: &str) -> Value {
    serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_string()))
}

/// 解析上游返回的 Token 用量（输入、输出、总数与缓存命中）。
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
