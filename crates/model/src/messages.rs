mod stream;
pub(crate) use self::stream::read_messages_sse_stream;

use std::collections::HashMap;
use std::fmt::Write as _;

use serde_json::{Value, json};

use crate::config::selection::SelectedModel;
use crate::error::{ModelErrorKind, ProviderError};
use crate::provider::{
    image_data_url, message_metadata_for, reasoning_wire_decision, split_leading_instructions,
};
use crate::types::{ContentBlock, Message, ModelTurnRequest, ProviderBlockMetadata};

const PREFIX_DIGEST_SEED: &str = "singularity-messages-prefix/v1";

pub(crate) struct EncodedMessagesRequest {
    pub(crate) payload: Value,
    pub(crate) prefix_digest: String,
}

pub(crate) fn messages_stream_request_payload(
    request: &ModelTurnRequest,
    selection: &SelectedModel,
    provider_name: &str,
    endpoint: &str,
) -> Result<EncodedMessagesRequest, ProviderError> {
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
    let initial_digest = chain_digest(
        PREFIX_DIGEST_SEED,
        &json!({
            "system": system_json.as_ref().unwrap_or(&Value::Null),
            "tools": tools_json.as_ref().unwrap_or(&Value::Null),
        }),
    );
    let (mut messages, final_digest, omit_thinking) =
        encode_messages(rest, &request.images, selection, provider_name, endpoint, initial_digest)?;
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
        Some(true) if !omit_thinking => {
            payload["thinking"] = json!({ "type": "adaptive" });
            if let Some(effort) = reasoning.effort {
                payload["output_config"] = json!({ "effort": effort });
            }
        }
        Some(false) => {
            payload["thinking"] = json!({ "type": "disabled" });
        }
        Some(true) | None => {}
    }
    Ok(EncodedMessagesRequest { payload, prefix_digest: final_digest })
}

fn encode_messages(
    rest: &[Message],
    images: &HashMap<String, String>,
    selection: &SelectedModel,
    provider_name: &str,
    endpoint: &str,
    mut digest: String,
) -> Result<(Vec<Value>, String, bool), ProviderError> {
    let mut encoded: Vec<Value> = Vec::new();
    let mut current_role: Option<&'static str> = None;
    let mut current_blocks: Vec<Value> = Vec::new();
    let mut stripped_tail = (false, false);
    let mut current_stripped = false;
    let mut pending_assistant_stripped = false;

    let flush_current = |encoded: &mut Vec<Value>,
                         digest: &mut String,
                         current_role: &mut Option<&'static str>,
                         current_blocks: &mut Vec<Value>,
                         stripped_tail: &mut (bool, bool),
                         current_stripped: &mut bool| {
        if let Some(role) = current_role.take() {
            let msg = json!({ "role": role, "content": std::mem::take(current_blocks) });
            *digest = chain_digest(digest, &msg);
            encoded.push(msg);
            *stripped_tail = (stripped_tail.1, std::mem::take(current_stripped));
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
        let mut pending_user_flush = None;
        let (blocks, stripped_signed) = match message {
            Message::ToolResult { tool_call_id, is_error, .. } => {
                let mut block = json!({
                    "type": "tool_result",
                    "tool_use_id": tool_call_id,
                    "content": encode_tool_result_content(message, images)?,
                });
                if *is_error {
                    block["is_error"] = json!(true);
                }
                (vec![block], false)
            }
            Message::Assistant { .. } => {
                let meta = message_metadata_for(message, selection, provider_name, endpoint);
                let same_provider = meta.is_some();
                let prefix_digest = if current_role == Some("user") {
                    let msg = json!({ "role": "user", "content": &current_blocks });
                    let next_digest = chain_digest(&digest, &msg);
                    pending_user_flush = Some((msg, next_digest));
                    pending_user_flush.as_ref().expect("pending user flush was just set").1.as_str()
                } else {
                    digest.as_str()
                };
                let prefix_matches = meta.and_then(|m| m.prefix_digest.as_deref()) == Some(prefix_digest);
                encode_assistant_blocks(message.content(), same_provider, prefix_matches)
            }
            Message::Developer { .. } | Message::User { .. } | Message::Notification { .. } => {
                (encode_user_blocks(message, images)?, false)
            }
        };
        if blocks.is_empty() {
            pending_assistant_stripped |= stripped_signed;
            continue;
        }
        if let Some((msg, next_digest)) = pending_user_flush {
            encoded.push(msg);
            stripped_tail = (stripped_tail.1, false);
            digest = next_digest;
            current_blocks.clear();
            current_stripped = false;
        } else if current_role.is_some_and(|prev| prev != next_role) {
            flush_current(
                &mut encoded,
                &mut digest,
                &mut current_role,
                &mut current_blocks,
                &mut stripped_tail,
                &mut current_stripped,
            );
        }
        current_role = Some(next_role);
        current_blocks.extend(blocks);
        if next_role == "assistant" {
            current_stripped |= stripped_signed | std::mem::take(&mut pending_assistant_stripped);
        } else {
            pending_assistant_stripped = false;
        }
    }
    flush_current(
        &mut encoded,
        &mut digest,
        &mut current_role,
        &mut current_blocks,
        &mut stripped_tail,
        &mut current_stripped,
    );
    let omit_thinking = trailing_tool_turn_stripped_signed_thinking(&encoded, stripped_tail.0);
    Ok((encoded, digest, omit_thinking))
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

fn trailing_tool_turn_stripped_signed_thinking(encoded: &[Value], prev_stripped: bool) -> bool {
    let [.., prev, last] = encoded else {
        return false;
    };
    if !prev_stripped
        || last.get("role").and_then(Value::as_str) != Some("user")
        || prev.get("role").and_then(Value::as_str) != Some("assistant")
    {
        return false;
    }
    let (Some(last_blocks), Some(prev_blocks)) = (
        last.get("content").and_then(Value::as_array),
        prev.get("content").and_then(Value::as_array),
    ) else {
        return false;
    };
    let has_type = |blocks: &[Value], kind: &str| {
        blocks.iter().any(|b| b.get("type").and_then(Value::as_str) == Some(kind))
    };
    let starts_with_thinking = prev_blocks
        .first()
        .and_then(|block| block.get("type").and_then(Value::as_str))
        .is_some_and(|kind| matches!(kind, "thinking" | "redacted_thinking"));
    has_type(last_blocks, "tool_result") && has_type(prev_blocks, "tool_use") && !starts_with_thinking
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

fn is_signed_thinking(metadata: Option<&ProviderBlockMetadata>) -> bool {
    let Some(meta) = metadata else {
        return false;
    };
    meta.0.get("type").and_then(Value::as_str) == Some("redacted_thinking")
        || meta.0.get("signature").and_then(Value::as_str).is_some_and(|sig| !sig.is_empty())
}

fn encode_assistant_blocks(
    content: &[ContentBlock],
    same_provider: bool,
    prefix_matches: bool,
) -> (Vec<Value>, bool) {
    let mut blocks = Vec::new();
    let mut stripped_signed = false;
    for block in content {
        match block {
            ContentBlock::Thinking { thinking, metadata } if same_provider => {
                if is_signed_thinking(metadata.as_ref()) && !prefix_matches {
                    stripped_signed = true;
                    continue;
                }
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
    (blocks, stripped_signed)
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

fn chain_digest(previous: &str, value: &Value) -> String {
    let serialized = serde_json::to_vec(value).expect("prefix digest input is a JSON value");
    sha256_hex(&[previous.as_bytes(), b"\0", &serialized])
}

fn sha256_hex(parts: &[&[u8]]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part);
    }
    let mut out = String::with_capacity(64);
    for byte in hasher.finalize() {
        let _ = write!(out, "{byte:02x}");
    }
    out
}
