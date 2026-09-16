//! OpenAI 协议共用的响应解析原语。
use crate::error::ProviderError;
use crate::provider::contract::{provider_response_validation_error, validate_model_turn_response};
use crate::types::{ModelTurnRequest, ModelTurnResponse, ModelUsage};
use serde_json::Value;

/// 两种协议共用响应结构校验，具体参数是否合法由工具 preflight 决定。
pub(crate) fn finalize_provider_response(
    request: &ModelTurnRequest,
    response: ModelTurnResponse,
) -> Result<ModelTurnResponse, ProviderError> {
    validate_model_turn_response(request, &response).map_err(|errors| {
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
/// 供应商上报了可用数字就原样采用（含显式零）；该字段缺失或不是数字，而输入与
/// 输出都已上报时，用两者 `saturating_add` 补出总数；输入或输出缺失时用量整体
/// 未测量，总数保持未知，不从局部计数伪造供应商用量。`usage_present` 标记输入
/// 与输出是否都上报；零值伴随 `usage_present = false` 与「真实零消费」保持区别。
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
    let total_tokens = count(usage.get("total_tokens")).or_else(|| {
        input_tokens
            .zip(output_tokens)
            .map(|(input, output)| input.saturating_add(output))
    });
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

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)] // 测试断言惯例
mod tests {
    use super::parse_usage;
    use serde_json::json;

    fn chat_usage(value: serde_json::Value) -> crate::ModelUsage {
        parse_usage(
            Some(&value),
            "prompt_tokens",
            "completion_tokens",
            "/prompt_tokens_details/cached_tokens",
            "/completion_tokens_details/reasoning_tokens",
        )
    }

    fn responses_usage(value: serde_json::Value) -> crate::ModelUsage {
        parse_usage(
            Some(&value),
            "input_tokens",
            "output_tokens",
            "/input_tokens_details/cached_tokens",
            "/output_tokens_details/reasoning_tokens",
        )
    }

    /// 输入与输出都已上报而总数没有可用数字时，由两者补出总数：两种协议的
    /// usage 形状在解析处得到同一个既定事实，后续消费者不再各自修补。
    #[test]
    fn a_missing_total_is_derived_from_known_input_and_output() {
        let chat = chat_usage(json!({"prompt_tokens": 10, "completion_tokens": 2}));
        assert_eq!(chat.input_tokens, 10);
        assert_eq!(chat.output_tokens, 2);
        assert_eq!(chat.total_tokens, 12);
        assert!(chat.usage_present, "input and output were both reported");

        let responses = responses_usage(json!({"input_tokens": 10, "output_tokens": 2}));
        assert_eq!(responses.total_tokens, 12);
        assert!(responses.usage_present);
    }

    /// 供应商上报的总数原样采用；显式零与字段缺失必须保持区别。
    #[test]
    fn a_reported_total_is_used_verbatim_and_explicit_zero_is_not_missing() {
        let reported =
            chat_usage(json!({"prompt_tokens": 10, "completion_tokens": 2, "total_tokens": 99}));
        assert_eq!(
            reported.total_tokens, 99,
            "a reported total is not recomputed"
        );

        // 同样的输入输出：总数显式为零时保留零，字段缺失时补出 12。
        let explicit_zero =
            chat_usage(json!({"prompt_tokens": 7, "completion_tokens": 5, "total_tokens": 0}));
        assert_eq!(explicit_zero.total_tokens, 0);
        let absent = chat_usage(json!({"prompt_tokens": 7, "completion_tokens": 5}));
        assert_eq!(absent.total_tokens, 12);

        let zero = chat_usage(json!({
            "prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0,
            "prompt_tokens_details": {"cached_tokens": 0}
        }));
        assert_eq!(zero.total_tokens, 0);
        assert!(zero.usage_present);
        assert!(zero.cached_input_tokens_present);
        assert_eq!(zero.cached_input_tokens, 0);

        let absent_cache =
            chat_usage(json!({"prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0}));
        assert!(!absent_cache.cached_input_tokens_present);
    }

    /// 输入或输出缺失时用量整体未测量：总数保持未知，不从局部计数伪造。
    #[test]
    fn a_partial_usage_is_not_a_measurement() {
        for value in [
            json!({"prompt_tokens": 10}),
            json!({"completion_tokens": 2}),
            json!({}),
        ] {
            let usage = chat_usage(value.clone());
            assert_eq!(usage.total_tokens, 0, "{value}");
            assert!(!usage.usage_present, "{value}");
        }
        // usage 对象缺失同样是未知，而不是零消费。
        let missing = parse_usage(None, "prompt_tokens", "completion_tokens", "", "");
        assert!(!missing.usage_present);
        assert_eq!(missing, crate::ModelUsage::default());
    }

    /// 总数不是可用数字时按缺失处理，仍由已知的输入输出补出。
    #[test]
    fn an_unusable_total_field_falls_back_to_the_known_counts() {
        for value in [
            json!({"prompt_tokens": 3, "completion_tokens": 4, "total_tokens": null}),
            json!({"prompt_tokens": 3, "completion_tokens": 4, "total_tokens": "7"}),
        ] {
            assert_eq!(chat_usage(value.clone()).total_tokens, 7, "{value}");
        }
    }

    /// 补出总数用饱和相加：极端计数不会溢出。
    #[test]
    fn a_derived_total_saturates_instead_of_overflowing() {
        let usage = chat_usage(json!({"prompt_tokens": u64::MAX, "completion_tokens": 2}));
        assert_eq!(usage.total_tokens, u64::MAX);
    }
}
