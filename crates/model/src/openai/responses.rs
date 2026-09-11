//! OpenAI Responses 协议请求序列化与响应解析。

use serde_json::{Value, json};

use crate::error::ProviderError;
use crate::openai::parse::{
    finalize_provider_response, parse_message_content, parse_tool_call, parse_usage,
};
use crate::provider::contract::{
    provider_content_filter_error, provider_response_validation_error,
};
use crate::provider::runtime::{OpenAiProviderConfig, SelectedModel};
use crate::transport::{provider_embedded_error, provider_error_fields};
use crate::types::{
    ModelMessage, ModelRole, ModelStopReason, ModelToolCall, ModelTurnRequest, ModelTurnResponse,
    ProviderReasoningReplay,
};

pub fn openai_responses_stream_request_payload(
    request: &ModelTurnRequest,
    model_name: &str,
    selection: &SelectedModel,
) -> Value {
    let (instructions, input) = openai_responses_input(&request.messages);
    let mut payload = json!({
        "model": request
            .model_preferences
            .model_name
            .as_deref()
            .unwrap_or(model_name),
        "input": input,
        "stream": true,
        "store": false,
        "include": ["reasoning.encrypted_content"],
    });
    let reasoning = super::reasoning_wire_decision(selection);
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
                    })
                })
                .collect::<Vec<_>>()
        );
        if selection.supports_tool_choice {
            payload["tool_choice"] = serde_json::json!("auto");
        }
    }
    match reasoning.enabled {
        Some(true) => {
            if let Some(effort) = reasoning.effort {
                payload["reasoning"] = json!({"effort": effort});
            }
        }
        Some(false) => {
            payload["reasoning"] = json!({"effort": "none"});
        }
        None => {}
    }
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
    model_name: &str,
    reasoning_effort: Option<&str>,
) -> Result<ModelTurnResponse, ProviderError> {
    if let Some(error) = payload.get("error").filter(|error| !error.is_null()) {
        return Err(provider_embedded_error(
            &provider_error_fields(error),
            "provider Responses payload contained an error",
            "responses_error_present",
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
        if incomplete_reason == Some("content_filter") {
            return Err(provider_content_filter_error(
                "provider Responses response was stopped by content filter",
            ));
        }
        return Err(provider_response_validation_error(
            &format!(
                "provider Responses payload was not completed (reason: {})",
                incomplete_reason.unwrap_or("unknown")
            ),
            vec!["responses_status_not_completed".to_string()],
        ));
    }
    let output = payload
        .get("output")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            provider_response_validation_error(
                "provider Responses payload missing output items",
                vec!["responses_output_missing".to_string()],
            )
        })?;
    let parsed = parse_responses_output(output)?;
    let ParsedResponsesOutput {
        content,
        thinking,
        tool_calls,
        replay_items,
    } = parsed;
    let has_reasoning_item = replay_items
        .iter()
        .any(|item| item.get("type").and_then(Value::as_str) == Some("reasoning"));
    let replay = if has_reasoning_item {
        Some(ProviderReasoningReplay::Responses {
            provider_name: config.provider_name.clone(),
            model_name: model_name.to_string(),
            reasoning_effort: reasoning_effort.map(str::to_string),
            tool_call_ids: tool_calls
                .iter()
                .map(|call| call.tool_call_id.clone())
                .collect(),
            items: replay_items,
        })
    } else {
        None
    };
    finalize_provider_response(
        request,
        ModelTurnResponse {
            assistant_message: ModelMessage {
                tool_calls,
                provider_reasoning_replay: replay,
                ..ModelMessage::text(ModelRole::Assistant, content)
            },
            thinking,
            usage: parse_usage(
                payload.get("usage"),
                "input_tokens",
                "output_tokens",
                "/input_tokens_details/cached_tokens",
                "/output_tokens_details/reasoning_tokens",
            ),
            stop_reason: Some(if length_truncated {
                ModelStopReason::Length
            } else {
                ModelStopReason::Stop
            }),
        },
    )
}

struct ParsedResponsesOutput {
    content: String,
    thinking: String,
    tool_calls: Vec<ModelToolCall>,
    replay_items: Vec<Value>,
}

fn parse_responses_output(output: &[Value]) -> Result<ParsedResponsesOutput, ProviderError> {
    let mut content = String::new();
    let mut thinking = String::new();
    let mut tool_calls = Vec::new();
    let mut replay_items = Vec::new();
    for item in output {
        let item = item.as_object().ok_or_else(|| {
            provider_response_validation_error(
                "provider Responses output item was not an object",
                vec!["responses_output_item_invalid".to_string()],
            )
        })?;
        let item_type = item.get("type").and_then(Value::as_str).ok_or_else(|| {
            provider_response_validation_error(
                "provider Responses output item type was missing",
                vec!["responses_output_item_type_missing".to_string()],
            )
        })?;
        match item_type {
            "message" => {
                let item_value = Value::Object(item.clone());
                let message = parse_message_content(
                    item_value.get("content"),
                    &["output_text"],
                    Some("responses_message_content_missing"),
                    "responses_message_content_invalid",
                    "responses_message_content_part_unsupported",
                    "responses_message_content_text_missing",
                )
                .map_err(|evidence| {
                    provider_response_validation_error(
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
                tool_calls.push(call);
                replay_items.push(item_value);
            }
            "reasoning" => {
                for summary in item
                    .get("summary")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    if summary.get("type").and_then(Value::as_str) == Some("summary_text")
                        && let Some(text) = summary.get("text").and_then(Value::as_str)
                    {
                        thinking.push_str(text);
                    }
                }
                if item
                    .get("id")
                    .and_then(Value::as_str)
                    .is_none_or(str::is_empty)
                {
                    return Err(provider_response_validation_error(
                        "provider Responses reasoning item id was missing",
                        vec!["responses_reasoning_item_id_missing".to_string()],
                    ));
                }
                replay_items.push(Value::Object(item.clone()));
            }
            _ => {
                return Err(provider_response_validation_error(
                    "provider Responses payload contained an unsupported output item",
                    vec!["responses_output_item_unsupported".to_string()],
                ));
            }
        }
    }
    Ok(ParsedResponsesOutput {
        content,
        thinking,
        tool_calls,
        replay_items,
    })
}

pub fn openai_responses_input(messages: &[ModelMessage]) -> (Option<String>, Vec<Value>) {
    let instruction_count = messages
        .iter()
        .take_while(|message| matches!(message.role, ModelRole::System | ModelRole::Developer))
        .count();
    let instructions = messages[..instruction_count]
        .iter()
        .map(|message| message.content.as_str())
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
                    "output": message.content,
                }));
            }
            ModelRole::Assistant => {
                if let Some(ProviderReasoningReplay::Responses {
                    items: replay_items,
                    ..
                }) = message.provider_reasoning_replay.as_ref()
                {
                    items.extend(replay_items.iter().cloned());
                } else {
                    if !message.content.is_empty() {
                        items.push(json!({
                            "type": "message",
                            "role": "assistant",
                            "content": message.content,
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
                    // 开头的 system/developer 消息已折叠进上方 Responses
                    // instructions 字段；出现在 user/assistant 历史之后的
                    // developer 消息必须使用 OpenAI 兼容 Responses 输入
                    // schema 接受的角色；system 保留其指令语义。
                    ModelRole::System | ModelRole::Developer => "system",
                    ModelRole::User => "user",
                    ModelRole::Assistant | ModelRole::Tool => unreachable!(),
                };
                items.push(json!({
                    "type": "message",
                    "role": role,
                    "content": message.content,
                }));
            }
        }
    }
    ((!instructions.is_empty()).then_some(instructions), items)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)] // 测试断言惯例
    use super::*;

    #[test]
    fn display_summary_is_separate_from_encrypted_continuation() -> Result<(), ProviderError> {
        let config = OpenAiProviderConfig {
            provider_name: "test".into(),
            base_url: "http://localhost/v1".into(),
            api_key: "test".into(),
        };
        let request = ModelTurnRequest::new(
            "request",
            vec![ModelMessage::text(ModelRole::User, "hello")],
        );
        let response = parse_openai_responses_response(
            &request,
            &config,
            json!({
                "id": "response", "status": "completed", "output": [
                    {"type": "reasoning", "id": "r", "encrypted_content": "private continuation", "summary": [{"type": "summary_text", "text": "visible summary"}]},
                    {"type": "message", "id": "m", "role": "assistant", "content": [{"type": "output_text", "text": "answer"}]}
                ]
            }),
            "model",
            None,
        )?;
        assert_eq!(response.thinking, "visible summary");
        let message = &response.assistant_message;
        assert!(message.provider_reasoning_replay.is_some());
        let (_, replayed) = openai_responses_input(std::slice::from_ref(message));
        assert_eq!(replayed[0]["encrypted_content"], "private continuation");
        assert!(
            !serde_json::to_string(&response)
                .unwrap()
                .contains("private continuation")
        );
        Ok(())
    }
}
