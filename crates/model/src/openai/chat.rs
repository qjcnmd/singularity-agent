//! OpenAI Chat Completions 协议请求序列化与响应解析。

use super::parse::*;
use serde_json::{Value, json};

use crate::error::ProviderError;
use crate::provider::contract::{
    ThinkingWireFormat, provider_content_filter_error, provider_finish_network_error,
};
use crate::provider::runtime::{OpenAiProviderConfig, SelectedModel};
use crate::types::{
    ModelMessage, ModelRole, ModelStopReason, ModelToolCall, ModelToolSchema, ModelTurnRequest,
    ModelTurnResponse, ProviderReasoningReplay,
};

pub fn openai_chat_stream_request_payload(
    request: &ModelTurnRequest,
    selection: &SelectedModel,
) -> Value {
    let mut payload = json!({
        "model": selection.model_name,
        "messages": request
            .messages
            .iter()
            .map(|message| {
                openai_message_payload_with_reasoning(
                    message,
                    selection.supports_developer_role,
                    selection.requires_assistant_content_for_tool_calls,
                )
            })
            .collect::<Vec<_>>(),
        "stream": true,
        // provider 实现 OpenAI 兼容 include_usage 扩展时，在最终流块中请求
        // usage；不支持的 provider 仍产生合法响应（usage_present=false）。
        "stream_options": {"include_usage": true},
    });
    let reasoning = super::reasoning_wire_decision(selection);
    // 输出上限 wire 字段取舍：chat completions 走 max_tokens（第三方兼容
    // 端点如 DeepSeek/dashscope 接受），responses 走 max_output_tokens
    // （OpenAI 官方 Responses API 命名）。官方 chat 对推理系模型要求
    // max_completion_tokens，本层不针对推理模型切换字段；推理模型经
    // chat 兼容端点使用时若需输出上限，由用户在配置中显式声明。
    if let Some(max_output_tokens) = request.model_preferences.max_output_tokens {
        payload["max_tokens"] = json!(max_output_tokens);
    }
    if let Some(enabled) = reasoning.enabled {
        apply_thinking_wire(&mut payload, enabled, selection.thinking_wire_format);
        if enabled && let Some(wire_effort) = reasoning.effort {
            payload["reasoning_effort"] = json!(wire_effort);
        }
    }
    if !request.tools.is_empty() {
        payload["tools"] = json!(
            request
                .tools
                .iter()
                .map(openai_tool_payload)
                .collect::<Vec<_>>()
        );
        if selection.supports_tool_choice {
            payload["tool_choice"] = serde_json::json!("auto");
        }
    }
    payload
}

/// Thinking 开关的协议落点统一投影：同一个语义开关按 provider 的 wire
/// 偏好落到 thinking 或 enable_thinking 字段。
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

/// 已知兼容字段只取首个非空值，避免同一增量重复显示。
pub(crate) fn chat_reasoning_text(
    message: &serde_json::Map<String, Value>,
) -> Option<(&'static str, &str)> {
    crate::types::CHAT_REASONING_FIELDS
        .iter()
        .find_map(|field| {
            message
                .get(*field)
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
                .map(|text| (*field, text))
        })
}

pub(crate) fn chat_reasoning_detail_text(detail: &Value) -> Option<&str> {
    detail
        .get(chat_reasoning_detail_text_field(detail)?)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
}

pub(crate) fn chat_reasoning_detail_text_field(detail: &Value) -> Option<&'static str> {
    match detail.get("type").and_then(Value::as_str)? {
        "reasoning.text" => Some("text"),
        "reasoning.summary" => Some("summary"),
        _ => None,
    }
}

pub(crate) struct ChatResponseParts {
    pub content: String,
    pub tool_calls: Vec<ModelToolCall>,
    pub reasoning_content: String,
    pub reasoning_field: String,
    pub reasoning_details: Vec<Value>,
    pub finish_reason: Option<String>,
    pub usage: crate::ModelUsage,
}

pub(crate) fn finish_chat_response(
    request: &ModelTurnRequest,
    config: &OpenAiProviderConfig,
    model_name: &str,
    reasoning_effort: Option<&str>,
    parts: ChatResponseParts,
) -> Result<ModelTurnResponse, ProviderError> {
    let ChatResponseParts {
        content,
        tool_calls,
        reasoning_content,
        reasoning_field,
        reasoning_details,
        finish_reason,
        usage,
    } = parts;
    if finish_reason.as_deref() == Some("content_filter") {
        return Err(provider_content_filter_error(
            "provider Chat response was stopped by content filter",
        ));
    }
    if finish_reason.as_deref() == Some("network_error") {
        return Err(provider_finish_network_error(
            "provider Chat response reported a network error",
        ));
    }
    let thinking = if !reasoning_content.is_empty() {
        reasoning_content.clone()
    } else {
        reasoning_details
            .iter()
            .filter_map(chat_reasoning_detail_text)
            .collect::<Vec<_>>()
            .join("")
    };
    let replay = if !reasoning_content.is_empty() || !reasoning_details.is_empty() {
        Some(ProviderReasoningReplay::Chat {
            provider_name: config.provider_name.clone(),
            model_name: model_name.to_string(),
            reasoning_effort: reasoning_effort.map(str::to_string),
            tool_call_ids: tool_calls
                .iter()
                .map(|call| call.tool_call_id.clone())
                .collect(),
            reasoning_content,
            reasoning_field,
            reasoning_details,
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
            usage,
            stop_reason: match finish_reason.as_deref() {
                Some("length") => Some(ModelStopReason::Length),
                Some("stop" | "tool_calls" | "function_call") => Some(ModelStopReason::Stop),
                _ => None,
            },
        },
    )
}

fn openai_message_payload_with_reasoning(
    message: &ModelMessage,
    supports_developer_role: bool,
    requires_assistant_content_for_tool_calls: bool,
) -> Value {
    let role = if message.role == ModelRole::Developer && !supports_developer_role {
        &ModelRole::System
    } else {
        &message.role
    };
    let mut content = openai_message_content(message);
    if message.role == ModelRole::Assistant
        && !message.tool_calls.is_empty()
        && requires_assistant_content_for_tool_calls
        && content.is_null()
    {
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
        payload["tool_calls"] = json!(
            message
                .tool_calls
                .iter()
                .map(openai_tool_call_payload)
                .collect::<Vec<_>>()
        );
    }
    if let Some(ProviderReasoningReplay::Chat {
        reasoning_content,
        reasoning_field,
        reasoning_details,
        ..
    }) = message.provider_reasoning_replay.as_ref()
    {
        if !reasoning_details.is_empty() {
            payload["reasoning_details"] = json!(reasoning_details);
        } else if !reasoning_content.is_empty() {
            payload[reasoning_field] = json!(reasoning_content);
        }
    }
    payload
}

pub fn openai_message_content(message: &ModelMessage) -> Value {
    let text = &message.content;
    if message.role == ModelRole::Assistant && !message.tool_calls.is_empty() && text.is_empty() {
        Value::Null
    } else {
        json!(text)
    }
}

pub fn openai_tool_call_payload(tool_call: &ModelToolCall) -> Value {
    json!({
        "id": tool_call.tool_call_id,
        "type": "function",
        "function": {
            "name": tool_call.tool_name,
            "arguments": tool_call.raw_arguments,
        }
    })
}

pub fn openai_tool_payload(tool: &ModelToolSchema) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": tool.name,
            "description": tool.description,
            "parameters": tool.parameters_schema,
        }
    })
}
