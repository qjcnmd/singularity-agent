//! OpenAI Chat Completions/Responses 的请求投影、响应解码和 envelope 校验。

use super::contract::{
    message_text, provider_response_validation_error, request_uses_tool_protocol,
};
use super::*;

/// 将基础 URL 解析为兼容 OpenAI 的 Chat Completions 端点。
pub fn chat_completions_endpoint(base_url: &str) -> String {
    let trimmed = base_url.trim().trim_end_matches('/');
    if trimmed.ends_with(CHAT_COMPLETIONS_PATH) {
        trimmed.to_string()
    } else if trimmed.ends_with("/v1") {
        format!("{trimmed}{CHAT_COMPLETIONS_PATH}")
    } else {
        format!("{trimmed}{V1_CHAT_COMPLETIONS_PATH}")
    }
}

/// 将基础 URL 解析为兼容 OpenAI 的 Responses 端点。
pub fn responses_endpoint(base_url: &str) -> String {
    let trimmed = base_url.trim().trim_end_matches('/');
    if trimmed.ends_with(RESPONSES_PATH) {
        trimmed.to_string()
    } else if let Some(prefix) = trimmed.strip_suffix(CHAT_COMPLETIONS_PATH) {
        format!("{prefix}{RESPONSES_PATH}")
    } else if trimmed.ends_with("/v1") {
        format!("{trimmed}{RESPONSES_PATH}")
    } else {
        format!("{trimmed}{V1_RESPONSES_PATH}")
    }
}

/// 将模型提供方失败转换为 `AgentLoop` 使用的失败响应结构。
pub fn provider_error_response(
    request: &ModelTurnRequest,
    error: ProviderError,
) -> ModelTurnResponse {
    let provider_attempt_metadata = error.provider_attempt_metadata.clone();
    ModelTurnResponse {
        request_id: request.request_id.clone(),
        response_id: format!("{}_provider_error", request.request_id),
        status: ModelTurnStatus::Failed,
        assistant_message: None,
        tool_calls: Vec::new(),
        usage: ModelUsage::default(),
        finish_reason: None,
        validation: None,
        error: Some(*error.error),
        provider_name: None,
        model_name: request.model_preferences.model_name.clone(),
        provider_attempt_metadata,
    }
}

pub(super) fn openai_request_payload(
    request: &ModelTurnRequest,
    model_name: &str,
    capabilities: &ProviderProtocolContract,
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
            .map(openai_message_payload)
            .collect::<Vec<_>>(),
        "stream": false,
    });
    if let Some(max_output_tokens) = request.model_preferences.max_output_tokens {
        payload["max_tokens"] = json!(max_output_tokens);
    }
    if let Some(temperature) = request.model_preferences.temperature {
        payload["temperature"] = json!(temperature);
    }
    if let Some(top_p) = request.model_preferences.top_p {
        payload["top_p"] = json!(top_p);
    }
    if request.model_preferences.json_mode {
        payload["response_format"] = json!({"type": "json_object"});
    }
    if !request.tools.is_empty() {
        payload["tools"] = json!(
            request
                .tools
                .iter()
                .map(|tool| openai_tool_payload(tool, request.tool_choice.strict_tool_schema))
                .collect::<Vec<_>>()
        );
        payload["tool_choice"] = openai_tool_choice_payload(request);
        payload["parallel_tool_calls"] = json!(request.tool_choice.max_tool_calls > 1);
    }
    if request_uses_tool_protocol(request)
        && capabilities.tool_reasoning_mode == ProviderToolReasoningMode::DisabledForToolCalls
    {
        payload["thinking"] = json!({"type": "disabled"});
    }
    payload
}

pub(super) fn openai_responses_request_payload(
    request: &ModelTurnRequest,
    model_name: &str,
    capabilities: &ProviderProtocolContract,
) -> Value {
    let (instructions, input) = openai_responses_input(&request.messages);
    let mut payload = json!({
        "model": request
            .model_preferences
            .model_name
            .as_deref()
            .unwrap_or(model_name),
        "input": input,
        "stream": false,
        "store": false,
    });
    if let Some(instructions) = instructions {
        payload["instructions"] = json!(instructions);
    }
    if let Some(max_output_tokens) = request.model_preferences.max_output_tokens {
        payload["max_output_tokens"] = json!(max_output_tokens);
    }
    if let Some(temperature) = request.model_preferences.temperature {
        payload["temperature"] = json!(temperature);
    }
    if let Some(top_p) = request.model_preferences.top_p {
        payload["top_p"] = json!(top_p);
    }
    if request.model_preferences.json_mode {
        payload["text"] = json!({"format": {"type": "json_object"}});
    }
    if !request.tools.is_empty() {
        payload["tools"] = json!(
            request
                .tools
                .iter()
                .map(|tool| {
                    json!({
                        "type": "function",
                        "name": tool.name,
                        "description": tool.description,
                        "parameters": tool.parameters_schema,
                        "strict": request.tool_choice.strict_tool_schema,
                    })
                })
                .collect::<Vec<_>>()
        );
        payload["tool_choice"] = openai_responses_tool_choice_payload(request);
        payload["parallel_tool_calls"] = json!(request.tool_choice.max_tool_calls > 1);
    }
    if request_uses_tool_protocol(request)
        && capabilities.tool_reasoning_mode == ProviderToolReasoningMode::DisabledForToolCalls
    {
        payload["reasoning"] = json!({"effort": "none"});
    }
    payload
}

pub(super) fn openai_responses_stream_request_payload(
    request: &ModelTurnRequest,
    model_name: &str,
    capabilities: &ProviderProtocolContract,
) -> Value {
    let mut payload = openai_responses_request_payload(request, model_name, capabilities);
    payload["stream"] = json!(true);
    payload
}

pub(super) fn openai_responses_input(messages: &[ModelMessage]) -> (Option<String>, Vec<Value>) {
    let instruction_count = messages
        .iter()
        .take_while(|message| matches!(message.role, ModelRole::System | ModelRole::Developer))
        .count();
    let instructions = messages[..instruction_count]
        .iter()
        .map(message_text)
        .filter(|message| !message.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    let mut items = Vec::new();
    for message in &messages[instruction_count..] {
        match message.role {
            ModelRole::Tool => {
                items.push(json!({
                    "type": "function_call_output",
                    "call_id": message.tool_call_id,
                    "output": message_text(message),
                }));
            }
            ModelRole::Assistant => {
                if !message_text(message).is_empty() {
                    items.push(json!({
                        "type": "message",
                        "role": "assistant",
                        "content": message_text(message),
                    }));
                }
                items.extend(message.tool_calls.iter().map(|call| {
                    json!({
                        "type": "function_call",
                        "call_id": call.tool_call_id,
                        "name": call.tool_name,
                        "arguments": call.raw_arguments,
                    })
                }));
            }
            ModelRole::System | ModelRole::Developer | ModelRole::User => {
                let role = match message.role {
                    ModelRole::System => "system",
                    ModelRole::Developer => "developer",
                    ModelRole::User => "user",
                    ModelRole::Assistant | ModelRole::Tool => unreachable!(),
                };
                items.push(json!({
                    "type": "message",
                    "role": role,
                    "content": message_text(message),
                }));
            }
        }
    }
    ((!instructions.is_empty()).then_some(instructions), items)
}

pub(super) fn openai_responses_tool_choice_payload(request: &ModelTurnRequest) -> Value {
    match request.tool_choice.mode {
        ToolChoiceMode::None => json!("none"),
        ToolChoiceMode::Required => json!("required"),
        ToolChoiceMode::Auto => json!("auto"),
    }
}

pub(super) fn openai_message_payload(message: &ModelMessage) -> Value {
    let role = serde_json::to_value(&message.role)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_else(|| "user".to_string());
    let mut payload = json!({
        "role": role,
        "content": openai_message_content(message),
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
    payload
}

pub(super) fn openai_message_content(message: &ModelMessage) -> Value {
    let text = message_text(message);
    if message.role == ModelRole::Assistant && !message.tool_calls.is_empty() && text.is_empty() {
        Value::Null
    } else {
        json!(text)
    }
}

pub(super) fn openai_tool_call_payload(tool_call: &ModelToolCall) -> Value {
    json!({
        "id": tool_call.tool_call_id,
        "type": "function",
        "function": {
            "name": tool_call.tool_name,
            "arguments": tool_call.raw_arguments,
        }
    })
}

pub(super) fn openai_tool_payload(tool: &ModelToolSchema, strict_tool_schema: bool) -> Value {
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

pub(super) fn openai_tool_choice_payload(request: &ModelTurnRequest) -> Value {
    match request.tool_choice.mode {
        ToolChoiceMode::None => json!("none"),
        ToolChoiceMode::Required => json!("required"),
        ToolChoiceMode::Auto => json!("auto"),
    }
}

pub(super) struct OpenAiCompletion {
    pub(super) response: ModelTurnResponse,
    pub(super) reasoning_content_present: bool,
}

pub(super) fn openai_reasoning_content_present(payload: &Value) -> bool {
    match payload.pointer("/choices/0/message/reasoning_content") {
        Some(Value::String(content)) => !content.is_empty(),
        Some(value) => !value.is_null(),
        None => false,
    }
}

pub(super) fn openai_responses_reasoning_content_present(payload: &Value) -> bool {
    payload
        .get("output")
        .and_then(Value::as_array)
        .is_some_and(|items| {
            items.iter().any(|item| {
                item.get("type").and_then(Value::as_str) == Some("reasoning")
                    && ["content", "summary", "encrypted_content", "text"]
                        .iter()
                        .filter_map(|field| item.get(*field))
                        .any(value_has_content)
            })
        })
}

pub(super) fn value_has_content(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::String(value) => !value.is_empty(),
        Value::Array(value) => !value.is_empty(),
        Value::Object(value) => !value.is_empty(),
        Value::Bool(_) | Value::Number(_) => true,
    }
}

pub(super) fn parse_openai_responses_response(
    request: &ModelTurnRequest,
    config: &OpenAiProviderConfig,
    payload: Value,
    capabilities: &ProviderProtocolContract,
    model_name: &str,
) -> Result<ModelTurnResponse, ProviderError> {
    if payload.get("error").is_some_and(|error| !error.is_null()) {
        return Err(provider_response_validation_error(
            config,
            model_name,
            "provider Responses payload contained an error",
            vec!["responses_error_present".to_string()],
        ));
    }
    let status = payload.get("status").and_then(Value::as_str);
    if status != Some("completed") {
        return Err(provider_response_validation_error(
            config,
            model_name,
            "provider Responses payload was not completed",
            vec!["responses_status_not_completed".to_string()],
        ));
    }
    let output = payload
        .get("output")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            provider_response_validation_error(
                config,
                model_name,
                "provider Responses payload missing output items",
                vec!["responses_output_missing".to_string()],
            )
        })?;
    let mut content = String::new();
    let mut tool_calls = Vec::new();
    for (index, item) in output.iter().enumerate() {
        match item.get("type").and_then(Value::as_str) {
            Some("message") => {
                let message = parse_openai_responses_message(item).map_err(|evidence| {
                    provider_response_validation_error(
                        config,
                        model_name,
                        "provider Responses message content was invalid",
                        vec![evidence.to_string()],
                    )
                })?;
                content.push_str(&message);
            }
            Some("function_call") => tool_calls.push(parse_openai_responses_tool_call(index, item)),
            Some("reasoning") if !openai_responses_reasoning_item_has_content(item) => {}
            Some("reasoning") => {
                return Err(provider_response_validation_error(
                    config,
                    model_name,
                    "provider returned reasoning content that cannot be replayed safely",
                    vec!["responses_reasoning_content_present".to_string()],
                ));
            }
            _ => {
                return Err(provider_response_validation_error(
                    config,
                    model_name,
                    "provider Responses payload contained an unsupported output item",
                    vec!["responses_output_item_unsupported".to_string()],
                ));
            }
        }
    }
    let assistant_message = Some(ModelMessage {
        tool_calls: tool_calls.clone(),
        ..ModelMessage::text(ModelRole::Assistant, content)
    });
    finalize_provider_response(
        request,
        config,
        model_name,
        capabilities,
        payload
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("response")
            .to_string(),
        assistant_message,
        tool_calls,
        parse_openai_responses_usage(payload.get("usage")),
        status.map(str::to_string),
    )
}

pub(super) fn openai_responses_reasoning_item_has_content(item: &Value) -> bool {
    ["content", "summary", "encrypted_content", "text"]
        .iter()
        .filter_map(|field| item.get(*field))
        .any(value_has_content)
}

pub(super) fn parse_openai_responses_message(message: &Value) -> Result<String, &'static str> {
    match message.get("content") {
        Some(Value::String(text)) => Ok(text.clone()),
        Some(Value::Array(parts)) => {
            let mut content = String::new();
            for part in parts {
                let text = match part.get("type").and_then(Value::as_str) {
                    Some("output_text") | Some("text") => part.get("text").and_then(Value::as_str),
                    Some("refusal") => part.get("refusal").and_then(Value::as_str),
                    _ => return Err("responses_message_content_part_unsupported"),
                }
                .ok_or("responses_message_content_text_missing")?;
                content.push_str(text);
            }
            Ok(content)
        }
        None | Some(Value::Null) => Err("responses_message_content_missing"),
        Some(_) => Err("responses_message_content_invalid"),
    }
}

pub(super) fn parse_openai_responses_tool_call(_index: usize, call: &Value) -> ModelToolCall {
    let arguments_value = call.get("arguments").unwrap_or(&Value::Null);
    let raw_arguments = match arguments_value {
        Value::String(raw) => raw.clone(),
        Value::Object(_) => serde_json::to_string(arguments_value).unwrap_or_default(),
        _ => String::new(),
    };
    let (arguments, parse_status, validation_errors) = if arguments_value.is_object() {
        (
            arguments_value.clone(),
            ModelToolParseStatus::Valid,
            Vec::new(),
        )
    } else {
        parse_tool_arguments(&raw_arguments)
    };
    ModelToolCall {
        tool_call_id: call
            .get("call_id")
            .or_else(|| call.get("id"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        tool_name: call
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        arguments,
        raw_arguments,
        parse_status,
        validation_errors,
    }
}

pub(super) fn parse_openai_responses_usage(usage: Option<&Value>) -> ModelUsage {
    let Some(usage) = usage else {
        return ModelUsage::default();
    };
    ModelUsage {
        input_tokens: usage
            .get("input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or_default(),
        output_tokens: usage
            .get("output_tokens")
            .and_then(Value::as_u64)
            .unwrap_or_default(),
        total_tokens: usage
            .get("total_tokens")
            .and_then(Value::as_u64)
            .unwrap_or_default(),
        cached_input_tokens: usage
            .pointer("/input_tokens_details/cached_tokens")
            .and_then(Value::as_u64)
            .unwrap_or_default(),
        reasoning_tokens: usage
            .pointer("/output_tokens_details/reasoning_tokens")
            .and_then(Value::as_u64)
            .unwrap_or_default(),
        cost_estimate: None,
    }
}

pub(super) fn parse_openai_response(
    request: &ModelTurnRequest,
    config: &OpenAiProviderConfig,
    payload: Value,
    capabilities: &ProviderProtocolContract,
    model_name: &str,
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
    let message = choice.get("message").unwrap_or(&Value::Null);
    let content = parse_openai_content(message.get("content"));
    let tool_calls = parse_openai_tool_calls(message);
    let assistant_message = Some(ModelMessage {
        tool_calls: tool_calls.clone(),
        ..ModelMessage::text(ModelRole::Assistant, content)
    });
    let finish_reason = choice
        .get("finish_reason")
        .and_then(Value::as_str)
        .map(str::to_string);
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
}

#[allow(clippy::too_many_arguments)]
pub(super) fn finalize_provider_response(
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
        provider_attempt_metadata: None,
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
    if !validation.valid {
        response.status = ModelTurnStatus::Invalid;
        let provider_rejected_parallelism = validation
            .errors
            .iter()
            .any(|error| error == "provider_does_not_support_parallel_tool_calls");
        let (kind, message, diagnostic_code) = if provider_rejected_parallelism {
            (
                ModelErrorKind::UnsupportedCapability,
                "provider does not support parallel tool calls".to_string(),
                "provider_does_not_support_parallel_tool_calls",
            )
        } else {
            (
                ModelErrorKind::JsonSchemaViolation,
                format!("provider_response_invalid: {}", validation.errors.join(",")),
                "provider_response_invalid",
            )
        };
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

pub(super) fn parse_openai_tool_calls(message: &Value) -> Vec<ModelToolCall> {
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

pub(super) fn parse_openai_tool_call(_index: usize, call: &Value) -> ModelToolCall {
    let function = call.get("function").unwrap_or(&Value::Null);
    let arguments_value = function.get("arguments").unwrap_or(&Value::Null);
    let raw_arguments = match arguments_value {
        Value::String(raw) => raw.clone(),
        Value::Object(_) => serde_json::to_string(arguments_value).unwrap_or_default(),
        _ => String::new(),
    };
    let (arguments, parse_status, validation_errors) = if arguments_value.is_object() {
        (
            arguments_value.clone(),
            ModelToolParseStatus::Valid,
            Vec::new(),
        )
    } else {
        parse_tool_arguments(&raw_arguments)
    };
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

pub(super) fn parse_tool_arguments(
    raw_arguments: &str,
) -> (Value, ModelToolParseStatus, Vec<String>) {
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

pub(super) fn parse_openai_content(content: Option<&Value>) -> String {
    match content {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| {
                (part.get("type").and_then(Value::as_str) == Some("text"))
                    .then(|| part.get("text").and_then(Value::as_str))
                    .flatten()
            })
            .collect::<Vec<_>>()
            .join(""),
        Some(_) => String::new(),
    }
}

pub(super) fn parse_openai_usage(usage: Option<&Value>) -> ModelUsage {
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
        cost_estimate: None,
    }
}
