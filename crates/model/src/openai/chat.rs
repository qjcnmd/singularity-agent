//! OpenAI Chat Completions 协议请求序列化与响应解析。

use serde_json::{Value, json};

use crate::error::ProviderError;
use crate::provider::contract::{
    ThinkingWireFormat, message_text, provider_content_filter_error, provider_finish_network_error,
    provider_response_validation_error, validate_model_turn_response,
};
use crate::provider::runtime::{OpenAiProviderConfig, SelectedModel};
use crate::transport::{provider_embedded_error, provider_error_fields};
use crate::types::{
    ModelMessage, ModelRole, ModelToolCall, ModelToolParseStatus, ModelToolSchema,
    ModelTurnRequest, ModelTurnResponse, ModelUsage, ProviderReasoningReplay,
};

pub fn openai_chat_stream_request_payload(
    request: &ModelTurnRequest,
    model_name: &str,
    selection: &SelectedModel,
) -> Value {
    let mut payload = json!({
        "model": request
            .model_preferences
            .model_name
            .as_deref()
            .unwrap_or(model_name),
        "messages": request
            .messages
            .iter()
            .map(|message| {
                openai_message_payload_with_reasoning(
                    message,
                    selection.supports_developer_role,
                    selection.requires_assistant_content_for_tool_calls,
                )
            })
            .collect::<Vec<_>>(),
        "stream": true,
        // provider 实现 OpenAI 兼容 include_usage 扩展时，在最终流块中请求
        // usage；不支持的 provider 仍产生合法响应（usage_present=false）。
        "stream_options": {"include_usage": true},
    });
    let reasoning = super::reasoning_wire_decision(selection);
    // 输出上限 wire 字段取舍：chat completions 走 max_tokens（第三方兼容
    // 端点如 DeepSeek/dashscope 接受），responses 走 max_output_tokens
    // （OpenAI 官方 Responses API 命名）。官方 chat 对推理系模型要求
    // max_completion_tokens，本层不针对推理模型切换字段；推理模型经
    // chat 兼容端点使用时若需输出上限，由用户在配置中显式声明。
    if let Some(max_output_tokens) = request.model_preferences.max_output_tokens {
        payload["max_tokens"] = json!(max_output_tokens);
    }
    if let Some(enabled) = reasoning.enabled {
        apply_thinking_wire(&mut payload, enabled, selection.thinking_wire_format);
        if enabled && let Some(wire_effort) = reasoning.effort {
            payload["reasoning_effort"] = json!(wire_effort);
        }
    }
    if !request.tools.is_empty() {
        payload["tools"] = json!(
            request
                .tools
                .iter()
                .map(openai_tool_payload)
                .collect::<Vec<_>>()
        );
        if selection.supports_tool_choice {
            payload["tool_choice"] = serde_json::json!("auto");
        }
    }
    payload
}

/// Thinking 开关的协议落点统一投影：同一个语义开关按 provider 的 wire
/// 偏好落到 thinking 或 enable_thinking 字段。
fn apply_thinking_wire(payload: &mut Value, enabled: bool, wire_format: ThinkingWireFormat) {
    match wire_format {
        ThinkingWireFormat::ThinkingType => {
            payload["thinking"] = json!({"type": if enabled { "enabled" } else { "disabled" }});
        }
        ThinkingWireFormat::EnableThinking => {
            payload["enable_thinking"] = json!(enabled);
        }
        ThinkingWireFormat::ReasoningEffort => {
            if !enabled {
                payload["reasoning_effort"] = json!("none");
            }
        }
    }
}

/// 已知兼容字段只取首个非空值，避免同一增量重复显示。
pub(crate) fn chat_reasoning_text(
    message: &serde_json::Map<String, Value>,
) -> Option<(&'static str, &str)> {
    crate::types::CHAT_REASONING_FIELDS
        .iter()
        .find_map(|field| {
            message
                .get(*field)
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
                .map(|text| (*field, text))
        })
}

pub(crate) fn chat_reasoning_detail_text(detail: &Value) -> Option<&str> {
    let key = match detail.get("type").and_then(Value::as_str)? {
        "reasoning.text" => "text",
        "reasoning.summary" => "summary",
        _ => return None,
    };
    detail
        .get(key)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
}

pub fn openai_reasoning_content_present(payload: &Value) -> bool {
    payload
        .pointer("/choices/0/message")
        .and_then(Value::as_object)
        .is_some_and(|message| {
            chat_reasoning_text(message).is_some()
                || message
                    .get("reasoning_details")
                    .and_then(Value::as_array)
                    .is_some_and(|details| !details.is_empty())
        })
}

pub fn parse_openai_response(
    request: &ModelTurnRequest,
    config: &OpenAiProviderConfig,
    payload: Value,
    model_name: &str,
    reasoning_effort: Option<&str>,
) -> Result<ModelTurnResponse, ProviderError> {
    if let Some(error) = payload.get("error").filter(|error| !error.is_null()) {
        return Err(provider_embedded_error(
            &provider_error_fields(error),
            "provider Chat payload contained an error",
            "chat_error_present",
            Some((config.provider_name.as_str(), model_name)),
        ));
    }
    let response_id = payload
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("response")
        .to_string();
    let choices = payload
        .get("choices")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            provider_response_validation_error(
                config,
                model_name,
                "provider response missing choices",
                vec!["response_choices_missing".to_string()],
            )
        })?;
    if choices.is_empty() {
        return Err(provider_response_validation_error(
            config,
            model_name,
            "provider response missing choices",
            vec!["response_choices_missing".to_string()],
        ));
    }
    if choices.len() != 1 {
        return Err(provider_response_validation_error(
            config,
            model_name,
            "provider response must contain exactly one choice",
            vec!["response_choices_count_invalid".to_string()],
        ));
    }
    let choice = &choices[0];
    validate_openai_chat_response_wire(choice).map_err(|validation_error| {
        provider_response_validation_error(
            config,
            model_name,
            "provider Chat response failed wire validation",
            vec![validation_error.to_string()],
        )
    })?;
    let message = choice.get("message").ok_or_else(|| {
        provider_response_validation_error(
            config,
            model_name,
            "provider Chat response message was missing",
            vec!["chat_message_invalid".to_string()],
        )
    })?;
    let content = parse_message_content(
        message.get("content"),
        &[],
        None,
        "chat_content_part_type_invalid",
        "chat_content_part_type_invalid",
        "chat_content_part_type_invalid",
    )
    .map_err(|validation_error| {
        provider_response_validation_error(
            config,
            model_name,
            "provider Chat response content was invalid",
            vec![validation_error.to_string()],
        )
    })?;
    let tool_calls = parse_openai_tool_calls(message);
    let assistant_message = Some(ModelMessage {
        tool_calls: tool_calls.clone(),
        ..ModelMessage::text(ModelRole::Assistant, content)
    });
    let finish_reason = choice
        .get("finish_reason")
        .and_then(Value::as_str)
        .map(str::to_string);
    if finish_reason.as_deref() == Some("content_filter") {
        return Err(provider_content_filter_error(
            config,
            model_name,
            "provider Chat response was stopped by content filter",
        ));
    }
    if finish_reason.as_deref() == Some("network_error") {
        return Err(provider_finish_network_error(
            config,
            model_name,
            "provider Chat response reported a network error",
        ));
    }
    let (reasoning_field, reasoning_content) = message
        .as_object()
        .and_then(chat_reasoning_text)
        .unwrap_or(("reasoning_content", ""));
    let reasoning_details = match message
        .get("reasoning_details")
        .filter(|value| !value.is_null())
    {
        Some(Value::Array(details)) if details.iter().all(Value::is_object) => details.clone(),
        Some(_) => {
            return Err(crate::transport::provider_reasoning_history_error(
                "provider reasoning_details must be an array of objects",
            ));
        }
        None => Vec::new(),
    };
    let thinking = if !reasoning_content.is_empty() {
        reasoning_content.to_string()
    } else {
        reasoning_details
            .iter()
            .filter_map(chat_reasoning_detail_text)
            .collect::<Vec<_>>()
            .join("")
    };
    let replay = if !reasoning_content.is_empty() || !reasoning_details.is_empty() {
        Some(ProviderReasoningReplay::Chat {
            provider_name: config.provider_name.clone(),
            model_name: model_name.to_string(),
            reasoning_effort: reasoning_effort.map(str::to_string),
            tool_call_ids: tool_calls
                .iter()
                .map(|call| call.tool_call_id.clone())
                .collect(),
            reasoning_content: reasoning_content.to_string(),
            reasoning_field: reasoning_field.to_string(),
            reasoning_details,
        })
    } else {
        None
    };
    finalize_provider_response(
        request,
        config,
        model_name,
        ParsedResponseParts {
            response_id,
            assistant_message,
            usage: parse_usage(
                payload.get("usage"),
                "prompt_tokens",
                "completion_tokens",
                "/prompt_tokens_details/cached_tokens",
                "/completion_tokens_details/reasoning_tokens",
            ),
            finish_reason,
        },
    )
    .map(|mut response| {
        response.thinking = thinking;
        if let Some(message) = response.assistant_message.as_mut() {
            message.provider_reasoning_replay = replay;
        }
        response
    })
}

/// 一次 provider 响应的解析产物：完成消息、用量与终止原因。
///
/// 由各协议适配器的 payload 解析组装，供 finalize_provider_response
/// 合成最终 ModelTurnResponse。
pub struct ParsedResponseParts {
    pub response_id: String,
    pub assistant_message: Option<ModelMessage>,
    pub usage: ModelUsage,
    pub finish_reason: Option<String>,
}

pub fn finalize_provider_response(
    request: &ModelTurnRequest,
    config: &OpenAiProviderConfig,
    model_name: &str,
    parsed: ParsedResponseParts,
) -> Result<ModelTurnResponse, ProviderError> {
    let ParsedResponseParts {
        response_id,
        assistant_message,
        usage,
        finish_reason,
    } = parsed;
    let response = ModelTurnResponse {
        request_id: request.request_id.clone(),
        response_id,
        assistant_message,
        usage,
        finish_reason,
        provider_name: Some(config.provider_name.clone()),
        thinking: String::new(),
        model_name: Some(model_name.to_string()),
    };
    let available_tool_names = request
        .tools
        .iter()
        .map(|tool| tool.name.clone())
        .collect::<Vec<_>>();
    let mut validation = validate_model_turn_response(request, &response);
    // 通用模型契约中未知名只是警告，调用方可报告且不丢失响应其余部分；
    // 但 OpenAI 适配器是原生工具信任边界：未注册名（或缺失调用身份）绝不
    // 能进入 AgentLoop 的参数修复路径。
    let unknown_tool = response.tool_calls().iter().any(|call| {
        !call.tool_name.trim().is_empty()
            && !available_tool_names
                .iter()
                .any(|tool_name| tool_name == &call.tool_name)
    });
    let invalid_tool_identity = response
        .tool_calls()
        .iter()
        .any(|call| call.tool_call_id.trim().is_empty() || call.tool_name.trim().is_empty());
    if unknown_tool
        && !validation
            .errors
            .iter()
            .any(|error| error == "unknown_tool")
    {
        validation.errors.push("unknown_tool".to_string());
        validation.errors.sort();
        validation.errors.dedup();
        validation.valid = false;
    }
    let invalid_tool_call = unknown_tool || invalid_tool_identity;
    if invalid_tool_call && validation.valid {
        validation.valid = false;
    }
    // 不可恢复的响应校验失败在本边界直接类型化失败（与请求校验同路径）；
    // 可恢复的畸形工具参数保持 Success，交由 AgentLoop 的工具派发产出
    // 模型可见的校验结果。
    if !validation.valid && !recoverable_tool_argument_validation(&response, &validation.errors) {
        return Err(provider_response_validation_error(
            config,
            model_name,
            &format!("provider_response_invalid: {}", validation.errors.join(",")),
            validation.errors,
        ));
    }
    Ok(response)
}

/// 只有已注册、身份完整的原生调用的畸形参数可以继续进入 AgentLoop 取得
/// 类型化校验结果；其余响应校验错误在本边界保持为 provider 失败。
fn recoverable_tool_argument_validation(
    response: &ModelTurnResponse,
    validation_errors: &[String],
) -> bool {
    !response.tool_calls().is_empty()
        && !validation_errors.is_empty()
        && validation_errors
            .iter()
            .all(|error| is_recoverable_tool_argument_error(error))
        && response.tool_calls().iter().all(|call| {
            !call.tool_call_id.trim().is_empty()
                && !call.tool_name.trim().is_empty()
                && call
                    .validation_errors
                    .iter()
                    .all(|error| is_recoverable_tool_argument_error(error))
        })
}

fn is_recoverable_tool_argument_error(error: &str) -> bool {
    matches!(
        error,
        "invalid_json" | "schema_mismatch" | "tool_call_arguments_must_be_object"
    )
}

pub fn parse_openai_tool_calls(message: &Value) -> Vec<ModelToolCall> {
    message
        .get("tool_calls")
        .and_then(Value::as_array)
        .map(|calls| {
            calls
                .iter()
                .map(|call| {
                    parse_tool_call(
                        call,
                        "id",
                        call.pointer("/function/name"),
                        call.pointer("/function/arguments"),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

fn validate_openai_chat_response_wire(choice: &Value) -> Result<(), &'static str> {
    let choice = choice.as_object().ok_or("chat_message_invalid")?;
    let message = choice
        .get("message")
        .and_then(Value::as_object)
        .ok_or("chat_message_invalid")?;
    if message.get("role").and_then(Value::as_str) != Some("assistant") {
        return Err("chat_message_role_invalid");
    }

    if let Some(content) = message.get("content") {
        match content {
            Value::String(_) | Value::Null => {}
            Value::Array(parts) => {
                for part in parts {
                    let part = part.as_object().ok_or("chat_content_part_type_invalid")?;
                    match part.get("type").and_then(Value::as_str) {
                        Some("text") if part.get("text").and_then(Value::as_str).is_some() => {}
                        Some("refusal")
                            if part.get("refusal").and_then(Value::as_str).is_some() => {}
                        _ => return Err("chat_content_part_type_invalid"),
                    }
                }
            }
            _ => return Err("chat_content_part_type_invalid"),
        }
    }

    if let Some(tool_calls) = message.get("tool_calls") {
        match tool_calls {
            Value::Null => {}
            Value::Array(calls) => {
                for call in calls {
                    let call = call.as_object().ok_or("chat_tool_call_type_invalid")?;
                    if call.get("type").and_then(Value::as_str) != Some("function") {
                        return Err("chat_tool_call_type_invalid");
                    }
                }
            }
            _ => return Err("chat_tool_call_type_invalid"),
        }
    }

    Ok(())
}

/// 按字段名参数化构建一次工具调用：id_field 为调用 id 字段名，
/// name/arguments 为已定位的取值（chat 在 function 子对象内，
/// responses 在顶层）。与 parse_tool_arguments 同属共享解析族。
pub(crate) fn parse_tool_call(
    call: &Value,
    id_field: &str,
    name: Option<&Value>,
    arguments: Option<&Value>,
) -> ModelToolCall {
    let (arguments, raw_arguments, parse_status, validation_errors) =
        parse_tool_call_arguments(arguments);
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
        parse_status,
        validation_errors,
    }
}

pub fn parse_tool_call_arguments(
    arguments_value: Option<&Value>,
) -> (Value, String, ModelToolParseStatus, Vec<String>) {
    let Some(arguments_value) = arguments_value else {
        return (
            json!({}),
            String::new(),
            ModelToolParseStatus::SchemaMismatch,
            vec!["tool_call_arguments_missing".to_string()],
        );
    };
    match arguments_value {
        Value::String(raw_arguments) => {
            let (arguments, parse_status, validation_errors) = parse_tool_arguments(raw_arguments);
            (
                arguments,
                raw_arguments.clone(),
                parse_status,
                validation_errors,
            )
        }
        Value::Object(_) => (
            arguments_value.clone(),
            serde_json::to_string(arguments_value).unwrap_or_default(),
            ModelToolParseStatus::Valid,
            Vec::new(),
        ),
        _ => (
            json!({}),
            String::new(),
            ModelToolParseStatus::SchemaMismatch,
            vec!["tool_call_arguments_type_invalid".to_string()],
        ),
    }
}

pub fn parse_tool_arguments(raw_arguments: &str) -> (Value, ModelToolParseStatus, Vec<String>) {
    match serde_json::from_str::<Value>(raw_arguments) {
        Ok(arguments) if arguments.is_object() => {
            (arguments, ModelToolParseStatus::Valid, Vec::new())
        }
        Ok(arguments) => (
            arguments,
            ModelToolParseStatus::SchemaMismatch,
            vec!["tool_call_arguments_must_be_object".to_string()],
        ),
        Err(_) => (
            json!({}),
            ModelToolParseStatus::InvalidJson,
            vec!["invalid_json".to_string()],
        ),
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

fn openai_message_payload_with_reasoning(
    message: &ModelMessage,
    supports_developer_role: bool,
    requires_assistant_content_for_tool_calls: bool,
) -> Value {
    let role = serde_json::to_value(&message.role)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_else(|| "user".to_string());
    let role = if message.role == ModelRole::Developer && !supports_developer_role {
        "system".to_string()
    } else {
        role
    };
    let mut content = openai_message_content(message);
    if message.role == ModelRole::Assistant
        && !message.tool_calls.is_empty()
        && requires_assistant_content_for_tool_calls
        && content.is_null()
    {
        content = json!("");
    }
    let mut payload = json!({
        "role": role,
        "content": content,
    });
    if let Some(tool_call_id) = &message.tool_call_id {
        payload["tool_call_id"] = json!(tool_call_id);
    }
    if !message.tool_calls.is_empty() {
        payload["tool_calls"] = json!(
            message
                .tool_calls
                .iter()
                .map(openai_tool_call_payload)
                .collect::<Vec<_>>()
        );
    }
    if let Some(ProviderReasoningReplay::Chat {
        reasoning_content,
        reasoning_field,
        reasoning_details,
        ..
    }) = message.provider_reasoning_replay.as_ref()
    {
        if !reasoning_details.is_empty() {
            payload["reasoning_details"] = json!(reasoning_details);
        } else if !reasoning_content.is_empty() {
            payload[reasoning_field] = json!(reasoning_content);
        }
    }
    payload
}

pub fn openai_message_content(message: &ModelMessage) -> Value {
    let text = message_text(message);
    if message.role == ModelRole::Assistant && !message.tool_calls.is_empty() && text.is_empty() {
        Value::Null
    } else {
        json!(text)
    }
}

pub fn openai_tool_call_payload(tool_call: &ModelToolCall) -> Value {
    json!({
        "id": tool_call.tool_call_id,
        "type": "function",
        "function": {
            "name": tool_call.tool_name,
            "arguments": tool_call.raw_arguments,
        }
    })
}

pub fn openai_tool_payload(tool: &ModelToolSchema) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": tool.name,
            "description": tool.description,
            "parameters": tool.parameters_schema,
        }
    })
}
