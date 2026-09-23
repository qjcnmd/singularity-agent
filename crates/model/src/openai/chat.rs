//! OpenAI Chat Completions 协议：请求编码、SSE 解码与响应解析。

use super::parse::*;
use serde_json::{Value, json};
use std::collections::BTreeMap;

use crate::config::selection::{OpenAiProviderConfig, SelectedModel};
use crate::error::{ModelErrorKind, ProviderError};
use crate::openai::wire::ThinkingWireFormat;
use crate::provider::contract::{provider_content_filter_error, provider_finish_network_error};
use crate::provider::telemetry::ProviderStreamEvent;
use crate::transport::stream::{
    SseFrame, SseFrameDecoder, SseStreamDecoder, provider_stream_malformed_error, read_sse_stream,
};
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
        "messages": request
            .messages
            .iter()
            .map(|message| openai_message_payload_with_reasoning(message, selection, provider_name))
            .collect::<Vec<_>>(),
        "stream": true,
        // provider 实现 OpenAI 兼容的 include_usage 扩展时会在最后一个流块带上 usage；
        // 不支持该扩展的 provider 照样返回合法响应，只是 usage_present=false。
        "stream_options": {"include_usage": true},
    });
    let reasoning = super::reasoning_wire_decision(selection);
    // 输出上限用哪个字段名由模型配置决定，本层不猜；Responses 协议用 `max_output_tokens`。
    if let Some(max_output_tokens) = request.model_preferences.max_output_tokens {
        payload[selection.chat_output_tokens_field.as_str()] = json!(max_output_tokens);
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

fn chat_reasoning_detail_text(detail: &serde_json::Map<String, Value>) -> Option<&str> {
    detail
        .get(chat_reasoning_detail_text_field(detail)?)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
}

fn chat_reasoning_detail_text_field(
    detail: &serde_json::Map<String, Value>,
) -> Option<&'static str> {
    match detail.get("type").and_then(Value::as_str)? {
        "reasoning.text" => Some("text"),
        "reasoning.summary" => Some("summary"),
        _ => None,
    }
}

struct ChatResponseParts {
    pub content: String,
    pub tool_calls: Vec<ModelToolCall>,
    pub reasoning_content: String,
    pub reasoning_field: String,
    pub reasoning_details: Vec<Value>,
    /// 停止原因在终态物化时已校验过必须存在：原始流里可以缺失，这个中间对象不行。
    pub finish_reason: String,
    pub usage: crate::ModelUsage,
}

fn finish_chat_response(
    config: &OpenAiProviderConfig,
    model_name: &str,
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
    if finish_reason == "content_filter" {
        return Err(provider_content_filter_error(
            "provider Chat response was stopped by content filter",
        ));
    }
    if finish_reason == "network_error" {
        return Err(provider_finish_network_error(
            "provider Chat response reported a network error",
        ));
    }
    let thinking = if !reasoning_content.is_empty() {
        reasoning_content.clone()
    } else {
        reasoning_details
            .iter()
            .filter_map(|detail| detail.as_object().and_then(chat_reasoning_detail_text))
            .collect::<Vec<_>>()
            .join("")
    };
    // 未识别的 finish_reason 不能当成「没有停止原因」：宿主无法判断这是正常完成、截断
    // 还是出错，所以一律按协议失败结束，绝不走进正常完成或工具执行路径。
    let stop_reason = match finish_reason.as_str() {
        "length" => Some(ModelStopReason::Length),
        "stop" | "tool_calls" | "function_call" => Some(ModelStopReason::Stop),
        unknown => return Err(provider_chat_finish_reason_unsupported(unknown)),
    };
    let replay = if !reasoning_content.is_empty() || !reasoning_details.is_empty() {
        Some(ProviderReasoningReplay::Chat {
            provider_name: config.provider_name.clone(),
            model_name: model_name.to_string(),
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
    finalize_provider_response(ModelTurnResponse {
        assistant_message: ModelMessage {
            tool_calls,
            provider_reasoning_replay: replay,
            ..ModelMessage::text(ModelRole::Assistant, content)
        },
        thinking,
        usage,
        stop_reason,
    })
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
        &ModelRole::System
    } else {
        &message.role
    };
    let mut content = openai_message_content(message);
    // 该端点的工具调用消息必须带 content 字段，用空串补齐。
    if message.role == ModelRole::Assistant
        && !message.tool_calls.is_empty()
        && selection.requires_assistant_content_for_tool_calls
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
    } else {
        json!(text)
    }
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

/// 用 Chat 协议解码一次真实响应：共享读取循环驱动本协议解码器，终结也在本模块内。
pub(crate) fn read_chat_sse_stream(
    runtime: &tokio::runtime::Handle,
    cancellation: &CancellationToken,
    response: reqwest::Response,
    on_event: &mut dyn FnMut(ProviderStreamEvent),
    config: &OpenAiProviderConfig,
    selection: &SelectedModel,
) -> Result<ModelTurnResponse, ProviderError> {
    let parts = read_sse_stream(
        runtime,
        cancellation,
        response,
        ChatSseDecoder::new(on_event),
    )?;
    finish_chat_response(config, &selection.model_name, parts)
}

#[derive(Default)]
struct ChatToolAccumulator {
    id: String,
    name: String,
    arguments: String,
}

/// 增量解析、总字节有上限的 Chat SSE 解码器。公开正文与公开 reasoning 文本按增量发出；
/// 未拼完的工具参数和 provider 私有续接材料只在最后物化规范化响应时一次性产出。
struct ChatSseDecoder<'a> {
    frames: SseFrameDecoder,
    content: String,
    reasoning_content: String,
    reasoning_field: Option<String>,
    reasoning_details: Vec<Value>,
    tool_calls: BTreeMap<u64, ChatToolAccumulator>,
    finish_reason: Option<String>,
    usage: Option<Value>,
    saw_choice: bool,
    done: bool,
    on_event: &'a mut dyn FnMut(ProviderStreamEvent),
}

impl SseStreamDecoder for ChatSseDecoder<'_> {
    type Terminal = ChatResponseParts;
    fn frame_malformed() -> fn(&'static str) -> ProviderError {
        provider_chat_stream_malformed_error
    }

    fn dispatch_event(&mut self, frame: SseFrame) -> Result<(), ProviderError> {
        let raw = std::str::from_utf8(&frame.data)
            .map_err(|_| provider_chat_stream_malformed_error("event_data_invalid_utf8"))?
            .trim();
        // [DONE] 是流终点；其后的尾帧会被共享驱动在到达终态后停止派发，不参与终态物化。
        if raw == "[DONE]" {
            self.done = true;
            return Ok(());
        }
        let payload = serde_json::from_str::<Value>(raw)
            .map_err(|_| provider_chat_stream_malformed_error("event_data_invalid_json"))?;
        if let Some(error) = payload.get("error").filter(|error| !error.is_null()) {
            return Err(crate::error::provider_embedded_error(
                &crate::error::provider_error_fields(error),
                "provider Chat stream returned an error",
                "chat_stream_error",
            ));
        }
        if let Some(usage) = payload.get("usage").filter(|value| value.is_object()) {
            self.usage = Some(usage.clone());
        }
        let Some(choices) = payload.get("choices").and_then(Value::as_array) else {
            if payload.get("choices").is_some() {
                return Err(provider_chat_stream_malformed_error("choices_invalid"));
            }
            // 只带 usage、没有 choices 的块是 OpenAI include_usage 扩展允许的，直接跳过。
            return Ok(());
        };
        if choices.len() > 1 {
            return Err(provider_chat_stream_malformed_error(
                "multiple_choices_unsupported",
            ));
        }
        for choice in choices {
            if !choice.is_object() {
                return Err(provider_chat_stream_malformed_error("choice_invalid"));
            }
            let index = stream_index(choice.get("index"), "choice_index_invalid")?;
            if index != 0 {
                return Err(provider_chat_stream_malformed_error(
                    "multiple_choices_unsupported",
                ));
            }
            self.saw_choice = true;
            if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
                self.finish_reason = Some(reason.to_string());
            }
            let delta = match choice.get("delta") {
                None | Some(Value::Null) => continue,
                Some(Value::Object(delta)) => delta,
                Some(_) => return Err(provider_chat_stream_malformed_error("delta_invalid")),
            };
            if delta
                .get("role")
                .is_some_and(|role| role.as_str() != Some("assistant"))
            {
                return Err(provider_chat_stream_malformed_error("role_invalid"));
            }
            if delta
                .get("content")
                .is_some_and(|content| !content.is_null() && !content.is_string())
            {
                return Err(provider_chat_stream_malformed_error("content_invalid"));
            }
            if delta
                .get("tool_calls")
                .is_some_and(|calls| !calls.is_null() && !calls.is_array())
            {
                return Err(provider_chat_stream_malformed_error("tool_calls_invalid"));
            }
            // 兼容端点可能在同一块里用多个键携带同一段 reasoning（实测双键同文），故只取第一个非空键。
            if let Some((field, reasoning)) = chat_reasoning_text(delta) {
                self.reasoning_field
                    .get_or_insert_with(|| field.to_string());
                self.reasoning_content.push_str(reasoning);
                (self.on_event)(ProviderStreamEvent::ReasoningTextDelta {
                    delta: reasoning.to_string(),
                });
            }
            if let Some(details) = delta
                .get("reasoning_details")
                .filter(|value| !value.is_null())
            {
                let details = details.as_array().ok_or_else(|| {
                    provider_chat_stream_malformed_error("reasoning_details_not_array")
                })?;
                self.receive_reasoning_details(details)?;
            }
            if let Some(text) = delta.get("content").and_then(Value::as_str)
                && !text.is_empty()
            {
                self.content.push_str(text);
                (self.on_event)(ProviderStreamEvent::OutputTextDelta {
                    delta: text.to_string(),
                });
            }
            if let Some(tool_calls) = delta.get("tool_calls").and_then(Value::as_array) {
                for call in tool_calls {
                    self.receive_tool_call_fragment(call)?;
                    if ["/function/name", "/function/arguments"]
                        .iter()
                        .any(|path| {
                            call.pointer(path)
                                .and_then(Value::as_str)
                                .is_some_and(|text| !text.is_empty())
                        })
                    {
                        (self.on_event)(ProviderStreamEvent::ToolCallDelta);
                    }
                }
            }
        }
        Ok(())
    }

    fn materialize_terminal(&mut self) -> Result<Self::Terminal, ProviderError> {
        if !self.done {
            return Err(provider_chat_stream_malformed_error(
                "terminal_done_missing",
            ));
        }
        if !self.saw_choice {
            return Err(provider_chat_stream_malformed_error("choice_missing"));
        }
        let finish_reason = self
            .finish_reason
            .take()
            .ok_or_else(|| provider_chat_stream_malformed_error("finish_reason_missing"))?;
        let tool_calls = std::mem::take(&mut self.tool_calls)
            .into_values()
            .map(|call| {
                let arguments = parse_tool_arguments(&call.arguments);
                ModelToolCall {
                    tool_call_id: call.id,
                    tool_name: call.name,
                    arguments,
                }
            })
            .collect();
        Ok(ChatResponseParts {
            content: std::mem::take(&mut self.content),
            tool_calls,
            reasoning_content: std::mem::take(&mut self.reasoning_content),
            reasoning_field: self
                .reasoning_field
                .take()
                .unwrap_or_else(|| crate::types::DEFAULT_CHAT_REASONING_FIELD.into()),
            reasoning_details: std::mem::take(&mut self.reasoning_details),
            finish_reason,
            usage: parse_usage(
                self.usage.as_ref(),
                "prompt_tokens",
                "completion_tokens",
                "/prompt_tokens_details/cached_tokens",
                "/completion_tokens_details/reasoning_tokens",
            ),
        })
    }

    fn protocol_complete(&self) -> bool {
        self.done
    }

    fn sse_frames(&mut self) -> &mut SseFrameDecoder {
        &mut self.frames
    }
}

impl<'a> ChatSseDecoder<'a> {
    fn new(on_event: &'a mut dyn FnMut(ProviderStreamEvent)) -> Self {
        Self {
            frames: SseFrameDecoder::default(),
            content: String::new(),
            reasoning_content: String::new(),
            reasoning_field: None,
            reasoning_details: Vec::new(),
            tool_calls: BTreeMap::new(),
            finish_reason: None,
            usage: None,
            saw_choice: false,
            done: false,
            on_event,
        }
    }

    /// 接收一组 reasoning detail 片段：先校验形状，再按既有片段规则并入累计状态。只有当
    /// provider 没有通过 reasoning_content/reasoning 给出公开思考文本时，detail 里的公开
    /// 文本才作为思考增量发出一次；其余字段（含加密回放材料）只累计，不进入公开事件。
    fn receive_reasoning_details(&mut self, details: &[Value]) -> Result<(), ProviderError> {
        for detail in details {
            let Some(detail) = detail.as_object() else {
                return Err(provider_chat_stream_malformed_error(
                    "reasoning_detail_not_object",
                ));
            };
            if self.reasoning_field.is_none()
                && let Some(text) = chat_reasoning_detail_text(detail)
            {
                (self.on_event)(ProviderStreamEvent::ReasoningTextDelta {
                    delta: text.to_string(),
                });
            }
            append_reasoning_detail(&mut self.reasoning_details, detail);
        }
        Ok(())
    }

    /// 接收一个工具调用分片：校验形状后按 index 归入聚合槽，名称和参数继续往后拼。
    fn receive_tool_call_fragment(&mut self, call: &Value) -> Result<(), ProviderError> {
        // 空串等同于「本分片没有声明类型」：部分提供方只在第一个分片声明 function，
        // 后续参数分片重复该字段但留空。不是字符串的仍然拒绝。
        if !call.is_object()
            || call
                .get("type")
                .is_some_and(|kind| !matches!(kind.as_str(), Some("") | Some("function")))
        {
            return Err(provider_chat_stream_malformed_error(
                "tool_call_type_invalid",
            ));
        }
        if call
            .get("function")
            .is_some_and(|function| !function.is_null() && !function.is_object())
        {
            return Err(provider_chat_stream_malformed_error(
                "tool_function_invalid",
            ));
        }
        if let Some(function) = call.get("function").and_then(Value::as_object)
            && ["name", "arguments"].iter().any(|key| {
                function
                    .get(*key)
                    .is_some_and(|value| !value.is_null() && !value.is_string())
            })
        {
            return Err(provider_chat_stream_malformed_error(
                "tool_function_field_invalid",
            ));
        }
        // 工具分片归到哪个调用必须没有歧义：省略 index 的单个调用按兼容保留，非法
        // 索引直接拒绝，绝不把不同调用拼进同一个聚合槽。
        let index = stream_index(call.get("index"), "tool_call_index_invalid")?;
        let entry = self.tool_calls.entry(index).or_default();
        if let Some(id) = call.get("id").and_then(Value::as_str)
            && entry.id.is_empty()
        {
            entry.id = id.to_string();
        }
        if let Some(function) = call.get("function").and_then(Value::as_object) {
            if let Some(name) = function.get("name").and_then(Value::as_str) {
                entry.name.push_str(name);
            }
            if let Some(arguments) = function.get("arguments").and_then(Value::as_str) {
                entry.arguments.push_str(arguments);
            }
        }
        Ok(())
    }
}

/// 合并同一文本/摘要片段的增量；加密条目保持原有的边界和字段。
fn append_reasoning_detail(details: &mut Vec<Value>, incoming: &serde_json::Map<String, Value>) {
    let text_key = chat_reasoning_detail_text_field(incoming);
    if let (Some(key), Some(previous)) = (text_key, details.last_mut()) {
        let same_segment = previous.get("type") == incoming.get("type")
            && ["id", "index"].iter().all(|key| {
                match (
                    previous.get(*key).filter(|value| !value.is_null()),
                    incoming.get(*key).filter(|value| !value.is_null()),
                ) {
                    (Some(left), Some(right)) => left == right,
                    _ => true,
                }
            });
        if same_segment
            && let (Some(Value::String(existing)), Some(delta)) = (
                previous.get_mut(key),
                incoming.get(key).and_then(Value::as_str),
            )
        {
            existing.push_str(delta);
            for (field, value) in incoming {
                if previous
                    .get(field)
                    .is_none_or(|existing| existing.is_null() || existing.as_str() == Some(""))
                {
                    previous[field] = value.clone();
                }
            }
            return;
        }
    }
    details.push(Value::Object(incoming.clone()));
}

fn provider_chat_stream_malformed_error(reason: &'static str) -> ProviderError {
    provider_stream_malformed_error(
        "provider Chat stream was malformed",
        "chat_stream_malformed",
        reason,
    )
}

/// 流里 index 字段的唯一解释：省略 index 的单个 choice / 工具调用按兼容保留为 0；
/// 写了却不是合法索引（负数、字符串、null）属于协议缺陷，不与真实的索引 0 混同。
fn stream_index(value: Option<&Value>, malformed: &'static str) -> Result<u64, ProviderError> {
    match value {
        None => Ok(0),
        Some(value) => value
            .as_u64()
            .ok_or_else(|| provider_chat_stream_malformed_error(malformed)),
    }
}
