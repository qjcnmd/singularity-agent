//! Messages 协议（Anthropic 兼容 / DeepSeek Messages）：请求编码、前缀哈希绑定与 SSE 流解码。

#[cfg(test)]
pub(crate) mod stream;
#[cfg(not(test))]
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
    pub(crate) effective_reasoning: Option<String>,
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
    let (messages, final_digest) =
        encode_messages(rest, &request.images, selection, provider_name, endpoint, initial_digest)?;

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
    let mut effective_reasoning = selection.reasoning_variant.clone();
    match reasoning.enabled {
        Some(true) => {
            if trailing_tool_turn_lacks_thinking(&messages) {
                payload["thinking"] = json!({ "type": "disabled" });
                effective_reasoning = Some("off".to_string());
            } else {
                payload["thinking"] = json!({ "type": "adaptive" });
                if let Some(effort) = reasoning.effort {
                    payload["output_config"] = json!({ "effort": effort });
                }
            }
        }
        Some(false) => {
            payload["thinking"] = json!({ "type": "disabled" });
        }
        None => {}
    }
    Ok(EncodedMessagesRequest {
        payload,
        prefix_digest: final_digest,
        effective_reasoning,
    })
}

fn trailing_tool_turn_lacks_thinking(encoded: &[Value]) -> bool {
    if encoded.len() < 2 {
        return false;
    }
    let last = &encoded[encoded.len() - 1];
    let prev = &encoded[encoded.len() - 2];
    if last.get("role").and_then(Value::as_str) != Some("user")
        || prev.get("role").and_then(Value::as_str) != Some("assistant")
    {
        return false;
    }
    let has_tool_result = last.get("content").and_then(Value::as_array).is_some_and(|blocks| {
        blocks.iter().any(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))
    });
    if !has_tool_result {
        return false;
    }
    let starts_with_thinking = prev
        .get("content")
        .and_then(Value::as_array)
        .and_then(|blocks| blocks.first())
        .and_then(|block| block.get("type"))
        .and_then(Value::as_str)
        .is_some_and(|kind| matches!(kind, "thinking" | "redacted_thinking"));
    !starts_with_thinking
}

fn encode_messages(
    rest: &[Message],
    images: &HashMap<String, String>,
    selection: &SelectedModel,
    provider_name: &str,
    endpoint: &str,
    mut digest: String,
) -> Result<(Vec<Value>, String), ProviderError> {
    let mut encoded: Vec<Value> = Vec::new();
    let mut current_role: Option<&'static str> = None;
    let mut current_blocks: Vec<Value> = Vec::new();

    let flush_current = |encoded: &mut Vec<Value>,
                         digest: &mut String,
                         current_role: &mut Option<&'static str>,
                         current_blocks: &mut Vec<Value>| {
        if let Some(role) = current_role.take() {
            let msg = json!({ "role": role, "content": std::mem::take(current_blocks) });
            *digest = chain_digest(digest, &msg);
            encoded.push(msg);
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
                encode_user_blocks(message, images)?
            }
        };
        if blocks.is_empty() {
            continue;
        }
        // Messages 协议要求 user / assistant 严格交替；确认本条非空后再切换角色并合并连续同角色。
        if let Some((msg, next_digest)) = pending_user_flush {
            encoded.push(msg);
            digest = next_digest;
            current_blocks.clear();
        } else if current_role.is_some_and(|prev| prev != next_role) {
            flush_current(&mut encoded, &mut digest, &mut current_role, &mut current_blocks);
        }
        current_role = Some(next_role);
        current_blocks.extend(blocks);
    }
    flush_current(&mut encoded, &mut digest, &mut current_role, &mut current_blocks);
    Ok((encoded, digest))
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
) -> Vec<Value> {
    let mut blocks = Vec::new();
    for block in content {
        match block {
            ContentBlock::Thinking { thinking, metadata } if same_provider => {
                if is_signed_thinking(metadata.as_ref()) && !prefix_matches {
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

fn chain_digest(previous: &str, value: &Value) -> String {
    let serialized = serde_json::to_vec(value).unwrap_or_default();
    sha256_hex(&[previous.as_bytes(), b"\0", &serialized])
}

/// 纯 Rust FIPS 180-4 SHA-256，用于 Messages 前缀哈希绑定，不引入额外依赖。
fn sha256_hex(parts: &[&[u8]]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
        0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
        0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
        0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
        0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
        0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
        0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
    ];
    let total_len: usize = parts.iter().map(|p| p.len()).sum();
    let mut buf = Vec::with_capacity(total_len + 72);
    for part in parts {
        buf.extend_from_slice(part);
    }
    buf.push(0x80);
    while buf.len() % 64 != 56 {
        buf.push(0);
    }
    buf.extend_from_slice(&((total_len as u64).wrapping_mul(8)).to_be_bytes());

    for chunk in buf.chunks_exact(64) {
        let mut w = [0u32; 64];
        for (idx, word) in chunk.chunks_exact(4).enumerate() {
            w[idx] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = hh.wrapping_add(s1).wrapping_add(ch).wrapping_add(K[i]).wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (slot, delta) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
            *slot = slot.wrapping_add(delta);
        }
    }
    let mut out = String::with_capacity(64);
    for word in h {
        let _ = write!(out, "{word:08x}");
    }
    out
}
