//! OpenAI 协议共用的响应解析原语。
use crate::error::ProviderError;
use crate::provider::contract::{provider_response_validation_error, validate_model_turn_response};
use crate::types::{ModelTurnResponse, ModelUsage};
use serde_json::Value;

/// 两种协议共用响应结构校验，具体参数是否合法由工具 preflight 决定。
pub(crate) fn finalize_provider_response(
    response: ModelTurnResponse,
) -> Result<ModelTurnResponse, ProviderError> {
    validate_model_turn_response(&response).map_err(|errors| {
        provider_response_validation_error("provider_response_invalid", errors)
    })?;
    Ok(response)
}

pub(crate) fn parse_tool_call_arguments(value: Option<&Value>) -> Result<Value, ProviderError> {
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
pub(crate) fn parse_tool_arguments(raw: &str) -> Value {
    serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_string()))
}

/// 按字段名参数化解析 usage：input_field/output_field 为计数顶层字段，
/// cached_path/reasoning_path 为嵌套 detail 的 JSON Pointer。每个数字字段只
/// 解析一次：取值与「是否提供」由同一个 Option 派生，未知用量不并成零。
///
/// `total_tokens` 是后续记账、聚合与实测校正共用的既定事实，在这里一次定好：
/// 供应商上报了可用数字就原样采用；该字段缺失或不是数字，而输入与输出都已上报
/// 时，用两者 `saturating_add` 补出总数；显式零与已上报分项矛盾时同样按分项补出
/// （总数不可能小于任一分项，而一条不可信的零会让实测校正失效）。`usage_present`
/// 标记输入与输出是否都上报，为 false 只说明分项不全：未被矛盾零值规则排除的有效
/// `total_tokens` 仍会保留，只有总数也缺失时才保持未知，不从局部计数伪造供应商
/// 用量。零值伴随 `usage_present = false` 与「真实零消费」保持区别。
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
    let parts = input_tokens
        .zip(output_tokens)
        .map(|(input, output)| input.saturating_add(output));
    let reported_total = count(usage.get("total_tokens"));
    let reported_zero_is_contradicted = reported_total == Some(0)
        && (input_tokens.unwrap_or_default() > 0 || output_tokens.unwrap_or_default() > 0);
    let total_tokens = if reported_zero_is_contradicted {
        parts
    } else {
        reported_total.or(parts)
    };
    ModelUsage {
        input_tokens: input_tokens.unwrap_or_default(),
        output_tokens: output_tokens.unwrap_or_default(),
        total_tokens: total_tokens.unwrap_or_default(),
        cached_input_tokens: cached_input_tokens.unwrap_or_default(),
        cached_input_tokens_present: cached_input_tokens.is_some(),
        reasoning_tokens: count(usage.pointer(reasoning_path)).unwrap_or_default(),
        usage_present: input_tokens.is_some() && output_tokens.is_some(),
    }
}
