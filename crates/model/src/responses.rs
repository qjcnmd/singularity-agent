//! OpenAI Responses 协议：请求编码、SSE 解码与响应解析。

mod stream;
pub(crate) use self::stream::read_responses_sse_stream;

use serde_json::{Value, json};

use crate::config::selection::{ProviderConfig, SelectedModel};
use crate::error::{
    ModelErrorKind, ProviderError, bounded_provider_error_diagnostic, provider_embedded_error,
    provider_error_fields,
};
use crate::provider::{
    finalize_provider_response, image_data_url, message_metadata_for, parse_tool_call_arguments, parse_usage,
    provider_response_validation_error, reasoning_wire_decision, serialize_tool_call_arguments,
};
use crate::types::{
    ContentBlock, Message, ModelRole, ModelStopReason, ModelToolCall, ModelTurnRequest, ModelTurnResponse,
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
) -> Result<ModelTurnResponse, ProviderError> {
    if let Some(error) = payload.get("error").filter(|error| !error.is_null()) {
        return Err(provider_embedded_error(
            &provider_error_fields(error),
            "provider Responses payload contained an error",
        ));
    }
    let status = payload.get("status").and_then(Value::as_str);
    let incomplete_reason = payload
        .get("incomplete_details")
        .and_then(|details| details.get("reason"))
        .and_then(Value::as_str);
    let length_truncated = status == Some("incomplete") && incomplete_reason == Some("max_output_tokens");
    // 只有触达输出上限的不完整响应算正常截断，其余未完成一律失败。
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
    // 内容按提供方输出的顺序保存；续接字段跟随各块，正文和参数不重复保存。
    let Some(Value::Array(output)) = payload.get_mut("output").map(std::mem::take) else {
        return Err(provider_response_validation_error(
            "provider Responses payload missing output items",
            &["responses_output_missing"],
        ));
    };
    let mut content = Vec::new();
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
                let text = parse_responses_message_content(item.get("content")).map_err(|evidence| {
                    provider_response_validation_error(
                        "provider Responses message content was invalid",
                        &[evidence],
                    )
                })?;
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
                let thinking = item
                    .get("summary")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter(|part| part.get("type").and_then(Value::as_str) == Some("summary_text"))
                    .filter_map(|part| part.get("text").and_then(Value::as_str))
                    .collect();
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
    let provider_metadata = Some(ProviderMessageMetadata::Responses {
        provider_name: config.provider_name.clone(),
        model_name: model_name.into(),
        endpoint: endpoint.into(),
    });
    Ok(finalize_provider_response(ModelTurnResponse {
        assistant_message: Message::Assistant { content, provider_metadata },
        usage: parse_usage(
            payload.get("usage"),
            "input_tokens",
            "output_tokens",
            "/input_tokens_details/cached_tokens",
        ),
        stop_reason: if length_truncated {
            ModelStopReason::Length
        } else {
            ModelStopReason::Stop
        },
    }))
}

/// message content 规则：缺 content 算协议错误，text/output_text 与 refusal 都拼成可见文本。
fn parse_responses_message_content(content: Option<&Value>) -> Result<String, &'static str> {
    match content {
        None | Some(Value::Null) => Err("responses_message_content_missing"),
        Some(Value::String(text)) => Ok(text.clone()),
        Some(Value::Array(parts)) => {
            let mut content = String::new();
            for part in parts {
                let text = match part.get("type").and_then(Value::as_str) {
                    Some("text" | "output_text") => part.get("text").and_then(Value::as_str),
                    Some("refusal") => part.get("refusal").and_then(Value::as_str),
                    _ => return Err("responses_message_content_part_unsupported"),
                }
                .ok_or("responses_message_content_text_missing")?;
                content.push_str(text);
            }
            Ok(content)
        }
        Some(_) => Err("responses_message_content_invalid"),
    }
}

/// 把消息编码成 Responses 的 input；是否附带提供方私有数据见 `message_metadata_for`。
pub(crate) fn openai_responses_input(
    messages: &[Message],
    images: &std::collections::HashMap<String, String>,
    selection: &SelectedModel,
    provider_name: &str,
    endpoint: &str,
) -> Result<(Option<String>, Vec<Value>), ProviderError> {
    let instruction_count =
        messages.iter().take_while(|message| message.role() == ModelRole::Developer).count();
    let instructions = messages[..instruction_count]
        .iter()
        .map(Message::content_text)
        .filter(|message| !message.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    let mut items = Vec::new();
    for message in &messages[instruction_count..] {
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
                // 开头的 developer 已折叠进 instructions；历史之后的 developer
                // 使用 Responses 输入 schema 接受的 system 角色。
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
    Ok(((!instructions.is_empty()).then_some(instructions), items))
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

fn block_metadata(
    mut fields: serde_json::Map<String, Value>,
    content_fields: &[&str],
) -> ProviderBlockMetadata {
    for field in content_fields {
        fields.remove(*field);
    }
    ProviderBlockMetadata(fields)
}
