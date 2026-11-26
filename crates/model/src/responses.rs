mod stream;
pub(crate) use self::stream::read_responses_sse_stream;

use serde_json::{Value, json};

use crate::config::selection::{ProviderConfig, SelectedModel};
use crate::error::{
    ModelErrorKind, ProviderError, bounded_provider_error_diagnostic, provider_embedded_error,
    provider_error_fields,
};
use crate::provider::{
    finalize_provider_response, image_data_url, message_metadata_for, parse_tool_call_arguments,
    provider_response_validation_error, reasoning_wire_decision, serialize_tool_call_arguments,
    split_leading_instructions,
};
use crate::types::{
    ContentBlock, Message, ModelStopReason, ModelToolCall, ModelTurnRequest, ModelTurnResponse, ModelUsage,
    ProviderBlockMetadata, ProviderMessageMetadata,
};

pub(crate) fn openai_responses_stream_request_payload(
    request: &ModelTurnRequest,
    selection: &SelectedModel,
    provider_name: &str,
    endpoint: &str,
) -> Result<Value, ProviderError> {
    let (instructions, input) =
        openai_responses_input(&request.messages, &request.images, selection, provider_name, endpoint)?;
    let mut payload = json!({
        "model": selection.model_name,
        "input": input,
        "stream": true,
        "store": false,
        "include": ["reasoning.encrypted_content"],
    });
    let reasoning = reasoning_wire_decision(selection);
    if let Some(instructions) = instructions {
        payload["instructions"] = json!(instructions);
    }
    if let Some(prompt_cache_key) = request.prompt_cache_key.as_deref().filter(|key| !key.trim().is_empty()) {
        payload["prompt_cache_key"] = json!(prompt_cache_key);
    }
    payload["max_output_tokens"] = json!(request.max_output_tokens);
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
                        // 保留工具允许省略的参数，避免 Responses 自动改写为 strict schema。
                        "strict": false,
                    })
                })
                .collect::<Vec<_>>()
        );
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
    Ok(payload)
}

pub(crate) fn parse_openai_responses_response(
    config: &ProviderConfig,
    mut payload: Value,
    model_name: &str,
    endpoint: &str,
    reasoning: Option<String>,
) -> Result<ModelTurnResponse, ProviderError> {
    if let Some(error) = payload.get("error").filter(|error| !error.is_null()) {
        return Err(provider_embedded_error(
            &provider_error_fields(error),
            "provider Responses payload contained an error",
            &config.api_key,
        ));
    }
    let status = payload.get("status").and_then(Value::as_str);
    let incomplete_reason = payload
        .get("incomplete_details")
        .and_then(|details| details.get("reason"))
        .and_then(Value::as_str);
    let length_truncated = status == Some("incomplete") && incomplete_reason == Some("max_output_tokens");
    if status != Some("completed") && !length_truncated {
        if incomplete_reason == Some("content_filter") {
            return Err(ProviderError::new(
                ModelErrorKind::Other,
                "provider Responses response was stopped by content filter",
            ));
        }
        let status = status.map(bounded_provider_error_diagnostic).filter(|s| !s.is_empty());
        let reason = incomplete_reason.map(bounded_provider_error_diagnostic).filter(|s| !s.is_empty());
        return Err(provider_response_validation_error(
            &format!(
                "provider Responses payload was not completed (status: {}, reason: {})",
                status.as_deref().unwrap_or("missing"),
                reason.as_deref().unwrap_or("unknown")
            ),
            &["responses_status_not_completed"],
        ));
    }
    let Some(Value::Array(output)) = payload.get_mut("output").map(std::mem::take) else {
        return Err(provider_response_validation_error(
            "provider Responses payload missing output items",
            &["responses_output_missing"],
        ));
    };
    let mut content = Vec::new();
    let mut has_refusal = false;
    for item in output {
        let Value::Object(item) = item else {
            return Err(provider_response_validation_error(
                "provider Responses output item type was missing",
                &["responses_output_item_type_missing"],
            ));
        };
        let Some(item_type) = item.get("type").and_then(Value::as_str) else {
            return Err(provider_response_validation_error(
                "provider Responses output item type was missing",
                &["responses_output_item_type_missing"],
            ));
        };
        let block = match item_type {
            "message" => {
                let (text, item_refusal) =
                    parse_responses_message_content(item.get("content")).map_err(|evidence| {
                        provider_response_validation_error(
                            "provider Responses message content was invalid",
                            &[evidence],
                        )
                    })?;
                has_refusal |= item_refusal;
                ContentBlock::Text {
                    text,
                    metadata: Some(block_metadata(item, &["content"])),
                }
            }
            "function_call" => {
                let arguments = parse_tool_call_arguments(item.get("arguments"))?;
                ContentBlock::ToolCall(ModelToolCall {
                    tool_call_id: item.get("call_id").and_then(Value::as_str).unwrap_or_default().into(),
                    tool_name: item.get("name").and_then(Value::as_str).unwrap_or_default().into(),
                    arguments,
                    metadata: Some(block_metadata(item, &["call_id", "name", "arguments"])),
                })
            }
            "reasoning" => {
                if item.get("id").and_then(Value::as_str).is_none_or(str::is_empty) {
                    return Err(provider_response_validation_error(
                        "provider Responses reasoning item id was missing",
                        &["responses_reasoning_item_id_missing"],
                    ));
                }
                let mut thinking = String::new();
                for text in item
                    .get("summary")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter(|part| part.get("type").and_then(Value::as_str) == Some("summary_text"))
                    .filter_map(|part| part.get("text").and_then(Value::as_str))
                    .filter(|text| !text.is_empty())
                {
                    thinking.push_str(summary_part_separator(&thinking, text));
                    thinking.push_str(text);
                }
                ContentBlock::Thinking {
                    thinking,
                    metadata: Some(block_metadata(item, &["summary"])),
                }
            }
            _ => {
                return Err(provider_response_validation_error(
                    "provider Responses payload contained an unsupported output item",
                    &["responses_output_item_unsupported"],
                ));
            }
        };
        content.push(block);
    }
    let provider_metadata = Some(ProviderMessageMetadata {
        protocol: protocol::ProviderApiProtocol::Responses,
        provider_name: config.provider_name.clone(),
        model_name: model_name.into(),
        endpoint: endpoint.into(),
        prefix_digest: None,
        reasoning,
    });
    Ok(finalize_provider_response(ModelTurnResponse {
        assistant_message: Message::Assistant { content, provider_metadata },
        usage: parse_responses_usage(payload.get("usage")),
        stop_reason: if length_truncated {
            ModelStopReason::Length
        } else {
            ModelStopReason::Stop
        },
        refusal: has_refusal,
    }))
}

fn parse_responses_usage(usage: Option<&Value>) -> Option<ModelUsage> {
    let usage = usage?;
    let count = |value: Option<&Value>| value.and_then(Value::as_u64);
    let input_tokens = count(usage.get("input_tokens"))?;
    let output_tokens = count(usage.get("output_tokens"))?;
    Some(ModelUsage {
        input_tokens,
        output_tokens,
        total_tokens: count(usage.get("total_tokens"))
            .unwrap_or_else(|| input_tokens.saturating_add(output_tokens)),
        cached_input_tokens: count(usage.pointer("/input_tokens_details/cached_tokens")),
    })
}

fn parse_responses_message_content(content: Option<&Value>) -> Result<(String, bool), &'static str> {
    match content {
        None | Some(Value::Null) => Err("responses_message_content_missing"),
        Some(Value::String(text)) => Ok((text.clone(), false)),
        Some(Value::Array(parts)) => {
            let mut content = String::new();
            let mut refusal = false;
            for part in parts {
                let text = match part.get("type").and_then(Value::as_str) {
                    Some("text" | "output_text") => part.get("text").and_then(Value::as_str),
                    Some("refusal") => {
                        refusal = true;
                        part.get("refusal").and_then(Value::as_str)
                    }
                    _ => return Err("responses_message_content_part_unsupported"),
                }
                .ok_or("responses_message_content_text_missing")?;
                content.push_str(text);
            }
            Ok((content, refusal))
        }
        Some(_) => Err("responses_message_content_invalid"),
    }
}

pub(crate) fn openai_responses_input(
    messages: &[Message],
    images: &std::collections::HashMap<String, String>,
    selection: &SelectedModel,
    provider_name: &str,
    endpoint: &str,
) -> Result<(Option<String>, Vec<Value>), ProviderError> {
    let (instructions, rest) = split_leading_instructions(messages);
    let mut items = Vec::new();
    for message in rest {
        match message {
            Message::ToolResult { tool_call_id, .. } => {
                items.push(json!({
                    "type": "function_call_output",
                    "call_id": tool_call_id,
                    "output": responses_content(message, images)?,
                }));
            }
            Message::Assistant { content, .. } => {
                let same_provider =
                    message_metadata_for(message, selection, provider_name, endpoint).is_some();
                for block in content {
                    let (fields, metadata): (Vec<(&str, Value)>, Option<&ProviderBlockMetadata>) = match block
                    {
                        ContentBlock::Text { text, metadata } => (
                            vec![
                                ("type", json!("message")),
                                ("role", json!("assistant")),
                                ("content", json!([{"type": "output_text", "text": text}])),
                            ],
                            metadata.as_ref(),
                        ),
                        ContentBlock::Thinking { thinking, metadata }
                            if same_provider && metadata.is_some() =>
                        {
                            let summary = if thinking.is_empty() {
                                vec![]
                            } else {
                                vec![json!({"type": "summary_text", "text": thinking})]
                            };
                            (
                                vec![("type", json!("reasoning")), ("summary", json!(summary))],
                                metadata.as_ref(),
                            )
                        }
                        ContentBlock::ToolCall(call) => (
                            vec![
                                ("type", json!("function_call")),
                                ("call_id", json!(call.tool_call_id)),
                                ("name", json!(call.tool_name)),
                                ("arguments", json!(serialize_tool_call_arguments(&call.arguments))),
                            ],
                            call.metadata.as_ref(),
                        ),
                        _ => continue,
                    };
                    let mut map = if same_provider {
                        metadata.map(|meta| meta.0.clone()).unwrap_or_default()
                    } else {
                        serde_json::Map::new()
                    };
                    for (key, value) in fields {
                        map.insert(key.to_string(), value);
                    }
                    items.push(Value::Object(map));
                }
            }
            Message::Developer { .. } | Message::User { .. } | Message::Notification { .. } => {
                let role = if matches!(message, Message::Developer { .. }) {
                    "system"
                } else {
                    "user"
                };
                items.push(json!({
                    "type": "message",
                    "role": role,
                    "content": responses_content(message, images)?,
                }));
            }
        }
    }
    Ok((instructions, items))
}

fn responses_content(
    message: &Message,
    images: &std::collections::HashMap<String, String>,
) -> Result<Value, ProviderError> {
    if message.images().next().is_none() {
        return Ok(json!(message.content_text()));
    }
    let mut parts = vec![json!({"type": "input_text", "text": message.content_text()})];
    for image in message.images() {
        let url = image_data_url(images, &image.id)?;
        parts.push(json!({"type": "input_image", "image_url": url}));
    }
    Ok(json!(parts))
}

fn summary_part_separator(existing: &str, next: &str) -> &'static str {
    if existing.is_empty()
        || next.is_empty()
        || existing.ends_with("\n\n")
        || next.starts_with("\n\n")
        || (existing.ends_with('\n') && next.starts_with('\n'))
    {
        ""
    } else if existing.ends_with('\n') || next.starts_with('\n') {
        "\n"
    } else {
        "\n\n"
    }
}

fn block_metadata(
    mut fields: serde_json::Map<String, Value>,
    content_fields: &[&str],
) -> ProviderBlockMetadata {
    for field in content_fields {
        fields.remove(*field);
    }
    ProviderBlockMetadata(fields)
}
