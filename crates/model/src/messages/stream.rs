use std::collections::BTreeMap;

use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::config::selection::{ProviderConfig, SelectedModel};
use crate::error::{
    ModelErrorKind, ProviderError, bounded_provider_error_diagnostic, provider_embedded_error,
    provider_error_fields,
};
use crate::provider::{
    ProviderStreamEvent, finalize_provider_response, parse_tool_arguments, provider_response_validation_error,
};
use crate::transport::stream::{
    SseStreamDecoder, provider_stream_malformed_error, provider_stream_unterminated_error, read_sse_stream,
};
use crate::types::{
    ContentBlock, Message, ModelStopReason, ModelToolCall, ModelTurnResponse, ModelUsage,
    ProviderBlockMetadata, ProviderMessageMetadata,
};

pub(crate) async fn read_messages_sse_stream(
    cancellation: &CancellationToken,
    response: reqwest::Response,
    on_event: &mut (dyn FnMut(ProviderStreamEvent) + Send),
    config: &ProviderConfig,
    selection: &SelectedModel,
    endpoint: &str,
    prefix_digest: Option<String>,
) -> ModelTurnResponse {
    let mut decoder = MessagesSseDecoder::new(on_event, &config.api_key);
    let result = read_sse_stream(cancellation, response, &mut decoder).await;
    decoder.finish(result, config, selection, endpoint, prefix_digest)
}

#[cfg(test)]
pub(crate) fn decode_messages_events_for_test(
    events: &[Value],
    config: &ProviderConfig,
    selection: &SelectedModel,
    endpoint: &str,
) -> ModelTurnResponse {
    let mut noop = |_| {};
    let mut decoder = MessagesSseDecoder::new(&mut noop, &config.api_key);
    let mut result = Ok(());
    for event in events {
        let bytes = serde_json::to_vec(event).expect("serialize test sse event");
        if let Err(err) = decoder.dispatch_event(&bytes) {
            result = Err(err);
            break;
        }
    }
    decoder.finish(result, config, selection, endpoint, None)
}

enum MessagesBlock {
    Text {
        text: String,
    },
    Thinking {
        thinking: String,
        signature: String,
    },
    RedactedThinking {
        fields: serde_json::Map<String, Value>,
    },
    ToolUse {
        id: String,
        name: String,
        initial_input: Option<Value>,
        partial_json: String,
    },
}

#[derive(Default)]
struct MessagesUsageAccumulator {
    input_tokens: Option<u64>,
    cache_creation_input_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
    output_tokens: Option<u64>,
}

impl MessagesUsageAccumulator {
    fn update(&mut self, usage: Option<&Value>) {
        let Some(usage) = usage.and_then(Value::as_object) else {
            return;
        };
        if let Some(value) = usage.get("input_tokens").and_then(Value::as_u64) {
            self.input_tokens = Some(value);
        }
        if let Some(value) = usage.get("cache_creation_input_tokens").and_then(Value::as_u64) {
            self.cache_creation_input_tokens = Some(value);
        }
        if let Some(value) = usage.get("cache_read_input_tokens").and_then(Value::as_u64) {
            self.cache_read_input_tokens = Some(value);
        }
        if let Some(value) = usage.get("output_tokens").and_then(Value::as_u64) {
            self.output_tokens = Some(value);
        }
    }

    /// Messages 协议的 `input_tokens` 不含缓存创建与缓存命中部分，总输入需三者相加。
    fn into_model_usage(self) -> Option<ModelUsage> {
        let input = self.input_tokens?;
        let output = self.output_tokens?;
        let total_input = input
            .saturating_add(self.cache_creation_input_tokens.unwrap_or(0))
            .saturating_add(self.cache_read_input_tokens.unwrap_or(0));
        Some(ModelUsage {
            input_tokens: total_input,
            output_tokens: output,
            total_tokens: total_input.saturating_add(output),
            cached_input_tokens: self.cache_read_input_tokens,
        })
    }
}

struct MessagesSseDecoder<'a> {
    blocks: BTreeMap<u64, MessagesBlock>,
    usage: MessagesUsageAccumulator,
    stop_reason: Option<String>,
    completed: bool,
    received_event: bool,
    api_key: &'a str,
    on_event: &'a mut (dyn FnMut(ProviderStreamEvent) + Send),
}

impl SseStreamDecoder for MessagesSseDecoder<'_> {
    fn frame_malformed(reason: &'static str) -> ProviderError {
        provider_messages_stream_malformed_error(reason)
    }

    fn frame_unterminated(&self) -> ProviderError {
        provider_stream_unterminated_error("provider Messages stream was malformed", self.received_event)
    }

    fn dispatch_event(&mut self, data: &[u8]) -> Result<(), ProviderError> {
        let payload = serde_json::from_slice::<Value>(data).map_err(|error| {
            let detail = bounded_provider_error_diagnostic(&error.to_string());
            ProviderError::diagnostic(
                ModelErrorKind::Other,
                "provider Messages stream was malformed",
                &["event_data_invalid_json", detail.as_str()],
            )
        })?;
        let event_type = payload
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| provider_messages_stream_malformed_error("event_type_missing"))?;
        match event_type {
            "message_start" => {
                let message = payload
                    .get("message")
                    .and_then(Value::as_object)
                    .ok_or_else(|| provider_messages_stream_malformed_error("message_start_missing"))?;
                self.usage.update(message.get("usage"));
            }
            "content_block_start" => self.handle_block_start(&payload)?,
            "content_block_delta" => self.handle_block_delta(&payload)?,
            "content_block_stop" => {
                let index = block_index(&payload)?;
                if !self.blocks.contains_key(&index) {
                    return Err(provider_messages_stream_malformed_error("content_block_index_unknown"));
                }
            }
            "message_delta" => {
                self.usage.update(payload.get("usage"));
                if let Some(reason) =
                    payload.get("delta").and_then(|delta| delta.get("stop_reason")).and_then(Value::as_str)
                {
                    self.stop_reason = Some(reason.to_string());
                }
            }
            "message_stop" => {
                self.completed = true;
            }
            "ping" => {}
            "error" => {
                let fields = provider_error_fields(
                    payload.get("error").filter(|error| error.is_object()).unwrap_or(&payload),
                );
                return Err(provider_embedded_error(
                    &fields,
                    "provider Messages stream returned an error",
                    self.api_key,
                ));
            }
            _ => {}
        }
        self.received_event = true;
        Ok(())
    }

    fn protocol_complete(&self) -> bool {
        self.completed
    }
}

impl<'a> MessagesSseDecoder<'a> {
    fn new(on_event: &'a mut (dyn FnMut(ProviderStreamEvent) + Send), api_key: &'a str) -> Self {
        Self {
            blocks: BTreeMap::new(),
            usage: MessagesUsageAccumulator::default(),
            stop_reason: None,
            completed: false,
            received_event: false,
            api_key,
            on_event,
        }
    }

    fn handle_block_start(&mut self, payload: &Value) -> Result<(), ProviderError> {
        let index = block_index(payload)?;
        if self.blocks.contains_key(&index) {
            return Err(provider_messages_stream_malformed_error("content_block_index_duplicate"));
        }
        let content_block = payload
            .get("content_block")
            .and_then(Value::as_object)
            .ok_or_else(|| provider_messages_stream_malformed_error("content_block_missing"))?;
        let block_type = content_block
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| provider_messages_stream_malformed_error("content_block_type_missing"))?;
        let block = match block_type {
            "text" => {
                let text = content_block.get("text").and_then(Value::as_str).unwrap_or_default().to_string();
                if !text.is_empty() {
                    (self.on_event)(ProviderStreamEvent::OutputTextDelta { delta: text.clone() });
                }
                MessagesBlock::Text { text }
            }
            "thinking" => {
                let thinking =
                    content_block.get("thinking").and_then(Value::as_str).unwrap_or_default().to_string();
                let signature =
                    content_block.get("signature").and_then(Value::as_str).unwrap_or_default().to_string();
                if !thinking.is_empty() {
                    (self.on_event)(ProviderStreamEvent::ReasoningTextDelta { delta: thinking.clone() });
                }
                MessagesBlock::Thinking { thinking, signature }
            }
            "redacted_thinking" => MessagesBlock::RedactedThinking { fields: content_block.clone() },
            "tool_use" => {
                let id = content_block
                    .get("id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| provider_messages_stream_malformed_error("tool_use_id_missing"))?
                    .to_string();
                let name = content_block
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| provider_messages_stream_malformed_error("tool_use_name_missing"))?
                    .to_string();
                let initial_input = content_block.get("input").filter(|value| value.is_object()).cloned();
                (self.on_event)(ProviderStreamEvent::ToolCallDelta);
                MessagesBlock::ToolUse {
                    id,
                    name,
                    initial_input,
                    partial_json: String::new(),
                }
            }
            _ => {
                return Err(provider_messages_stream_malformed_error("content_block_type_unsupported"));
            }
        };
        self.blocks.insert(index, block);
        Ok(())
    }

    fn handle_block_delta(&mut self, payload: &Value) -> Result<(), ProviderError> {
        let index = block_index(payload)?;
        let delta = payload
            .get("delta")
            .and_then(Value::as_object)
            .ok_or_else(|| provider_messages_stream_malformed_error("content_block_delta_missing"))?;
        let delta_type = delta
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| provider_messages_stream_malformed_error("content_block_delta_type_missing"))?;
        let block = self
            .blocks
            .get_mut(&index)
            .ok_or_else(|| provider_messages_stream_malformed_error("content_block_index_unknown"))?;
        match (delta_type, block) {
            ("text_delta", MessagesBlock::Text { text }) => {
                let chunk = delta
                    .get("text")
                    .and_then(Value::as_str)
                    .ok_or_else(|| provider_messages_stream_malformed_error("text_delta_missing"))?;
                if !chunk.is_empty() {
                    text.push_str(chunk);
                    (self.on_event)(ProviderStreamEvent::OutputTextDelta { delta: chunk.to_string() });
                }
            }
            ("thinking_delta", MessagesBlock::Thinking { thinking, .. }) => {
                let chunk = delta
                    .get("thinking")
                    .and_then(Value::as_str)
                    .ok_or_else(|| provider_messages_stream_malformed_error("thinking_delta_missing"))?;
                if !chunk.is_empty() {
                    thinking.push_str(chunk);
                    (self.on_event)(ProviderStreamEvent::ReasoningTextDelta { delta: chunk.to_string() });
                }
            }
            ("signature_delta", MessagesBlock::Thinking { signature, .. }) => {
                let chunk = delta
                    .get("signature")
                    .and_then(Value::as_str)
                    .ok_or_else(|| provider_messages_stream_malformed_error("signature_delta_missing"))?;
                signature.push_str(chunk);
            }
            ("input_json_delta", MessagesBlock::ToolUse { partial_json, .. }) => {
                let chunk = delta
                    .get("partial_json")
                    .and_then(Value::as_str)
                    .ok_or_else(|| provider_messages_stream_malformed_error("input_json_delta_missing"))?;
                if !chunk.is_empty() {
                    partial_json.push_str(chunk);
                    (self.on_event)(ProviderStreamEvent::ToolCallDelta);
                }
            }
            ("citations_delta", _) => {}
            _ => {
                return Err(provider_messages_stream_malformed_error("content_block_delta_mismatch"));
            }
        }
        Ok(())
    }

    fn finish(
        self,
        result: Result<(), ProviderError>,
        config: &ProviderConfig,
        selection: &SelectedModel,
        endpoint: &str,
        prefix_digest: Option<String>,
    ) -> ModelTurnResponse {
        let stop_reason = result.and_then(|()| {
            if !self.completed {
                return Err(provider_messages_stream_terminal_missing_error(self.received_event));
            }
            match self.stop_reason.as_deref() {
                Some("end_turn" | "tool_use" | "stop_sequence" | "pause_turn") => Ok(ModelStopReason::Stop),
                Some("max_tokens") => Ok(ModelStopReason::Length),
                Some("model_context_window_exceeded") => Err(ProviderError::new(
                    ModelErrorKind::ContextLengthExceeded,
                    "provider Messages response exceeded the model context window",
                )),
                Some("refusal") => Err(ProviderError::new(
                    ModelErrorKind::Other,
                    "provider Messages response was stopped by refusal",
                )),
                Some(other) => {
                    let other = bounded_provider_error_diagnostic(other);
                    Err(provider_response_validation_error(
                        &format!("provider Messages response reported an unsupported stop_reason: {other}"),
                        &["messages_stop_reason_unsupported"],
                    ))
                }
                None => Err(provider_response_validation_error(
                    "provider Messages stream did not report stop_reason",
                    &["messages_stop_reason_missing"],
                )),
            }
        });
        let stop_reason = match stop_reason {
            Ok(stop_reason) => stop_reason,
            Err(error) => {
                let mut partial_text = String::new();
                let mut partial_thinking = String::new();
                for block in self.blocks.into_values() {
                    match block {
                        MessagesBlock::Text { text } => partial_text.push_str(&text),
                        MessagesBlock::Thinking { thinking, .. } => partial_thinking.push_str(&thinking),
                        MessagesBlock::RedactedThinking { .. } | MessagesBlock::ToolUse { .. } => {}
                    }
                }
                return ModelTurnResponse::failed(
                    error,
                    Message::assistant(partial_text, partial_thinking, Vec::new(), None),
                );
            }
        };
        let mut content = Vec::with_capacity(self.blocks.len());
        for block in self.blocks.into_values() {
            match block {
                MessagesBlock::Text { text } if !text.is_empty() => {
                    content.push(ContentBlock::Text { text, metadata: None });
                }
                MessagesBlock::Thinking { thinking, signature } => {
                    let metadata = (!signature.is_empty()).then(|| {
                        let mut map = serde_json::Map::new();
                        map.insert("signature".to_string(), Value::String(signature));
                        ProviderBlockMetadata(map)
                    });
                    if !thinking.is_empty() || metadata.is_some() {
                        content.push(ContentBlock::Thinking { thinking, metadata });
                    }
                }
                MessagesBlock::RedactedThinking { fields } => {
                    content.push(ContentBlock::Thinking {
                        thinking: String::new(),
                        metadata: Some(ProviderBlockMetadata(fields)),
                    });
                }
                MessagesBlock::ToolUse { id, name, initial_input, partial_json } => {
                    let arguments = if partial_json.trim().is_empty() {
                        initial_input.unwrap_or_else(|| json!({}))
                    } else {
                        parse_tool_arguments(&partial_json)
                    };
                    content.push(ContentBlock::ToolCall(ModelToolCall {
                        tool_call_id: id,
                        tool_name: name,
                        arguments,
                        metadata: None,
                    }));
                }
                MessagesBlock::Text { .. } => {}
            }
        }
        let provider_metadata = Some(ProviderMessageMetadata {
            protocol: protocol::ProviderApiProtocol::Messages,
            provider_name: config.provider_name.clone(),
            model_name: selection.model_name.clone(),
            endpoint: endpoint.into(),
            prefix_digest,
            reasoning: selection.reasoning_variant.clone(),
        });
        finalize_provider_response(ModelTurnResponse {
            assistant_message: Message::Assistant { content, provider_metadata },
            usage: self.usage.into_model_usage(),
            stop_reason,
            refusal: false,
        })
    }
}

fn block_index(payload: &Value) -> Result<u64, ProviderError> {
    payload
        .get("index")
        .and_then(Value::as_u64)
        .ok_or_else(|| provider_messages_stream_malformed_error("content_block_index_missing"))
}

fn provider_messages_stream_malformed_error(reason: &'static str) -> ProviderError {
    provider_stream_malformed_error("provider Messages stream was malformed", reason)
}

fn provider_messages_stream_terminal_missing_error(received_event: bool) -> ProviderError {
    let kind = if received_event {
        ModelErrorKind::Temporary
    } else {
        ModelErrorKind::Other
    };
    ProviderError::new(kind, "provider Messages stream did not contain message_stop")
}
