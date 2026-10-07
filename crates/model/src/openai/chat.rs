//! OpenAI Chat Completions 协议：请求编码、SSE 解码与响应解析。

use super::parse::*;
mod stream;
pub(crate) use self::stream::read_chat_sse_stream;

use serde_json::{Value, json};
use std::collections::BTreeMap;

use crate::config::selection::{OpenAiProviderConfig, SelectedModel};
use crate::error::{ModelErrorKind, ProviderError};
use crate::openai::wire::ThinkingWireFormat;
use crate::provider::contract::{provider_content_filter_error, provider_finish_network_error};
use crate::provider::telemetry::ProviderStreamEvent;
use crate::transport::stream::{SseStreamDecoder, provider_stream_malformed_error, read_sse_stream};
use crate::types::{
    ModelMessage, ModelRole, ModelStopReason, ModelToolCall, ModelToolSchema, ModelTurnRequest,
    ModelTurnResponse, ProviderReasoningReplay,
};
use tokio_util::sync::CancellationToken;

pub(crate) fn openai_chat_stream_request_payload(
    request: &ModelTurnRequest,
    selection: &SelectedModel,
    provider_name: &str,
) -> Value {
    let mut payload = json!({
        "model": selection.model_name,
        "messages": openai_messages(&request.messages, selection, provider_name),
        "stream": true,
        // provider 实现 OpenAI 兼容的 include_usage 扩展时会在最后一个流块带上 usage；
        // 不支持该扩展的 provider 照样返回合法响应，只是 usage_present=false。
        "stream_options": {"include_usage": true},
    });
    let reasoning = super::reasoning_wire_decision(selection);
    // 输出上限用哪个字段名由模型配置决定，本层不猜；Responses 协议用 `max_output_tokens`。
    if let Some(max_output_tokens) = request.model_preferences.max_output_tokens {
        payload[selection.chat_output_tokens_field] = json!(max_output_tokens);
    }
    if let Some(enabled) = reasoning.enabled {
        apply_thinking_wire(&mut payload, enabled, selection.thinking_wire_format);
        if enabled && let Some(wire_effort) = reasoning.effort {
            payload["reasoning_effort"] = json!(wire_effort);
        }
    }
    if !request.tools.is_empty() {
        payload["tools"] = json!(request.tools.iter().map(openai_tool_payload).collect::<Vec<_>>());
        if selection.supports_tool_choice {
            payload["tool_choice"] = serde_json::json!("auto");
        }
    }
    payload
}

/// 把思考开关落到 wire 上；具体形状由 provider 选定的 ThinkingWireFormat 决定。
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

/// 在已知的兼容字段里只取第一个非空值，避免同一段增量被显示两次。
fn chat_reasoning_text(message: &serde_json::Map<String, Value>) -> Option<(&'static str, &str)> {
    crate::types::CHAT_REASONING_FIELDS.iter().find_map(|field| {
        message
            .get(*field)
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .map(|text| (*field, text))
    })
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
        "chat_stream_malformed",
        vec![format!(
            "finish_reason={}",
            crate::error::bounded_provider_error_diagnostic(reason)
        )],
    )
}

/// 构造单条消息的 payload；私有续接材料的身份筛选见 super::reasoning_replay_for。
fn openai_message_payload_with_reasoning(
    message: &ModelMessage,
    selection: &SelectedModel,
    provider_name: &str,
) -> Value {
    let role = if message.role == ModelRole::Developer && !selection.supports_developer_role {
        json!("system")
    } else {
        json!(message.role)
    };
    let mut content = openai_message_content(message);
    // 该端点的工具调用消息必须带 content 字段，用空串补齐。
    if selection.requires_assistant_content_for_tool_calls && content.is_null() {
        content = json!("");
    }
    let mut payload = json!({
        "role": role,
        "content": content,
    });
    if let Some(tool_call_id) = &message.tool_call_id {
        payload["tool_call_id"] = json!(tool_call_id);
    }
    if !message.tool_calls.is_empty() {
        payload["tool_calls"] =
            json!(message.tool_calls.iter().map(openai_tool_call_payload).collect::<Vec<_>>());
    }
    if let Some(ProviderReasoningReplay::Chat {
        reasoning_content,
        reasoning_field,
        reasoning_details,
        ..
    }) = super::reasoning_replay_for(message, selection, provider_name)
    {
        // 续接材料按形态择一发送：有 details 时不再重复发思考文本。
        if !reasoning_details.is_empty() {
            payload["reasoning_details"] = json!(reasoning_details);
        } else if !reasoning_content.is_empty() {
            payload[reasoning_field] = json!(reasoning_content);
        }
    }
    payload
}

fn openai_message_content(message: &ModelMessage) -> Value {
    let text = &message.content;
    if message.role == ModelRole::Assistant && !message.tool_calls.is_empty() && text.is_empty() {
        Value::Null
    } else if message.images.is_empty() || message.role == ModelRole::Tool {
        json!(text)
    } else {
        let mut content = vec![json!({"type": "text", "text": text})];
        content.extend(message.images.iter().map(|image| {
            json!({
                "type": "image_url", "image_url": {"url": image}
            })
        }));
        json!(content)
    }
}

// Chat 只接受文本工具结果；整批结果闭合后才发送视觉消息，避免拆开并行调用的配对。
fn openai_messages(messages: &[ModelMessage], selection: &SelectedModel, provider_name: &str) -> Vec<Value> {
    let mut result = Vec::new();
    let mut images = Vec::new();
    for message in messages {
        if message.role != ModelRole::Tool && !images.is_empty() {
            result.push(json!({"role": "user", "content": std::mem::take(&mut images)}));
        }
        result.push(openai_message_payload_with_reasoning(message, selection, provider_name));
        if message.role == ModelRole::Tool {
            for image in &message.images {
                images.push(json!({"type": "text", "text": format!("Image from tool call {}:", message.tool_call_id.as_deref().expect("tool result has its call ID"))}));
                images.push(json!({"type": "image_url", "image_url": {"url": image}}));
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
