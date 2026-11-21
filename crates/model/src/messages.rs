//! Messages 协议（Anthropic 兼容 / DeepSeek Messages）：请求编码与 SSE 流解码。

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
    let (system, messages) =
        encode_messages(&request.messages, &request.images, selection, provider_name, endpoint)?;
    let mut payload = json!({
        "model": selection.model_name,
        "max_tokens": request.max_output_tokens,
        "stream": true,
        "messages": messages,
    });
    if let Some(system) = system {
        payload["system"] = json!([{
            "type": "text",
            "text": system,
            "cache_control": { "type": "ephemeral" },
        }]);
    }
    if !request.tools.is_empty() {
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
        payload["tools"] = json!(tools);
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
    messages: &[Message],
    images: &HashMap<String, String>,
    selection: &SelectedModel,
    provider_name: &str,
    endpoint: &str,
) -> Result<(Option<String>, Vec<Value>), ProviderError> {
    let (system, rest) = split_leading_instructions(messages);

    let mut grouped: Vec<(&'static str, Vec<Value>)> = Vec::new();
    for message in rest {
        let (role, blocks) = match message {
            Message::ToolResult { tool_call_id, is_error, .. } => {
                let mut block = json!({
                    "type": "tool_result",
                    "tool_use_id": tool_call_id,
                    "content": encode_tool_result_content(message, images)?,
                });
                if *is_error {
                    block["is_error"] = json!(true);
                }
                ("user", vec![block])
            }
            Message::Assistant { .. } => {
                let same_provider =
                    message_metadata_for(message, selection, provider_name, endpoint).is_some();
                ("assistant", encode_assistant_blocks(message.content(), same_provider))
            }
            Message::Developer { .. } | Message::User { .. } | Message::Notification { .. } => {
                ("user", encode_user_blocks(message, images)?)
            }
        };
        if blocks.is_empty() {
            continue;
        }
        // Messages 协议要求 user / assistant 严格交替；连续同角色（如并发工具结果与后续用户输入）合并到同一消息。
        if let Some((prev_role, prev_blocks)) = grouped.last_mut()
            && *prev_role == role
        {
            prev_blocks.extend(blocks);
        } else {
            grouped.push((role, blocks));
        }
    }

    let encoded = grouped
        .into_iter()
        .map(|(role, content)| json!({ "role": role, "content": content }))
        .collect();
    Ok((system, encoded))
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
