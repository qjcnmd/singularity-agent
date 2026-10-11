//! OpenAI 两种协议共用的响应解析原语。
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

/// 解析工具参数 JSON 字符串；解析失败时保留原始字符串供预检阶段报错。
pub(crate) fn parse_tool_arguments(raw: &str) -> Value {
    serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_string()))
}

/// 解析上游返回的 Token 用量（输入、输出、缓存命中与思考消耗）。
pub(crate) fn parse_usage(
    usage: Option<&Value>,
    input_field: &str,
    output_field: &str,
    cached_path: &str,
    reasoning_path: &str,
) -> ModelUsage {
    let Some(usage) = usage else {
        return ModelUsage::default();
    };
    let count = |value: Option<&Value>| value.and_then(Value::as_u64);
    let (Some(input_tokens), Some(output_tokens)) =
        (count(usage.get(input_field)), count(usage.get(output_field)))
    else {
        return ModelUsage::default();
    };
    ModelUsage {
        input_tokens,
        output_tokens,
        total_tokens: count(usage.get("total_tokens"))
            .unwrap_or_else(|| input_tokens.saturating_add(output_tokens)),
        cached_input_tokens: count(usage.pointer(cached_path)),
        reasoning_tokens: count(usage.pointer(reasoning_path)).unwrap_or_default(),
        usage_present: true,
    }
}
