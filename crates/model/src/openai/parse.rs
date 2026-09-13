//! OpenAI 协议共用的响应解析原语。
use crate::error::ProviderError;
use crate::provider::contract::{provider_response_validation_error, validate_model_turn_response};
use crate::types::{ModelTurnRequest, ModelTurnResponse, ModelUsage};
use serde_json::{Value, json};

/// 两种协议共用完成响应校验，可恢复的参数错误留给工具派发处理。
pub fn finalize_provider_response(
    request: &ModelTurnRequest,
    response: ModelTurnResponse,
) -> Result<ModelTurnResponse, ProviderError> {
    // 不可恢复的响应校验失败在本边界直接类型化失败（与请求校验同路径）；
    // 可恢复的畸形工具参数保持 Success，交由 AgentLoop 的工具派发产出
    // 模型可见的校验结果。
    if let Err(errors) = validate_model_turn_response(request, &response)
        && errors.iter().any(|error| {
            !matches!(
                error.as_str(),
                "invalid_json" | "tool_call_arguments_must_be_object"
            )
        })
    {
        return Err(provider_response_validation_error(
            "provider_response_invalid",
            errors,
        ));
    }
    Ok(response)
}

pub fn parse_tool_call_arguments(arguments_value: Option<&Value>) -> (Value, String, Vec<String>) {
    let Some(arguments_value) = arguments_value else {
        return (
            json!({}),
            String::new(),
            vec!["tool_call_arguments_missing".to_string()],
        );
    };
    match arguments_value {
        Value::String(raw_arguments) => {
            let (arguments, validation_errors) = parse_tool_arguments(raw_arguments);
            (arguments, raw_arguments.clone(), validation_errors)
        }
        Value::Object(_) => (
            arguments_value.clone(),
            serde_json::to_string(arguments_value).unwrap_or_default(),
            Vec::new(),
        ),
        _ => (
            json!({}),
            String::new(),
            vec!["tool_call_arguments_type_invalid".to_string()],
        ),
    }
}

pub fn parse_tool_arguments(raw_arguments: &str) -> (Value, Vec<String>) {
    match serde_json::from_str::<Value>(raw_arguments) {
        Ok(arguments) if arguments.is_object() => (arguments, Vec::new()),
        Ok(arguments) => (
            arguments,
            vec!["tool_call_arguments_must_be_object".to_string()],
        ),
        Err(_) => (json!({}), vec!["invalid_json".to_string()]),
    }
}

/// 按字段名参数化解析 usage：input_field/output_field 为计数顶层字段，
/// cached_path/reasoning_path 为嵌套 detail 的 JSON Pointer。
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
    ModelUsage {
        input_tokens: usage
            .get(input_field)
            .and_then(Value::as_u64)
            .unwrap_or_default(),
        output_tokens: usage
            .get(output_field)
            .and_then(Value::as_u64)
            .unwrap_or_default(),
        total_tokens: usage
            .get("total_tokens")
            .and_then(Value::as_u64)
            .unwrap_or_default(),
        cached_input_tokens: usage
            .pointer(cached_path)
            .and_then(Value::as_u64)
            .unwrap_or_default(),
        cached_input_tokens_present: usage.pointer(cached_path).and_then(Value::as_u64).is_some(),
        reasoning_tokens: usage
            .pointer(reasoning_path)
            .and_then(Value::as_u64)
            .unwrap_or_default(),
        usage_present: usage.get(input_field).and_then(Value::as_u64).is_some()
            && usage.get(output_field).and_then(Value::as_u64).is_some(),
    }
}
