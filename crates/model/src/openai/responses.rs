//! OpenAI Responses 协议请求序列化与响应解析。

use serde_json::{Value, json};

use crate::error::ProviderError;
use crate::openai::chat::{finalize_provider_response, parse_tool_call_arguments};
use crate::provider::contract::{
    ProviderProtocolContract, message_text, provider_response_validation_error,
    request_uses_tool_protocol,
};
use crate::provider::runtime::OpenAiProviderConfig;
use crate::types::{
    ModelMessage, ModelRole, ModelToolCall, ModelTurnRequest, ModelTurnResponse, ModelUsage,
    ProviderReasoningReplay, ProviderToolReasoningMode,
};

pub fn openai_responses_request_payload(
    request: &ModelTurnRequest,
    model_name: &str,
    capabilities: &ProviderProtocolContract,
    reasoning_enabled: bool,
    reasoning_disabled: bool,
    wire_reasoning_effort: Option<&str>,
    supports_tool_choice: bool,
) -> Value {
    let (instructions, input) =
        openai_responses_input(&request.messages, &request.provider_reasoning_history);
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
        if supports_tool_choice {
            payload["tool_choice"] = openai_responses_tool_choice_payload();
            payload["parallel_tool_calls"] = json!(request.tool_choice.max_tool_calls > 1);
        }
    }
    if reasoning_enabled {
        let Some(wire_effort) = wire_reasoning_effort else {
            return payload;
        };
        payload["reasoning"] = json!({"effort": wire_effort});
        payload["include"] = json!(["reasoning.encrypted_content"]);
    } else if reasoning_disabled
        || (request_uses_tool_protocol(request)
            && capabilities.tool_reasoning_mode == ProviderToolReasoningMode::DisabledForToolCalls)
    {
        payload["reasoning"] = json!({"effort": "none"});
    }
    payload
}

pub fn openai_responses_stream_request_payload(
    request: &ModelTurnRequest,
    model_name: &str,
    capabilities: &ProviderProtocolContract,
    reasoning_enabled: bool,
    reasoning_disabled: bool,
    wire_reasoning_effort: Option<&str>,
    supports_tool_choice: bool,
) -> Value {
    let mut payload = openai_responses_request_payload(
        request,
        model_name,
        capabilities,
        reasoning_enabled,
        reasoning_disabled,
        wire_reasoning_effort,
        supports_tool_choice,
    );
    payload["stream"] = json!(true);
    payload
}

pub fn openai_responses_reasoning_content_present(payload: &Value) -> bool {
    payload
        .get("output")
        .and_then(Value::as_array)
        .is_some_and(|items| {
            items
                .iter()
                .any(|item| item.get("type").and_then(Value::as_str) == Some("reasoning"))
        })
}

pub fn parse_openai_responses_response(
    request: &ModelTurnRequest,
    config: &OpenAiProviderConfig,
    payload: Value,
    capabilities: &ProviderProtocolContract,
    model_name: &str,
    reasoning_effort: Option<&str>,
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
    let incomplete_reason = payload
        .get("incomplete_details")
        .and_then(|details| details.get("reason"))
        .and_then(Value::as_str);
    let length_truncated =
        status == Some("incomplete") && incomplete_reason == Some("max_output_tokens");
    if status != Some("completed") && !length_truncated {
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
    let mut tool_calls: Vec<ModelToolCall> = Vec::new();
    let mut replay_items = Vec::new();
    for (index, item) in output.iter().enumerate() {
        let item = item.as_object().ok_or_else(|| {
            provider_response_validation_error(
                config,
                model_name,
                "provider Responses output item was not an object",
                vec!["responses_output_item_invalid".to_string()],
            )
        })?;
        let item_type = item.get("type").and_then(Value::as_str).ok_or_else(|| {
            provider_response_validation_error(
                config,
                model_name,
                "provider Responses output item type was missing",
                vec!["responses_output_item_type_missing".to_string()],
            )
        })?;
        match item_type {
            "message" => {
                let item_value = Value::Object(item.clone());
                let message = parse_openai_responses_message(&item_value).map_err(|evidence| {
                    provider_response_validation_error(
                        config,
                        model_name,
                        "provider Responses message content was invalid",
                        vec![evidence.to_string()],
                    )
                })?;
                content.push_str(&message);
            }
            "function_call" => {
                let item_value = Value::Object(item.clone());
                let call = parse_openai_responses_tool_call(index, &item_value);
                if call.tool_call_id.is_empty() {
                    return Err(provider_response_validation_error(
                        config,
                        model_name,
                        "provider Responses function_call id was missing",
                        vec!["responses_function_call_id_missing".to_string()],
                    ));
                }
                if tool_calls
                    .iter()
                    .any(|existing| existing.tool_call_id == call.tool_call_id)
                {
                    return Err(provider_response_validation_error(
                        config,
                        model_name,
                        "provider Responses function_call ids were duplicated",
                        vec!["responses_function_call_id_duplicate".to_string()],
                    ));
                }
                tool_calls.push(call);
                replay_items.push(Value::Object(item.clone()));
            }
            "reasoning" => {
                if item
                    .get("id")
                    .and_then(Value::as_str)
                    .is_none_or(str::is_empty)
                {
                    return Err(provider_response_validation_error(
                        config,
                        model_name,
                        "provider Responses reasoning item id was missing",
                        vec!["responses_reasoning_item_id_missing".to_string()],
                    ));
                }
                replay_items.push(Value::Object(item.clone()));
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
        if item_type == "message" {
            replay_items.push(Value::Object(item.clone()));
        }
    }
    let assistant_message = Some(ModelMessage {
        tool_calls: tool_calls.clone(),
        ..ModelMessage::text(ModelRole::Assistant, content)
    });
    let has_reasoning_item = replay_items
        .iter()
        .any(|item| item.get("type").and_then(Value::as_str) == Some("reasoning"));
    let provider_reasoning_history = if has_reasoning_item && !tool_calls.is_empty() {
        let tool_call_ids: Vec<String> = tool_calls
            .iter()
            .map(|call| call.tool_call_id.clone())
            .collect();
        vec![ProviderReasoningReplay::Responses {
            provider_name: config.provider_name.clone(),
            model_name: model_name.to_string(),
            reasoning_effort: reasoning_effort.unwrap_or("off").to_string(),
            tool_call_ids,
            items: replay_items,
        }]
    } else {
        Vec::new()
    };
    if has_reasoning_item
        && !tool_calls.is_empty()
        && capabilities.tool_reasoning_mode == ProviderToolReasoningMode::ReplayResponsesItems
    {
        let Some(replay) = provider_reasoning_history.first() else {
            return Err(provider_response_validation_error(
                config,
                model_name,
                "provider Responses tool calls did not include a valid reasoning replay",
                vec!["responses_reasoning_replay_invalid".to_string()],
            ));
        };
        if replay.validate().is_err() {
            return Err(provider_response_validation_error(
                config,
                model_name,
                "provider Responses reasoning replay was invalid",
                vec!["responses_reasoning_replay_invalid".to_string()],
            ));
        }
    }
    let response_finish_reason = if length_truncated {
        "length"
    } else if !tool_calls.is_empty() {
        "tool_calls"
    } else {
        "stop"
    };
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
        Some(response_finish_reason.to_string()),
    )
    .map(|mut response| {
        response.provider_reasoning_history = provider_reasoning_history;
        response
    })
}

pub fn parse_openai_responses_message(message: &Value) -> Result<String, &'static str> {
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

pub fn parse_openai_responses_tool_call(_index: usize, call: &Value) -> ModelToolCall {
    let (arguments, raw_arguments, parse_status, validation_errors) =
        parse_tool_call_arguments(call.get("arguments"));
    ModelToolCall {
        tool_call_id: call
            .get("call_id")
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

pub fn parse_openai_responses_usage(usage: Option<&Value>) -> ModelUsage {
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
        usage_present: true,
    }
}

pub fn openai_responses_input(
    messages: &[ModelMessage],
    reasoning_history: &[ProviderReasoningReplay],
) -> (Option<String>, Vec<Value>) {
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
                let call_ids = message
                    .tool_calls
                    .iter()
                    .map(|call| call.tool_call_id.clone())
                    .collect::<Vec<_>>();
                if let Some(ProviderReasoningReplay::Responses {
                    items: replay_items,
                    ..
                }) = reasoning_history
                    .iter()
                    .find(|replay| replay.matches_tool_call_ids(&call_ids))
                {
                    items.extend(replay_items.iter().cloned());
                } else {
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
            }
            ModelRole::System | ModelRole::Developer | ModelRole::User => {
                let role = match message.role {
                    ModelRole::System => "system",
                    // Leading system/developer messages are collapsed into the
                    // Responses instructions field above. A developer message
                    // that appears after user/assistant history must use a role
                    // accepted by the OpenAI-compatible Responses input schema;
                    // system preserves its instruction semantics.
                    ModelRole::Developer => "system",
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

pub fn openai_responses_tool_choice_payload() -> Value {
    json!("auto")
}
