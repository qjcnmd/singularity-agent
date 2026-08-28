//! OpenAI Responses 协议请求序列化与响应解析。

use serde_json::{Value, json};

use crate::error::ProviderError;
use crate::openai::chat::{
    ParsedResponseParts, finalize_provider_response, parse_message_content, parse_tool_call,
    parse_usage,
};
use crate::provider::contract::{
    ProviderProtocolContract, message_text, provider_response_validation_error,
};
use crate::provider::runtime::{OpenAiProviderConfig, WireRequestOptions};
use crate::types::{
    ModelMessage, ModelRole, ModelToolCall, ModelTurnRequest, ModelTurnResponse, ModelUsage,
    ProviderReasoningReplay, ProviderToolReasoningMode,
};

pub fn openai_responses_request_payload(
    request: &ModelTurnRequest,
    model_name: &str,
    capabilities: &ProviderProtocolContract,
    wire: &WireRequestOptions,
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
    let reasoning = super::reasoning_wire_decision(request, capabilities, wire);
    if let Some(instructions) = instructions {
        payload["instructions"] = json!(instructions);
    }
    if let Some(max_output_tokens) = request.model_preferences.max_output_tokens {
        payload["max_output_tokens"] = json!(max_output_tokens);
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
        if wire.supports_tool_choice {
            payload["tool_choice"] = super::tool_choice_payload();
            // 诚实信号：本地按模型给定顺序串行执行全部工具调用，不请求并行。
            payload["parallel_tool_calls"] = json!(false);
        }
    }
    if reasoning.enabled {
        let Some(wire_effort) = reasoning.effort else {
            return payload;
        };
        payload["reasoning"] = json!({"effort": wire_effort});
        payload["include"] = json!(["reasoning.encrypted_content"]);
    } else if reasoning.disabled || reasoning.disabled_for_tool_calls {
        payload["reasoning"] = json!({"effort": "none"});
    }
    payload
}

pub fn openai_responses_stream_request_payload(
    request: &ModelTurnRequest,
    model_name: &str,
    capabilities: &ProviderProtocolContract,
    wire: &WireRequestOptions,
) -> Value {
    let mut payload = openai_responses_request_payload(request, model_name, capabilities, wire);
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
    let parsed = parse_responses_output(output, config, model_name)?;
    let ParsedResponsesOutput {
        content,
        tool_calls,
        replay_items,
    } = parsed;
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
            // 绑定请求时实际 selection 的 reasoning 变体；provider 不回显
            // effort 时保持 None，不伪造禁用变体。
            reasoning_effort: reasoning_effort.map(str::to_string),
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
        ParsedResponseParts {
            response_id: payload
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("response")
                .to_string(),
            assistant_message,
            usage: parse_openai_responses_usage(payload.get("usage")),
            finish_reason: Some(response_finish_reason.to_string()),
        },
    )
    .map(|mut response| {
        response.provider_reasoning_history = provider_reasoning_history;
        response
    })
}

struct ParsedResponsesOutput {
    content: String,
    tool_calls: Vec<ModelToolCall>,
    replay_items: Vec<Value>,
}

fn parse_responses_output(
    output: &[Value],
    config: &OpenAiProviderConfig,
    model_name: &str,
) -> Result<ParsedResponsesOutput, ProviderError> {
    let mut content = String::new();
    let mut tool_calls = Vec::new();
    let mut replay_items = Vec::new();
    for item in output {
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
                replay_items.push(item_value);
            }
            "function_call" => {
                let item_value = Value::Object(item.clone());
                let call = parse_tool_call(
                    &item_value,
                    "call_id",
                    item_value.get("name"),
                    item_value.get("arguments"),
                );
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
                    .any(|existing: &ModelToolCall| existing.tool_call_id == call.tool_call_id)
                {
                    return Err(provider_response_validation_error(
                        config,
                        model_name,
                        "provider Responses function_call ids were duplicated",
                        vec!["responses_function_call_id_duplicate".to_string()],
                    ));
                }
                tool_calls.push(call);
                replay_items.push(item_value);
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
    }
    Ok(ParsedResponsesOutput {
        content,
        tool_calls,
        replay_items,
    })
}

pub fn parse_openai_responses_message(message: &Value) -> Result<String, &'static str> {
    parse_message_content(
        message.get("content"),
        &["output_text"],
        Some("responses_message_content_missing"),
        "responses_message_content_invalid",
        "responses_message_content_part_unsupported",
        "responses_message_content_text_missing",
    )
}

pub fn parse_openai_responses_usage(usage: Option<&Value>) -> ModelUsage {
    parse_usage(
        usage,
        "input_tokens",
        "output_tokens",
        "/input_tokens_details/cached_tokens",
        "/output_tokens_details/reasoning_tokens",
    )
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
                }) = super::matching_reasoning_replay(reasoning_history, &call_ids)
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
                    // 开头的 system/developer 消息已折叠进上方 Responses
                    // instructions 字段；出现在 user/assistant 历史之后的
                    // developer 消息必须使用 OpenAI 兼容 Responses 输入
                    // schema 接受的角色；system 保留其指令语义。
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
