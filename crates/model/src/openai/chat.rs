//! OpenAI Chat Completions 协议：请求编码、SSE 解码与响应解析。

use super::parse::*;
mod stream;
pub(crate) use self::stream::read_chat_sse_stream;

use serde_json::{Value, json};
use std::collections::BTreeMap;

use crate::config::selection::{OpenAiProviderConfig, SelectedModel};
use crate::error::{ModelErrorKind, ProviderError};
use crate::openai::wire::ThinkingWireFormat;
use crate::provider::ProviderStreamEvent;
use crate::provider::contract::{provider_content_filter_error, provider_finish_network_error};
use crate::transport::stream::{SseStreamDecoder, provider_stream_malformed_error, read_sse_stream};
use crate::types::{
    Message, ModelRole, ModelStopReason, ModelToolCall, ModelToolSchema, ModelTurnRequest, ModelTurnResponse,
    ProviderMessageMetadata,
};
use tokio_util::sync::CancellationToken;

pub(crate) fn openai_chat_stream_request_payload(
    request: &ModelTurnRequest,
    selection: &SelectedModel,
    provider_name: &str,
    endpoint: &str,
) -> Value {
    let mut payload = json!({
        "model": selection.model_name,
        "messages": openai_messages(&request.messages, &request.images, selection, provider_name, endpoint),
        "stream": true,
        // provider 实现 OpenAI 兼容的 include_usage 扩展时会在最后一个流块带上 usage；
        // 不支持该扩展的 provider 照样返回合法响应，用量留空。
        "stream_options": {"include_usage": true},
    });
    let reasoning = super::reasoning_wire_decision(selection);
    // 输出上限用哪个字段名由模型配置决定，本层不猜；Responses 协议用 `max_output_tokens`。
    payload[selection.chat_output_tokens_field] = json!(request.max_output_tokens);
    if let Some(enabled) = reasoning.enabled {
        apply_thinking_wire(&mut payload, enabled, selection.thinking_wire_format);
        if enabled && let Some(wire_effort) = reasoning.effort {
            payload["reasoning_effort"] = json!(wire_effort);
        }
    }
    if !request.tools.is_empty() {
        payload["tools"] = json!(request.tools.iter().map(openai_tool_payload).collect::<Vec<_>>());
    }
    payload
}

/// 按模型配置的 ThinkingWireFormat 写入思考开关字段。
fn apply_thinking_wire(payload: &mut Value, enabled: bool, wire_format: ThinkingWireFormat) {
    match wire_format {
        ThinkingWireFormat::ThinkingType => {
            payload["thinking"] = json!({"type": if enabled { "enabled" } else { "disabled" }});
        }
        ThinkingWireFormat::EnableThinking => {
            payload["enable_thinking"] = json!(enabled);
        }
        ThinkingWireFormat::ReasoningEffort => {
            if !enabled {
                payload["reasoning_effort"] = json!("none");
            }
        }
    }
}

/// 优先取第一个非空字段；全部为空时仍保留字段，供下一次请求原样续接。
fn chat_reasoning_text(message: &serde_json::Map<String, Value>) -> Option<(&'static str, &str)> {
    let mut fields = crate::types::CHAT_REASONING_FIELDS
        .iter()
        .filter_map(|field| message.get(*field).and_then(Value::as_str).map(|text| (*field, text)));
    fields.clone().find(|(_, text)| !text.is_empty()).or_else(|| fields.next())
}

fn chat_reasoning_detail_text(detail: &serde_json::Map<String, Value>) -> Option<&str> {
    detail
        .get(chat_reasoning_detail_text_field(detail)?)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
}

fn chat_reasoning_detail_text_field(detail: &serde_json::Map<String, Value>) -> Option<&'static str> {
    match detail.get("type").and_then(Value::as_str)? {
        "reasoning.text" => Some("text"),
        "reasoning.summary" => Some("summary"),
        _ => None,
    }
}

/// 未识别的 finish_reason：保留截断后的原始词形，不猜它的含义。
fn provider_chat_finish_reason_unsupported(reason: &str) -> ProviderError {
    ProviderError::diagnostic(
        ModelErrorKind::JsonSchemaViolation,
        "provider Chat response reported an unsupported finish reason",
        vec![format!(
            "finish_reason={}",
            crate::error::bounded_provider_error_diagnostic(reason)
        )],
    )
}

/// 编码一条消息；是否附带提供方私有的思考数据见 super::message_metadata_for。
fn openai_message_payload_with_reasoning(
    message: &Message,
    images: &std::collections::HashMap<String, String>,
    selection: &SelectedModel,
    provider_name: &str,
    endpoint: &str,
) -> Value {
    let role = if message.role() == ModelRole::Developer {
        json!("system")
    } else {
        json!(message.role())
    };
    let content = openai_message_content(message, images);
    let mut payload = json!({
        "role": role,
        "content": content,
    });
    if let Some(tool_call_id) = message.tool_call_id() {
        payload["tool_call_id"] = json!(tool_call_id);
    }
    if message.tool_calls().next().is_some() {
        payload["tool_calls"] = json!(message.tool_calls().map(openai_tool_call_payload).collect::<Vec<_>>());
    }
    if let Some(ProviderMessageMetadata::Chat { reasoning_field, reasoning_details, .. }) =
        super::message_metadata_for(message, selection, provider_name, endpoint)
    {
        // 有 reasoning_details 时只发结构化片段，避免和思考文本重复。
        if !reasoning_details.is_empty() {
            payload["reasoning_details"] = json!(reasoning_details);
        } else {
            let thinking: String = message
                .content()
                .iter()
                .filter_map(|block| match block {
                    crate::ContentBlock::Thinking { thinking, .. } => Some(thinking.as_str()),
                    _ => None,
                })
                .collect();
            payload[reasoning_field] = json!(thinking);
        }
    }
    payload
}

fn openai_message_content(message: &Message, images: &std::collections::HashMap<String, String>) -> Value {
    let text = message.content_text();
    if message.images().next().is_none() || message.role() == ModelRole::Tool {
        json!(text)
    } else {
        let mut content = vec![json!({"type": "text", "text": text})];
        content.extend(message.images().map(|image| {
            json!({
                "type": "image_url", "image_url": {"url": images[&image.id]}
            })
        }));
        json!(content)
    }
}

// Chat 的工具结果只能是文本；工具结果里的图片放到这批结果之后的一条用户消息里，
// 避免插在一组并行调用的结果中间。
fn openai_messages(
    messages: &[Message],
    images_by_id: &std::collections::HashMap<String, String>,
    selection: &SelectedModel,
    provider_name: &str,
    endpoint: &str,
) -> Vec<Value> {
    let mut result = Vec::new();
    let mut images = Vec::new();
    for message in messages {
        if message.role() != ModelRole::Tool && !images.is_empty() {
            result.push(json!({"role": "user", "content": std::mem::take(&mut images)}));
        }
        result.push(openai_message_payload_with_reasoning(
            message,
            images_by_id,
            selection,
            provider_name,
            endpoint,
        ));
        if message.role() == ModelRole::Tool {
            for image in message.images() {
                images.push(json!({"type": "text", "text": format!("Image from tool call {}:", message.tool_call_id().expect("tool result has its call ID"))}));
                images.push(json!({"type": "image_url", "image_url": {"url": images_by_id[&image.id]}}));
            }
        }
    }
    if !images.is_empty() {
        result.push(json!({"role": "user", "content": images}));
    }
    result
}

fn openai_tool_call_payload(tool_call: &ModelToolCall) -> Value {
    json!({
        "id": tool_call.tool_call_id,
        "type": "function",
        "function": {
            "name": tool_call.tool_name,
            "arguments": tool_call.arguments.to_string(),
        }
    })
}

fn openai_tool_payload(tool: &ModelToolSchema) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": tool.name,
            "description": tool.description,
            "parameters": tool.parameters_schema,
        }
    })
}
