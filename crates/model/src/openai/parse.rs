//! OpenAI 协议共用的响应解析原语。
use crate::error::ProviderError;
use crate::provider::contract::{provider_response_validation_error, validate_model_turn_response};
use crate::types::{ModelTurnRequest, ModelTurnResponse, ModelUsage};
use serde_json::Value;

/// 两种协议共用响应结构校验，具体参数是否合法由工具 preflight 决定。
pub fn finalize_provider_response(
    request: &ModelTurnRequest,
    response: ModelTurnResponse,
) -> Result<ModelTurnResponse, ProviderError> {
    validate_model_turn_response(request, &response).map_err(|errors| {
        provider_response_validation_error("provider_response_invalid", errors)
    })?;
    Ok(response)
}

pub fn parse_tool_call_arguments(value: Option<&Value>) -> Result<Value, ProviderError> {
    match value {
        Some(Value::String(raw)) => Ok(parse_tool_arguments(raw)),
        Some(value @ Value::Object(_)) => Ok(value.clone()),
        _ => Err(provider_response_validation_error(
            "provider_response_invalid",
            vec![
                if value.is_none() {
                    "tool_call_arguments_missing"
                } else {
                    "tool_call_arguments_type_invalid"
                }
                .into(),
            ],
        )),
    }
}

/// 无法解析的字符串原样保留。它不是合法工具参数，preflight 会明确拒绝。
pub fn parse_tool_arguments(raw: &str) -> Value {
    serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_string()))
}

/// 按字段名参数化解析 usage：input_field/output_field 为计数顶层字段，
/// cached_path/reasoning_path 为嵌套 detail 的 JSON Pointer。每个数字字段只
/// 解析一次：取值与「是否提供」由同一个 Option 派生，未知用量不并成零。
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
    let input_tokens = count(usage.get(input_field));
    let output_tokens = count(usage.get(output_field));
    let cached_input_tokens = count(usage.pointer(cached_path));
    ModelUsage {
        input_tokens: input_tokens.unwrap_or_default(),
        output_tokens: output_tokens.unwrap_or_default(),
        total_tokens: count(usage.get("total_tokens")).unwrap_or_default(),
        cached_input_tokens: cached_input_tokens.unwrap_or_default(),
        cached_input_tokens_present: cached_input_tokens.is_some(),
        reasoning_tokens: count(usage.pointer(reasoning_path)).unwrap_or_default(),
        usage_present: input_tokens.is_some() && output_tokens.is_some(),
    }
}
