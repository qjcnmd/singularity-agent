use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::{parse_openai_responses_response, summary_part_separator};
use crate::config::selection::{ProviderConfig, SelectedModel};
use crate::error::{
    ModelErrorKind, ProviderError, bounded_provider_error_diagnostic, provider_embedded_error,
    provider_error_fields,
};
use crate::provider::ProviderStreamEvent;
use crate::transport::stream::{
    SseStreamDecoder, provider_stream_malformed_error, provider_stream_unterminated_error, read_sse_stream,
};
use crate::types::{Message, ModelTurnResponse};

pub(crate) async fn read_responses_sse_stream(
    cancellation: &CancellationToken,
    response: reqwest::Response,
    on_event: &mut (dyn FnMut(ProviderStreamEvent) + Send),
    config: &ProviderConfig,
    selection: &SelectedModel,
    endpoint: &str,
) -> ModelTurnResponse {
    let mut decoder = ResponsesSseDecoder::new(on_event);
    let result = read_sse_stream(cancellation, response, &mut decoder).await;
    decoder.finish(result, config, selection, endpoint)
}

struct ResponsesSseDecoder<'a> {
    terminal_response: Option<Value>,
    text: String,
    thinking: String,
    last_summary_index: Option<u64>,
    pending_reasoning_separator: bool,
    received_event: bool,
    on_event: &'a mut (dyn FnMut(ProviderStreamEvent) + Send),
}

impl SseStreamDecoder for ResponsesSseDecoder<'_> {
    fn frame_malformed(reason: &'static str) -> ProviderError {
        provider_responses_stream_malformed_error(reason)
    }

    fn frame_unterminated(&self) -> ProviderError {
        provider_stream_unterminated_error("provider Responses stream was malformed", self.received_event)
    }

    fn dispatch_event(&mut self, data: &[u8]) -> Result<(), ProviderError> {
        let mut payload = serde_json::from_slice::<Value>(data).map_err(|error| {
            let detail = bounded_provider_error_diagnostic(&error.to_string());
            ProviderError::diagnostic(
                ModelErrorKind::Other,
                "provider Responses stream was malformed",
                &["event_data_invalid_json", detail.as_str()],
            )
        })?;
        let payload_type = payload
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| provider_responses_stream_malformed_error("event_type_missing"))?;
        let completed = payload_type == "response.completed";
        match payload_type {
            "response.function_call_arguments.delta" => {
                if payload.get("delta").and_then(Value::as_str).is_some_and(|text| !text.is_empty()) {
                    (self.on_event)(ProviderStreamEvent::ToolCallDelta);
                }
            }
            "response.reasoning_summary_part.added" | "response.reasoning_summary_part.done" => {
                if !self.thinking.is_empty() {
                    self.pending_reasoning_separator = true;
                }
                if let Some(index) = payload.get("summary_index").and_then(Value::as_u64) {
                    self.last_summary_index = Some(index);
                }
            }
            "response.output_text.delta"
            | "response.refusal.delta"
            | "response.reasoning_summary_text.delta" => {
                let reasoning = payload_type == "response.reasoning_summary_text.delta";
                let delta = payload.get("delta").and_then(Value::as_str).ok_or_else(|| {
                    provider_responses_stream_malformed_error(if reasoning {
                        "reasoning_summary_delta_missing"
                    } else {
                        "output_text_delta_missing"
                    })
                })?;
                if reasoning && let Some(index) = payload.get("summary_index").and_then(Value::as_u64) {
                    if self.last_summary_index.is_some_and(|prev| prev != index) && !self.thinking.is_empty()
                    {
                        self.pending_reasoning_separator = true;
                    }
                    self.last_summary_index = Some(index);
                }
                if !delta.is_empty() {
                    let delta = if reasoning {
                        let sep = if std::mem::take(&mut self.pending_reasoning_separator) {
                            summary_part_separator(&self.thinking, delta)
                        } else {
                            ""
                        };
                        self.thinking.push_str(sep);
                        self.thinking.push_str(delta);
                        if sep.is_empty() {
                            delta.to_string()
                        } else {
                            format!("{sep}{delta}")
                        }
                    } else {
                        self.text.push_str(delta);
                        delta.to_string()
                    };
                    (self.on_event)(if reasoning {
                        ProviderStreamEvent::ReasoningTextDelta { delta }
                    } else {
                        ProviderStreamEvent::OutputTextDelta { delta }
                    });
                }
            }
            "response.completed" | "response.incomplete" => {
                let response = payload
                    .get_mut("response")
                    .filter(|value| value.is_object())
                    .map(std::mem::take)
                    .ok_or_else(|| {
                        provider_responses_stream_malformed_error(if completed {
                            "completed_response_missing"
                        } else {
                            "incomplete_response_missing"
                        })
                    })?;
                self.terminal_response = Some(response);
            }
            "error" => {
                let fields = provider_error_fields(
                    payload.get("error").filter(|error| error.is_object()).unwrap_or(&payload),
                );
                return Err(provider_embedded_error(&fields, "provider Responses stream returned an error"));
            }
            "response.failed" => {
                let fields = payload
                    .get("response")
                    .and_then(|response| response.get("error"))
                    .map(provider_error_fields)
                    .unwrap_or_default();
                return Err(provider_embedded_error(&fields, "provider Responses stream failed"));
            }
            _ => {}
        }
        self.received_event = true;
        Ok(())
    }

    fn protocol_complete(&self) -> bool {
        self.terminal_response.is_some()
    }
}

impl<'a> ResponsesSseDecoder<'a> {
    fn new(on_event: &'a mut (dyn FnMut(ProviderStreamEvent) + Send)) -> Self {
        Self {
            terminal_response: None,
            text: String::new(),
            thinking: String::new(),
            last_summary_index: None,
            pending_reasoning_separator: false,
            received_event: false,
            on_event,
        }
    }

    fn finish(
        mut self,
        result: Result<(), ProviderError>,
        config: &ProviderConfig,
        selection: &SelectedModel,
        endpoint: &str,
    ) -> ModelTurnResponse {
        let result = result.and_then(|()| {
            let payload = self
                .terminal_response
                .take()
                .ok_or_else(|| provider_responses_stream_terminal_missing_error(self.received_event))?;
            parse_openai_responses_response(
                config,
                payload,
                &selection.model_name,
                endpoint,
                selection.reasoning_variant.clone(),
            )
        });
        match result {
            Ok(response) => response,
            Err(error) => ModelTurnResponse::failed(
                error,
                Message::assistant(self.text, self.thinking, Vec::new(), None),
            ),
        }
    }
}

fn provider_responses_stream_malformed_error(reason: &'static str) -> ProviderError {
    provider_stream_malformed_error("provider Responses stream was malformed", reason)
}

fn provider_responses_stream_terminal_missing_error(received_event: bool) -> ProviderError {
    let kind = if received_event {
        ModelErrorKind::Temporary
    } else {
        ModelErrorKind::Other
    };
    ProviderError::new(kind, "provider Responses stream did not contain a completed terminal")
}
