mod stream;
pub(crate) use self::stream::read_messages_sse_stream;

use std::collections::HashMap;

use serde_json::{Value, json};

use crate::config::selection::SelectedModel;
use crate::error::{ModelErrorKind, ProviderError};
use crate::provider::{
    image_data_url, message_metadata_for, reasoning_wire_decision, split_leading_instructions,
};
use crate::types::{ContentBlock, Message, ModelTurnRequest};

pub(crate) fn messages_stream_request_payload(
    request: &ModelTurnRequest,
    selection: &SelectedModel,
    provider_name: &str,
    endpoint: &str,
) -> Result<Value, ProviderError> {
    let (system, rest) = split_leading_instructions(&request.messages);
    let system_json = system.map(|text| {
        json!([{
            "type": "text",
            "text": text,
            "cache_control": { "type": "ephemeral" },
        }])
    });
    let tools_json = (!request.tools.is_empty()).then(|| {
        let last_index = request.tools.len() - 1;
        let tools: Vec<Value> = request
            .tools
            .iter()
            .enumerate()
            .map(|(index, tool)| {
                let mut item = json!({
                    "name": tool.name,
                    "description": tool.description,
                    "input_schema": tool.parameters_schema,
                });
                if index == last_index {
                    item["cache_control"] = json!({ "type": "ephemeral" });
                }
                item
            })
            .collect();
        json!(tools)
    });
    let mut messages = encode_messages(rest, &request.images, selection, provider_name, endpoint)?;
    apply_rolling_message_cache_control(&mut messages);

    let mut payload = json!({
        "model": selection.model_name,
        "max_tokens": request.max_output_tokens,
        "stream": true,
        "messages": messages,
    });
    if let Some(system_json) = system_json {
        payload["system"] = system_json;
    }
    if let Some(tools_json) = tools_json {
        payload["tools"] = tools_json;
    }
    let reasoning = reasoning_wire_decision(selection);
    match reasoning.enabled {
        Some(true) => {
            payload["thinking"] = json!({ "type": "adaptive" });
            if let Some(effort) = reasoning.effort {
                payload["output_config"] = json!({ "effort": effort });
            }
        }
        Some(false) => {
            payload["thinking"] = json!({ "type": "disabled" });
        }
        None => {}
    }
    Ok(payload)
}

fn encode_messages(
    rest: &[Message],
    images: &HashMap<String, String>,
    selection: &SelectedModel,
    provider_name: &str,
    endpoint: &str,
) -> Result<Vec<Value>, ProviderError> {
    let mut encoded: Vec<Value> = Vec::new();
    let mut current_role: Option<&'static str> = None;
    let mut current_blocks: Vec<Value> = Vec::new();

    let flush_current = |encoded: &mut Vec<Value>,
                         current_role: &mut Option<&'static str>,
                         current_blocks: &mut Vec<Value>| {
        if let Some(role) = current_role.take() {
            encoded.push(json!({ "role": role, "content": std::mem::take(current_blocks) }));
        }
    };

    for message in rest {
        let next_role = match message {
            Message::Assistant { .. } => "assistant",
            Message::ToolResult { .. }
            | Message::Developer { .. }
            | Message::User { .. }
            | Message::Notification { .. } => "user",
        };
        let blocks = match message {
            Message::ToolResult { tool_call_id, is_error, .. } => {
                let mut block = json!({
                    "type": "tool_result",
                    "tool_use_id": tool_call_id,
                    "content": encode_tool_result_content(message, images)?,
                });
                if *is_error {
                    block["is_error"] = json!(true);
                }
                vec![block]
            }
            Message::Assistant { .. } => {
                let same_provider =
                    message_metadata_for(message, selection, provider_name, endpoint).is_some();
                encode_assistant_blocks(message.content(), same_provider)
            }
            Message::Developer { .. } | Message::User { .. } | Message::Notification { .. } => {
                encode_user_blocks(message, images)?
            }
        };
        if blocks.is_empty() {
            continue;
        }
        if current_role.is_some_and(|prev| prev != next_role) {
            flush_current(&mut encoded, &mut current_role, &mut current_blocks);
        }
        current_role = Some(next_role);
        current_blocks.extend(blocks);
    }
    flush_current(&mut encoded, &mut current_role, &mut current_blocks);
    Ok(encoded)
}

fn apply_rolling_message_cache_control(messages: &mut [Value]) {
    let is_cacheable = |block: &Value| match block.get("type").and_then(Value::as_str) {
        Some("text") => block.get("text").and_then(Value::as_str).is_some_and(|text| !text.is_empty()),
        Some("image" | "tool_use" | "tool_result") => true,
        _ => false,
    };
    if let Some(block) = messages
        .last_mut()
        .and_then(|message| message.get_mut("content"))
        .and_then(Value::as_array_mut)
        .and_then(|blocks| blocks.iter_mut().rev().find(|block| is_cacheable(block)))
    {
        block["cache_control"] = json!({ "type": "ephemeral" });
    }
}

fn encode_user_blocks(
    message: &Message,
    images: &HashMap<String, String>,
) -> Result<Vec<Value>, ProviderError> {
    let mut blocks = Vec::new();
    let text = message.content_text();
    if !text.is_empty() {
        blocks.push(json!({ "type": "text", "text": text }));
    }
    for image in message.images() {
        let url = image_data_url(images, &image.id)?;
        blocks.push(encode_image_block(url, &image.id)?);
    }
    Ok(blocks)
}

fn encode_tool_result_content(
    message: &Message,
    images: &HashMap<String, String>,
) -> Result<Value, ProviderError> {
    if message.images().next().is_none() {
        return Ok(json!(message.content_text()));
    }
    Ok(json!(encode_user_blocks(message, images)?))
}

fn encode_assistant_blocks(content: &[ContentBlock], same_provider: bool) -> Vec<Value> {
    let mut blocks = Vec::new();
    for block in content {
        match block {
            ContentBlock::Thinking { thinking, metadata } if same_provider => {
                if let Some(meta) = metadata
                    && meta.0.get("type").and_then(Value::as_str) == Some("redacted_thinking")
                {
                    blocks.push(Value::Object(meta.0.clone()));
                } else if !thinking.is_empty() || metadata.is_some() {
                    let mut map = metadata.as_ref().map(|meta| meta.0.clone()).unwrap_or_default();
                    map.insert("type".to_string(), json!("thinking"));
                    map.insert("thinking".to_string(), json!(thinking));
                    blocks.push(Value::Object(map));
                }
            }
            ContentBlock::Text { text, .. } if !text.is_empty() => {
                blocks.push(json!({ "type": "text", "text": text }));
            }
            ContentBlock::ToolCall(call) => {
                let input = match &call.arguments {
                    Value::Object(_) => call.arguments.clone(),
                    _ => json!({}),
                };
                blocks.push(json!({
                    "type": "tool_use",
                    "id": call.tool_call_id,
                    "name": call.tool_name,
                    "input": input,
                }));
            }
            _ => {}
        }
    }
    blocks
}

fn encode_image_block(data_url: &str, image_id: &str) -> Result<Value, ProviderError> {
    let rest = data_url.strip_prefix("data:").ok_or_else(|| {
        ProviderError::new(ModelErrorKind::Other, format!("invalid image data URL for attachment {image_id}"))
    })?;
    let (media_type, data) = rest.split_once(";base64,").ok_or_else(|| {
        ProviderError::new(
            ModelErrorKind::Other,
            format!("unsupported image encoding for attachment {image_id}"),
        )
    })?;
    Ok(json!({
        "type": "image",
        "source": {
            "type": "base64",
            "media_type": media_type,
            "data": data,
        },
    }))
}
