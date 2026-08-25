//! OpenAI Chat Completions 协议请求序列化与响应解析。

use serde_json::{Value, json};

use crate::error::{ModelError, ModelErrorKind, ProviderError, ProviderErrorStage};
use crate::provider::contract::{
    ProviderProtocolContract, ThinkingWireFormat, message_text, provider_content_filter_error,
    provider_response_validation_error, request_uses_tool_protocol, validate_model_turn_response,
};
use crate::provider::runtime::OpenAiProviderConfig;
use crate::types::{
    ModelMessage, ModelRole, ModelToolCall, ModelToolParseStatus, ModelToolSchema,
    ModelTurnRequest, ModelTurnResponse, ModelTurnStatus, ModelUsage, ProviderReasoningReplay,
    ProviderToolReasoningMode,
};

#[allow(clippy::too_many_arguments)]
pub fn openai_request_payload(
    request: &ModelTurnRequest,
    model_name: &str,
    capabilities: &ProviderProtocolContract,
    reasoning_enabled: bool,
    reasoning_disabled: bool,
    wire_reasoning_effort: Option<&str>,
    thinking_wire_format: ThinkingWireFormat,
    supports_developer_role: bool,
    supports_tool_choice: bool,
    requires_assistant_content_for_tool_calls: bool,
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
                    &request.provider_reasoning_history,
                    supports_developer_role,
                    requires_assistant_content_for_tool_calls,
                )
            })
            .collect::<Vec<_>>(),
        "stream": false,
    });
    // 输出上限 wire 字段取舍：chat completions 走 `max_tokens`（第三方兼容
    // 端点如 DeepSeek/dashscope 接受），responses 走 `max_output_tokens`
    // （OpenAI 官方 Responses API 命名）。官方 chat 对推理系模型要求
    // `max_completion_tokens`，本层不针对推理模型切换字段；推理模型经
    // chat 兼容端点使用时若需输出上限，由用户在配置中显式声明。
    if let Some(max_output_tokens) = request.model_preferences.max_output_tokens {
        payload["max_tokens"] = json!(max_output_tokens);
    }
    if reasoning_enabled {
        match thinking_wire_format {
            ThinkingWireFormat::ThinkingType => {
                payload["thinking"] = json!({"type": "enabled"});
            }
            ThinkingWireFormat::EnableThinking => {
                payload["enable_thinking"] = json!(true);
            }
        }
        if let Some(wire_effort) = wire_reasoning_effort {
            payload["reasoning_effort"] = json!(wire_effort);
        }
    } else if reasoning_disabled {
        match thinking_wire_format {
            ThinkingWireFormat::ThinkingType => {
                payload["thinking"] = json!({"type": "disabled"});
            }
            ThinkingWireFormat::EnableThinking => {
                payload["enable_thinking"] = json!(false);
            }
        }
    }
    if !request.tools.is_empty() {
        payload["tools"] = json!(
            request
                .tools
                .iter()
                .map(|tool| openai_tool_payload(tool, request.tool_choice.strict_tool_schema))
                .collect::<Vec<_>>()
        );
        if supports_tool_choice {
            payload["tool_choice"] = super::tool_choice_payload();
            // 诚实信号：本地按模型给定顺序串行执行全部工具调用，不请求并行。
            payload["parallel_tool_calls"] = json!(false);
        }
    }
    if request_uses_tool_protocol(request)
        && capabilities.tool_reasoning_mode == ProviderToolReasoningMode::DisabledForToolCalls
    {
        match thinking_wire_format {
            ThinkingWireFormat::ThinkingType => {
                payload["thinking"] = json!({"type": "disabled"});
            }
            ThinkingWireFormat::EnableThinking => {
                payload["enable_thinking"] = json!(false);
            }
        }
    }
    payload
}

#[allow(clippy::too_many_arguments)]
pub fn openai_chat_stream_request_payload(
    request: &ModelTurnRequest,
    model_name: &str,
    capabilities: &ProviderProtocolContract,
    reasoning_enabled: bool,
    reasoning_disabled: bool,
    wire_reasoning_effort: Option<&str>,
    thinking_wire_format: ThinkingWireFormat,
    supports_developer_role: bool,
    supports_tool_choice: bool,
    requires_assistant_content_for_tool_calls: bool,
) -> Value {
    let mut payload = openai_request_payload(
        request,
        model_name,
        capabilities,
        reasoning_enabled,
        reasoning_disabled,
        wire_reasoning_effort,
        thinking_wire_format,
        supports_developer_role,
        supports_tool_choice,
        requires_assistant_content_for_tool_calls,
    );
    payload["stream"] = json!(true);
    // Request usage in the final stream chunk when the provider implements
    // the OpenAI-compatible include_usage extension. Providers that omit it
    // still produce a valid response with usage_present=false.
    payload["stream_options"] = json!({"include_usage": true});
    payload
}

pub fn openai_reasoning_content_present(payload: &Value) -> bool {
    match payload.pointer("/choices/0/message/reasoning_content") {
        Some(Value::String(content)) => !content.is_empty(),
        Some(value) => !value.is_null(),
        None => false,
    }
}

pub fn parse_openai_response(
    request: &ModelTurnRequest,
    config: &OpenAiProviderConfig,
    payload: Value,
    capabilities: &ProviderProtocolContract,
    model_name: &str,
    reasoning_effort: Option<&str>,
) -> Result<ModelTurnResponse, ProviderError> {
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
    let content = parse_openai_content(message.get("content")).map_err(|validation_error| {
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
    let provider_reasoning_history = message
        .get("reasoning_content")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .filter(|_| !tool_calls.is_empty())
        .map(|reasoning_content| {
            vec![ProviderReasoningReplay::Chat {
                provider_name: config.provider_name.clone(),
                model_name: model_name.to_string(),
                // 绑定请求时实际 selection 的 reasoning 变体；provider 不回显
                // effort 时保持 None，不伪造禁用变体。
                reasoning_effort: reasoning_effort.map(str::to_string),
                tool_call_ids: tool_calls
                    .iter()
                    .map(|call| call.tool_call_id.clone())
                    .collect(),
                reasoning_content: reasoning_content.to_string(),
            }]
        })
        .unwrap_or_default();
    finalize_provider_response(
        request,
        config,
        model_name,
        capabilities,
        response_id,
        assistant_message,
        tool_calls,
        parse_openai_usage(payload.get("usage")),
        finish_reason,
    )
    .map(|mut response| {
        response.provider_reasoning_history = provider_reasoning_history;
        response
    })
}

#[allow(clippy::too_many_arguments)]
pub fn finalize_provider_response(
    request: &ModelTurnRequest,
    config: &OpenAiProviderConfig,
    model_name: &str,
    capabilities: &ProviderProtocolContract,
    response_id: String,
    assistant_message: Option<ModelMessage>,
    tool_calls: Vec<ModelToolCall>,
    usage: ModelUsage,
    finish_reason: Option<String>,
) -> Result<ModelTurnResponse, ProviderError> {
    let mut response = ModelTurnResponse {
        request_id: request.request_id.clone(),
        response_id,
        status: ModelTurnStatus::Success,
        assistant_message,
        tool_calls,
        usage,
        finish_reason,
        validation: None,
        error: None,
        provider_name: Some(config.provider_name.clone()),
        model_name: Some(model_name.to_string()),
        provider_reasoning_history: Vec::new(),
    };
    let available_tool_names = request
        .tools
        .iter()
        .map(|tool| tool.name.clone())
        .collect::<Vec<_>>();
    let validation = validate_model_turn_response(
        request,
        &response,
        &available_tool_names,
        Some(capabilities),
    );
    let mut validation = validation;
    // Unknown names are warnings in the generic model contract so callers can
    // report them without losing the rest of the response. The OpenAI adapter
    // is the native tool trust boundary, however: an unregistered name (or a
    // missing call identity) must never enter AgentLoop's argument-repair path.
    let unknown_tool = response.tool_calls.iter().any(|call| {
        call.parse_status == ModelToolParseStatus::UnknownTool
            || (!call.tool_name.trim().is_empty()
                && !available_tool_names
                    .iter()
                    .any(|tool_name| tool_name == &call.tool_name))
    });
    let invalid_tool_identity = response
        .tool_calls
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
    if !validation.valid && !recoverable_tool_argument_validation(&response, &validation.errors) {
        response.status = ModelTurnStatus::Invalid;
        let (kind, message, diagnostic_code) = (
            ModelErrorKind::JsonSchemaViolation,
            format!("provider_response_invalid: {}", validation.errors.join(",")),
            "provider_response_invalid",
        );
        response.error = Some(
            ModelError::new(kind, message)
                .with_provider(config.provider_name.clone())
                .with_model(model_name.to_string())
                .with_provider_diagnostic(diagnostic_code, ProviderErrorStage::ResponseValidation),
        );
        if let Some(error) = response.error.as_mut() {
            error.validation_errors = validation.errors.clone();
        }
    }
    response.validation = Some(validation);
    Ok(response)
}

/// Only malformed arguments for a registered, fully identified native call may
/// continue to `AgentLoop` for a typed validation result. Every other response
/// validation error remains a provider failure at this boundary.
fn recoverable_tool_argument_validation(
    response: &ModelTurnResponse,
    validation_errors: &[String],
) -> bool {
    !response.tool_calls.is_empty()
        && !validation_errors.is_empty()
        && validation_errors
            .iter()
            .all(|error| is_recoverable_tool_argument_error(error))
        && response.tool_calls.iter().all(|call| {
            !call.tool_call_id.trim().is_empty()
                && !call.tool_name.trim().is_empty()
                && call.parse_status != ModelToolParseStatus::UnknownTool
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
                .enumerate()
                .map(|(index, call)| parse_openai_tool_call(index, call))
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

pub fn parse_openai_tool_call(_index: usize, call: &Value) -> ModelToolCall {
    let function = call.get("function").unwrap_or(&Value::Null);
    let (arguments, raw_arguments, parse_status, validation_errors) =
        parse_tool_call_arguments(function.get("arguments"));
    let wire_tool_name = function.get("name").and_then(Value::as_str).unwrap_or("");
    ModelToolCall {
        tool_call_id: call
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_default(),
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

pub fn parse_openai_content(content: Option<&Value>) -> Result<String, &'static str> {
    match content {
        None | Some(Value::Null) => Ok(String::new()),
        Some(Value::String(text)) => Ok(text.clone()),
        Some(Value::Array(parts)) => {
            let mut content = String::new();
            for part in parts {
                let part = part.as_object().ok_or("chat_content_part_type_invalid")?;
                match part.get("type").and_then(Value::as_str) {
                    Some("text") => content.push_str(
                        part.get("text")
                            .and_then(Value::as_str)
                            .ok_or("chat_content_part_type_invalid")?,
                    ),
                    Some("refusal") => content.push_str(
                        part.get("refusal")
                            .and_then(Value::as_str)
                            .ok_or("chat_content_part_type_invalid")?,
                    ),
                    _ => return Err("chat_content_part_type_invalid"),
                }
            }
            Ok(content)
        }
        Some(_) => Err("chat_content_part_type_invalid"),
    }
}

pub fn parse_openai_usage(usage: Option<&Value>) -> ModelUsage {
    let Some(usage) = usage else {
        return ModelUsage::default();
    };
    ModelUsage {
        input_tokens: usage
            .get("prompt_tokens")
            .and_then(Value::as_u64)
            .unwrap_or_default(),
        output_tokens: usage
            .get("completion_tokens")
            .and_then(Value::as_u64)
            .unwrap_or_default(),
        total_tokens: usage
            .get("total_tokens")
            .and_then(Value::as_u64)
            .unwrap_or_default(),
        cached_input_tokens: usage
            .pointer("/prompt_tokens_details/cached_tokens")
            .and_then(Value::as_u64)
            .unwrap_or_default(),
        reasoning_tokens: usage
            .pointer("/completion_tokens_details/reasoning_tokens")
            .and_then(Value::as_u64)
            .unwrap_or_default(),
        usage_present: true,
    }
}

fn openai_message_payload_with_reasoning(
    message: &ModelMessage,
    reasoning_history: &[ProviderReasoningReplay],
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
    if message.role == ModelRole::Assistant && !message.tool_calls.is_empty() {
        let call_ids = message
            .tool_calls
            .iter()
            .map(|call| call.tool_call_id.clone())
            .collect::<Vec<_>>();
        if let Some(ProviderReasoningReplay::Chat {
            reasoning_content, ..
        }) = reasoning_history
            .iter()
            .find(|replay| replay.matches_tool_call_ids(&call_ids))
        {
            payload["reasoning_content"] = json!(reasoning_content);
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

pub fn openai_tool_payload(tool: &ModelToolSchema, strict_tool_schema: bool) -> Value {
    let mut payload = json!({
        "type": "function",
        "function": {
            "name": tool.name,
            "description": tool.description,
            "parameters": tool.parameters_schema,
        }
    });
    if strict_tool_schema {
        payload["function"]["strict"] = json!(true);
    }
    payload
}

#[cfg(test)]
mod replay_binding_tests {
    use super::*;
    use crate::ProviderConfigSource;
    use crate::types::ProviderToolReasoningMode;

    fn replay_test_config() -> OpenAiProviderConfig {
        OpenAiProviderConfig {
            provider_name: "openai_compatible".to_string(),
            model_name: "test-model".to_string(),
            base_url: "http://127.0.0.1:1/v1".to_string(),
            api_key: "test-key-placeholder".to_string(),
            source: ProviderConfigSource::ProcessEnvironment,
            max_context_tokens: Some(crate::DEFAULT_MAX_CONTEXT_TOKENS),
            max_output_tokens: crate::DEFAULT_MAX_OUTPUT_TOKENS,
        }
    }

    fn reasoning_tool_call_payload() -> Value {
        json!({
            "id": "chat_reasoning_no_variant",
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "",
                    "reasoning_content": "opaque chain of thought",
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {"name": "read", "arguments": "{}"}
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5}
        })
    }

    fn replay_test_request() -> ModelTurnRequest {
        let mut request = ModelTurnRequest::new(
            "request_replay_binding",
            vec![ModelMessage::text(ModelRole::User, "hello")],
        );
        request.tools.push(ModelToolSchema {
            name: "read".to_string(),
            description: "Read a file".to_string(),
            parameters_schema: json!({"type": "object"}),
        });
        request
    }

    /// provider 返回 reasoning_content + tool calls 且不回显 effort、请求时
    /// selection 无变体 → replay 绑定 `None`，不伪造 `"off"`；绑定对无变体
    /// 选择兼容，对带变体选择拒绝。
    #[test]
    fn chat_replay_binds_selection_none_when_provider_omits_effort() {
        let response = parse_openai_response(
            &replay_test_request(),
            &replay_test_config(),
            reasoning_tool_call_payload(),
            &ProviderProtocolContract::default(),
            "test-model",
            None,
        )
        .expect("parse response with reasoning_content");
        assert_eq!(response.status, crate::types::ModelTurnStatus::Success);
        assert_eq!(response.provider_reasoning_history.len(), 1);
        let replay = &response.provider_reasoning_history[0];
        match replay {
            ProviderReasoningReplay::Chat {
                reasoning_effort,
                tool_call_ids,
                reasoning_content,
                ..
            } => {
                assert_eq!(reasoning_effort, &None);
                assert_eq!(tool_call_ids, &vec!["call_1".to_string()]);
                assert_eq!(reasoning_content, "opaque chain of thought");
            }
            other => panic!("expected Chat replay, got {other:?}"),
        }
        assert!(replay.is_valid());
        assert!(replay.is_compatible_with(
            "openai_compatible",
            "test-model",
            None,
            ProviderToolReasoningMode::ReplayReasoningContent
        ));
        assert!(!replay.is_compatible_with(
            "openai_compatible",
            "test-model",
            Some("high"),
            ProviderToolReasoningMode::ReplayReasoningContent
        ));
    }
}
