//! OpenAI 协议共用的响应解析原语。
use crate::error::ProviderError;
use crate::provider::contract::{provider_response_validation_error, validate_model_turn_response};
use crate::types::{ModelToolCall, ModelTurnRequest, ModelTurnResponse, ModelUsage};
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

/// 按字段名参数化构建一次工具调用：id_field 为调用 id 字段名，
/// name/arguments 为已定位的取值，与 parse_tool_arguments 共用参数校验。
pub(crate) fn parse_tool_call(
    call: &Value,
    id_field: &str,
    name: Option<&Value>,
    arguments: Option<&Value>,
) -> ModelToolCall {
    let (arguments, raw_arguments, validation_errors) = parse_tool_call_arguments(arguments);
    let wire_tool_name = name.and_then(Value::as_str).unwrap_or("");
    ModelToolCall {
        tool_call_id: call
            .get(id_field)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        tool_name: wire_tool_name.to_string(),
        arguments,
        raw_arguments,
        validation_errors,
    }
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

/// 解析 content 为纯文本。协议差异按参数区分：text_aliases 是 text 类型的
/// 额外别名（responses 的 output_text）；missing_error 为 None 时缺失
/// content 视为空文本（chat），否则返回该错误（responses）。
pub(crate) fn parse_message_content(
    content: Option<&Value>,
    text_aliases: &[&str],
    missing_error: Option<&'static str>,
    invalid_error: &'static str,
    part_unsupported_error: &'static str,
    part_text_missing_error: &'static str,
) -> Result<String, &'static str> {
    match content {
        None | Some(Value::Null) => match missing_error {
            Some(error) => Err(error),
            None => Ok(String::new()),
        },
        Some(Value::String(text)) => Ok(text.clone()),
        Some(Value::Array(parts)) => {
            let mut content = String::new();
            for part in parts {
                let part = part.as_object().ok_or(part_unsupported_error)?;
                let text = match part.get("type").and_then(Value::as_str) {
                    Some("text") => part.get("text").and_then(Value::as_str),
                    Some(alias) if text_aliases.contains(&alias) => {
                        part.get("text").and_then(Value::as_str)
                    }
                    Some("refusal") => part.get("refusal").and_then(Value::as_str),
                    _ => return Err(part_unsupported_error),
                }
                .ok_or(part_text_missing_error)?;
                content.push_str(text);
            }
            Ok(content)
        }
        Some(_) => Err(invalid_error),
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
