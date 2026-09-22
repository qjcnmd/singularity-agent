//! OpenAI 两种协议共用的响应解析原语。
use crate::error::ProviderError;
use crate::provider::contract::{provider_response_validation_error, validate_model_turn_response};
use crate::types::{ModelTurnResponse, ModelUsage};
use serde_json::Value;

/// 两种协议共用的响应结构校验；工具参数是否合法由工具 preflight 判定。
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

/// 解析不了的字符串原样保留。它不是合法的工具参数，preflight 会明确拒绝。
pub(crate) fn parse_tool_arguments(raw: &str) -> Value {
    serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_string()))
}

/// 按字段名解析 usage：input_field/output_field 是顶层计数，cached_path/reasoning_path 是
/// 指向嵌套 detail 的 JSON Pointer；取值与「是否上报」来自同一个 Option，没上报的不并成零。
///
/// `total_tokens` 要被记账、聚合和实测校正共用，所以在这里一次定好：供应商给了可用数字就
/// 原样采用；缺失、不是数字或小于已上报的输入输出之和时，按 `saturating_add` 补出。
/// `usage_present` 表示输入输出是否都上报；为 false 只说明分项不全，有效的 `total_tokens`
/// 仍保留，只有总数也缺失时才保持未知，绝不从局部计数编造供应商用量（上报零且
/// `usage_present = false` 与「真实零消费」仍分得清）。
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
    let reported_total_is_contradicted = reported_total.is_some_and(|total| {
        total
            < input_tokens
                .unwrap_or_default()
                .saturating_add(output_tokens.unwrap_or_default())
    });
    let total_tokens = if reported_total_is_contradicted {
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
