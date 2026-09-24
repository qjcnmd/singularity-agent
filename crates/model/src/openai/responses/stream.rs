use super::*;

/// 用 Responses 协议解码一次真实响应：共享读取循环驱动本协议解码器，终结也在本模块内。
pub(crate) async fn read_responses_sse_stream(
    cancellation: &CancellationToken,
    response: reqwest::Response,
    on_event: &mut (dyn FnMut(ProviderStreamEvent) + Send),
    config: &OpenAiProviderConfig,
    selection: &SelectedModel,
) -> Result<ModelTurnResponse, ProviderError> {
    let payload =
        read_sse_stream(cancellation, response, ResponsesSseDecoder::new(on_event)).await?;
    parse_openai_responses_response(config, payload, &selection.model_name)
}

/// 按 Responses 事件契约增量解析、总字节有上限的 SSE 解码器。
struct ResponsesSseDecoder<'a> {
    frames: SseFrameDecoder,
    terminal_response: Option<Value>,
    on_event: &'a mut (dyn FnMut(ProviderStreamEvent) + Send),
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
            "response.function_call_arguments.delta" => {
                if payload
                    .get("delta")
                    .and_then(Value::as_str)
                    .is_some_and(|text| !text.is_empty())
                {
                    (self.on_event)(ProviderStreamEvent::ToolCallDelta);
                }
            }
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
    fn new(on_event: &'a mut (dyn FnMut(ProviderStreamEvent) + Send)) -> Self {
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
