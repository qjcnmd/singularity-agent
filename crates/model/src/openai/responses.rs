//! OpenAI Responses 协议：请求编码、SSE 解码与响应解析。

use serde_json::{Value, json};

use crate::config::selection::{OpenAiProviderConfig, SelectedModel};
use crate::error::{ModelErrorKind, ProviderError, provider_embedded_error, provider_error_fields};
use crate::openai::parse::{finalize_provider_response, parse_tool_call_arguments, parse_usage};
use crate::provider::contract::{
    provider_content_filter_error, provider_response_validation_error,
};
use crate::provider::telemetry::ProviderStreamEvent;
use crate::transport::stream::{
    SseFrame, SseFrameDecoder, SseStreamDecoder, provider_stream_malformed_error, read_sse_stream,
};
use crate::types::{
    ModelMessage, ModelRole, ModelStopReason, ModelToolCall, ModelTurnRequest, ModelTurnResponse,
    ProviderReasoningReplay,
};
use singularity_core::CancellationToken;

pub(crate) fn openai_responses_stream_request_payload(
    request: &ModelTurnRequest,
    selection: &SelectedModel,
    provider_name: &str,
) -> Value {
    let (instructions, input) = openai_responses_input(&request.messages, selection, provider_name);
    let mut payload = json!({
        "model": selection.model_name,
        "input": input,
        "stream": true,
        "store": false,
        "include": ["reasoning.encrypted_content"],
    });
    let reasoning = super::reasoning_wire_decision(selection);
    if let Some(instructions) = instructions {
        payload["instructions"] = json!(instructions);
    }
    if let Some(max_output_tokens) = request.model_preferences.max_output_tokens {
        payload["max_output_tokens"] = json!(max_output_tokens);
    }
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
                    })
                })
                .collect::<Vec<_>>()
        );
        if selection.supports_tool_choice {
            payload["tool_choice"] = serde_json::json!("auto");
        }
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
    payload
}

pub(crate) fn parse_openai_responses_response(
    config: &OpenAiProviderConfig,
    mut payload: Value,
    model_name: &str,
    reasoning_effort: Option<&str>,
) -> Result<ModelTurnResponse, ProviderError> {
    if let Some(error) = payload.get("error").filter(|error| !error.is_null()) {
        return Err(provider_embedded_error(
            &provider_error_fields(error),
            "provider Responses payload contained an error",
            "responses_error_present",
        ));
    }
    let status = payload.get("status").and_then(Value::as_str);
    let incomplete_reason = payload
        .get("incomplete_details")
        .and_then(|details| details.get("reason"))
        .and_then(Value::as_str);
    let length_truncated =
        status == Some("incomplete") && incomplete_reason == Some("max_output_tokens");
    // 只有触达输出上限的不完整响应算正常截断，其余未完成一律失败。
    if status != Some("completed") && !length_truncated {
        if incomplete_reason == Some("content_filter") {
            return Err(provider_content_filter_error(
                "provider Responses response was stopped by content filter",
            ));
        }
        return Err(provider_response_validation_error(
            &format!(
                "provider Responses payload was not completed (reason: {})",
                incomplete_reason.unwrap_or("unknown")
            ),
            vec!["responses_status_not_completed".to_string()],
        ));
    }
    // output 整份移出 payload：解析借用同一批条目，成功后它直接作为回放载荷，不再另建容器。
    let output = match payload.get_mut("output").map(std::mem::take) {
        Some(Value::Array(items)) => items,
        _ => {
            return Err(provider_response_validation_error(
                "provider Responses payload missing output items",
                vec!["responses_output_missing".to_string()],
            ));
        }
    };
    let ParsedResponsesOutput {
        content,
        thinking,
        tool_calls,
    } = parse_responses_output(&output)?;
    let has_reasoning_item = output
        .iter()
        .any(|item| item.get("type").and_then(Value::as_str) == Some("reasoning"));
    let replay = if has_reasoning_item {
        Some(ProviderReasoningReplay::Responses {
            provider_name: config.provider_name.clone(),
            model_name: model_name.to_string(),
            reasoning_effort: reasoning_effort.map(str::to_string),
            tool_call_ids: tool_calls
                .iter()
                .map(|call| call.tool_call_id.clone())
                .collect(),
            items: output,
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
        usage: parse_usage(
            payload.get("usage"),
            "input_tokens",
            "output_tokens",
            "/input_tokens_details/cached_tokens",
            "/output_tokens_details/reasoning_tokens",
        ),
        stop_reason: Some(if length_truncated {
            ModelStopReason::Length
        } else {
            ModelStopReason::Stop
        }),
    })
}

struct ParsedResponsesOutput {
    content: String,
    thinking: String,
    tool_calls: Vec<ModelToolCall>,
}

/// message content 规则：缺 content 算协议错误，text/output_text 与 refusal 都拼成可见文本。
fn parse_responses_message_content(content: Option<&Value>) -> Result<String, &'static str> {
    match content {
        None | Some(Value::Null) => Err("responses_message_content_missing"),
        Some(Value::String(text)) => Ok(text.clone()),
        Some(Value::Array(parts)) => {
            let mut content = String::new();
            for part in parts {
                let part = part
                    .as_object()
                    .ok_or("responses_message_content_part_unsupported")?;
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

fn parse_responses_output(output: &[Value]) -> Result<ParsedResponsesOutput, ProviderError> {
    let mut content = String::new();
    let mut thinking = String::new();
    let mut tool_calls = Vec::new();
    for item in output {
        let Value::Object(item) = item else {
            return Err(provider_response_validation_error(
                "provider Responses output item was not an object",
                vec!["responses_output_item_invalid".to_string()],
            ));
        };
        let Some(item_type) = item.get("type").and_then(Value::as_str) else {
            return Err(provider_response_validation_error(
                "provider Responses output item type was missing",
                vec!["responses_output_item_type_missing".to_string()],
            ));
        };
        match item_type {
            "message" => {
                let message =
                    parse_responses_message_content(item.get("content")).map_err(|evidence| {
                        provider_response_validation_error(
                            "provider Responses message content was invalid",
                            vec![evidence.to_string()],
                        )
                    })?;
                content.push_str(&message);
            }
            "function_call" => {
                let arguments = parse_tool_call_arguments(item.get("arguments"))?;
                tool_calls.push(ModelToolCall {
                    tool_call_id: item
                        .get("call_id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    tool_name: item
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    arguments,
                });
            }
            "reasoning" => {
                for summary in item
                    .get("summary")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    if summary.get("type").and_then(Value::as_str) == Some("summary_text")
                        && let Some(text) = summary.get("text").and_then(Value::as_str)
                    {
                        thinking.push_str(text);
                    }
                }
                if item
                    .get("id")
                    .and_then(Value::as_str)
                    .is_none_or(str::is_empty)
                {
                    return Err(provider_response_validation_error(
                        "provider Responses reasoning item id was missing",
                        vec!["responses_reasoning_item_id_missing".to_string()],
                    ));
                }
            }
            _ => {
                return Err(provider_response_validation_error(
                    "provider Responses payload contained an unsupported output item",
                    vec!["responses_output_item_unsupported".to_string()],
                ));
            }
        }
    }
    Ok(ParsedResponsesOutput {
        content,
        thinking,
        tool_calls,
    })
}

/// 把消息投影成 Responses 的输入；私有续接按身份筛选（见 super::reasoning_replay_for）。
pub(crate) fn openai_responses_input(
    messages: &[ModelMessage],
    selection: &SelectedModel,
    provider_name: &str,
) -> (Option<String>, Vec<Value>) {
    let instruction_count = messages
        .iter()
        .take_while(|message| matches!(message.role, ModelRole::System | ModelRole::Developer))
        .count();
    let instructions = messages[..instruction_count]
        .iter()
        .map(|message| message.content.as_str())
        .filter(|message| !message.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    let mut items = Vec::new();
    for message in &messages[instruction_count..] {
        match message.role {
            ModelRole::Tool => {
                items.push(json!({
                    "type": "function_call_output",
                    "call_id": message.tool_call_id,
                    "output": message.content,
                }));
            }
            ModelRole::Assistant => {
                if let Some(ProviderReasoningReplay::Responses {
                    items: replay_items,
                    ..
                }) = super::reasoning_replay_for(message, selection, provider_name)
                {
                    items.extend(replay_items.iter().cloned());
                } else {
                    if !message.content.is_empty() {
                        items.push(json!({
                            "type": "message",
                            "role": "assistant",
                            "content": message.content,
                        }));
                    }
                    items.extend(message.tool_calls.iter().map(|call| {
                        json!({
                            "type": "function_call",
                            "call_id": call.tool_call_id,
                            "name": call.tool_name,
                            "arguments": call.arguments.to_string(),
                        })
                    }));
                }
            }
            ModelRole::System | ModelRole::Developer | ModelRole::User => {
                let role = match message.role {
                    // 开头的 system/developer 已折叠进上面的 instructions 字段；历史之后的
                    // developer 消息只能用 Responses 输入 schema 接受的角色，而 system
                    // 保留它原本的指令语义。
                    ModelRole::System | ModelRole::Developer => "system",
                    ModelRole::User => "user",
                    ModelRole::Assistant | ModelRole::Tool => unreachable!(),
                };
                items.push(json!({
                    "type": "message",
                    "role": role,
                    "content": message.content,
                }));
            }
        }
    }
    ((!instructions.is_empty()).then_some(instructions), items)
}

/// 用 Responses 协议解码一次真实响应：共享读取循环驱动本协议解码器，终结也在本模块内。
pub(crate) fn read_responses_sse_stream(
    runtime: &tokio::runtime::Handle,
    cancellation: &CancellationToken,
    response: reqwest::Response,
    on_event: &mut dyn FnMut(ProviderStreamEvent),
    config: &OpenAiProviderConfig,
    selection: &SelectedModel,
) -> Result<ModelTurnResponse, ProviderError> {
    let payload = read_sse_stream(
        runtime,
        cancellation,
        response,
        ResponsesSseDecoder::new(on_event),
    )?;
    parse_openai_responses_response(
        config,
        payload,
        &selection.model_name,
        selection.reasoning_variant.as_deref(),
    )
}

/// 按 Responses 事件契约增量解析、总字节有上限的 SSE 解码器。
struct ResponsesSseDecoder<'a> {
    frames: SseFrameDecoder,
    terminal_response: Option<Value>,
    on_event: &'a mut dyn FnMut(ProviderStreamEvent),
}

impl SseStreamDecoder for ResponsesSseDecoder<'_> {
    type Terminal = Value;
    fn frame_malformed() -> fn(&'static str) -> ProviderError {
        provider_responses_stream_malformed_error
    }

    fn dispatch_event(&mut self, frame: SseFrame) -> Result<(), ProviderError> {
        let mut payload = serde_json::from_slice::<Value>(&frame.data)
            .map_err(|_| provider_responses_stream_malformed_error("event_data_invalid_json"))?;
        let payload_type = payload
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| provider_responses_stream_malformed_error("event_type_missing"))?;
        if frame
            .event_name
            .as_deref()
            .is_some_and(|event_name| event_name != payload_type)
        {
            return Err(provider_responses_stream_malformed_error(
                "event_type_mismatch",
            ));
        }
        if payload_type == "ping" {
            return Ok(());
        }
        // completed 和 incomplete 都把 response 对象当终态：后者由 parse_openai_responses_response
        // 判断是长度截断还是直接失败，两者只有诊断标签不同。
        let completed = payload_type == "response.completed";
        match payload_type {
            "response.output_text.delta" | "response.reasoning_summary_text.delta" => {
                let reasoning = payload_type == "response.reasoning_summary_text.delta";
                let delta = payload
                    .get("delta")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        provider_responses_stream_malformed_error(if reasoning {
                            "reasoning_summary_delta_missing"
                        } else {
                            "output_text_delta_missing"
                        })
                    })?;
                if !delta.is_empty() {
                    let delta = delta.to_string();
                    (self.on_event)(if reasoning {
                        ProviderStreamEvent::ReasoningTextDelta { delta }
                    } else {
                        ProviderStreamEvent::OutputTextDelta { delta }
                    });
                }
            }
            "response.completed" | "response.incomplete" => {
                let response =
                    payload
                        .get_mut("response")
                        .map(std::mem::take)
                        .ok_or_else(|| {
                            provider_responses_stream_malformed_error(if completed {
                                "completed_response_missing"
                            } else {
                                "incomplete_response_missing"
                            })
                        })?;
                if !response.is_object() {
                    return Err(provider_responses_stream_malformed_error(if completed {
                        "completed_response_invalid"
                    } else {
                        "incomplete_response_invalid"
                    }));
                }
                self.terminal_response = Some(response);
            }
            "error" => {
                let fields = provider_error_fields(
                    payload
                        .get("error")
                        .filter(|error| error.is_object())
                        .unwrap_or(&payload),
                );
                return Err(provider_embedded_error(
                    &fields,
                    "provider Responses stream returned an error",
                    "responses_stream_error",
                ));
            }
            "response.failed" => {
                let fields = payload
                    .get("response")
                    .and_then(|response| response.get("error"))
                    .map(provider_error_fields)
                    .unwrap_or_default();
                return Err(provider_embedded_error(
                    &fields,
                    "provider Responses stream failed",
                    "responses_stream_failed",
                ));
            }
            // 其余事件不影响公开文本与终态，统一忽略。
            _ => {}
        }
        Ok(())
    }

    fn materialize_terminal(&mut self) -> Result<Self::Terminal, ProviderError> {
        self.terminal_response
            .take()
            .ok_or_else(provider_responses_stream_terminal_missing_error)
    }

    fn protocol_complete(&self) -> bool {
        // failed/error 在 dispatch 阶段就已以错误结束，不会走到这里。
        self.terminal_response.is_some()
    }

    fn sse_frames(&mut self) -> &mut SseFrameDecoder {
        &mut self.frames
    }
}

impl<'a> ResponsesSseDecoder<'a> {
    fn new(on_event: &'a mut dyn FnMut(ProviderStreamEvent)) -> Self {
        Self {
            frames: SseFrameDecoder::default(),
            terminal_response: None,
            on_event,
        }
    }
}

fn provider_responses_stream_malformed_error(reason: &'static str) -> ProviderError {
    provider_stream_malformed_error(
        "provider Responses stream was malformed",
        "responses_stream_malformed",
        reason,
    )
}

fn provider_responses_stream_terminal_missing_error() -> ProviderError {
    ProviderError::new(
        ModelErrorKind::JsonSchemaViolation,
        "provider Responses stream did not contain a completed terminal",
    )
    .with_code("responses_stream_terminal_missing")
}
