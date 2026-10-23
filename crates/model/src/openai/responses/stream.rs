use super::*;

/// 读取并解码一次 Responses 流式响应。
pub(crate) async fn read_responses_sse_stream(
    cancellation: &CancellationToken,
    response: reqwest::Response,
    on_event: &mut (dyn FnMut(ProviderStreamEvent) + Send),
    config: &OpenAiProviderConfig,
    selection: &SelectedModel,
    endpoint: &str,
) -> ModelTurnResponse {
    let mut decoder = ResponsesSseDecoder::new(on_event);
    let result = read_sse_stream(cancellation, response, &mut decoder).await;
    decoder.finish(result, config, &selection.model_name, endpoint)
}

/// Responses SSE 解码器：边收边发出正文和思考增量，保留最终的 response 对象。
struct ResponsesSseDecoder<'a> {
    terminal_response: Option<Value>,
    text: String,
    thinking: String,
    on_event: &'a mut (dyn FnMut(ProviderStreamEvent) + Send),
}

impl SseStreamDecoder for ResponsesSseDecoder<'_> {
    fn frame_malformed() -> fn(&'static str) -> ProviderError {
        provider_responses_stream_malformed_error
    }

    fn dispatch_event(&mut self, data: &[u8]) -> Result<(), ProviderError> {
        let mut payload = serde_json::from_slice::<Value>(data)
            .map_err(|_| provider_responses_stream_malformed_error("event_data_invalid_json"))?;
        let payload_type = payload
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| provider_responses_stream_malformed_error("event_type_missing"))?;
        // completed 和 incomplete 都把 response 对象当终态：后者由 parse_openai_responses_response
        // 判断是长度截断还是直接失败，两者只有诊断标签不同。
        let completed = payload_type == "response.completed";
        match payload_type {
            "response.function_call_arguments.delta" => {
                if payload.get("delta").and_then(Value::as_str).is_some_and(|text| !text.is_empty()) {
                    (self.on_event)(ProviderStreamEvent::ToolCallDelta);
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
                if !delta.is_empty() {
                    if reasoning {
                        self.thinking.push_str(delta);
                    } else {
                        self.text.push_str(delta);
                    }
                    let delta = delta.to_string();
                    (self.on_event)(if reasoning {
                        ProviderStreamEvent::ReasoningTextDelta { delta }
                    } else {
                        ProviderStreamEvent::OutputTextDelta { delta }
                    });
                }
            }
            "response.completed" | "response.incomplete" => {
                let response = payload.get_mut("response").map(std::mem::take).ok_or_else(|| {
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
            // 其余事件不影响公开文本与终态，都忽略。
            _ => {}
        }
        Ok(())
    }

    fn protocol_complete(&self) -> bool {
        // failed/error 在 dispatch 阶段就已以错误结束，不会走到这里。
        self.terminal_response.is_some()
    }
}

impl<'a> ResponsesSseDecoder<'a> {
    fn new(on_event: &'a mut (dyn FnMut(ProviderStreamEvent) + Send)) -> Self {
        Self {
            terminal_response: None,
            text: String::new(),
            thinking: String::new(),
            on_event,
        }
    }

    fn finish(
        mut self,
        result: Result<(), ProviderError>,
        config: &OpenAiProviderConfig,
        model_name: &str,
        endpoint: &str,
    ) -> ModelTurnResponse {
        let result = result.and_then(|()| {
            let payload = self
                .terminal_response
                .take()
                .ok_or_else(provider_responses_stream_terminal_missing_error)?;
            parse_openai_responses_response(config, payload, model_name, endpoint)
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

fn provider_responses_stream_terminal_missing_error() -> ProviderError {
    ProviderError::new(
        ModelErrorKind::JsonSchemaViolation,
        "provider Responses stream did not contain a completed terminal",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ContentBlock;

    #[test]
    fn interrupted_stream_keeps_text_and_thinking() {
        let config = OpenAiProviderConfig {
            provider_name: "provider".into(),
            base_url: "https://example.com".into(),
            api_key: String::new(),
        };
        for kind in [ModelErrorKind::Cancelled, ModelErrorKind::JsonSchemaViolation] {
            let mut on_event = |_| {};
            let mut decoder = ResponsesSseDecoder::new(&mut on_event);
            decoder
                .dispatch_event(br#"{"type":"response.output_text.delta","delta":"partial"}"#)
                .expect("valid stream fragment");
            decoder
                .dispatch_event(br#"{"type":"response.reasoning_summary_text.delta","delta":"thinking"}"#)
                .expect("valid stream fragment");
            let response = decoder.finish(
                Err(ProviderError::new(kind, "stream ended")),
                &config,
                "model",
                "https://example.com/responses",
            );
            assert_eq!(response.assistant_message.content_text(), "partial");
            assert!(response.assistant_message.content().iter().any(
                |block| matches!(block, ContentBlock::Thinking { thinking, .. } if thinking == "thinking")
            ));
            assert_eq!(response.assistant_message.tool_calls().count(), 0);
            assert_eq!(response.error().map(|error| error.kind), Some(kind));
            assert_eq!(response.usage, None);
        }
    }
}
