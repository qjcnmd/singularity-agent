//! SSE 流式解码器（Chat Completions 与 Responses 格式）。

use std::collections::BTreeMap;
use std::time::Instant;

use reqwest::Response;
use serde_json::Value;
use singularity_core::CancellationToken;

use crate::error::{ModelError, ModelErrorKind, ProviderError, ProviderErrorStage};
use crate::provider::runtime::ProviderRuntime;
use crate::provider::telemetry::ProviderStreamEvent;
use crate::transport::http::{
    MAX_PROVIDER_RESPONSE_BODY_BYTES, block_on_provider_future, duration_millis,
};

/// A stream attempt error plus whether retrying could duplicate visible text.
pub struct StreamAttemptFailure {
    pub error: ProviderError,
    pub emitted_text_delta: bool,
    pub time_to_first_text_delta_ms: Option<u64>,
}

/// A completed stream decode plus timing captured at the decoder boundary.
pub struct StreamAttemptSuccess {
    pub payload: Value,
    pub time_to_first_text_delta_ms: Option<u64>,
}

/// Decode one Chat Completions body while preserving arbitrary HTTP chunk and SSE frame boundaries.
pub fn read_openai_chat_sse(
    runtime: &ProviderRuntime,
    cancellation: &CancellationToken,
    request_timeout_seconds: u64,
    mut response: Response,
    on_event: &mut dyn FnMut(ProviderStreamEvent),
    attempt_started_at: Instant,
) -> Result<StreamAttemptSuccess, StreamAttemptFailure> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_PROVIDER_RESPONSE_BODY_BYTES as u64)
    {
        return Err(StreamAttemptFailure {
            error: provider_response_stream_too_large_error(),
            emitted_text_delta: false,
            time_to_first_text_delta_ms: None,
        });
    }
    let mut decoder = ChatSseDecoder::new(on_event, attempt_started_at);
    loop {
        let chunk = match block_on_provider_future(
            runtime,
            cancellation,
            "provider_response_body_read_failed",
            ProviderErrorStage::ResponseBodyRead,
            request_timeout_seconds,
            || response.chunk(),
        ) {
            Ok(chunk) => chunk,
            Err(error) => {
                return Err(StreamAttemptFailure {
                    error,
                    emitted_text_delta: decoder.emitted_text_delta,
                    time_to_first_text_delta_ms: decoder.time_to_first_text_delta_ms,
                });
            }
        };
        let Some(chunk) = chunk else { break };
        if let Err(error) = decoder.push(&chunk) {
            return Err(StreamAttemptFailure {
                error,
                emitted_text_delta: decoder.emitted_text_delta,
                time_to_first_text_delta_ms: decoder.time_to_first_text_delta_ms,
            });
        }
    }
    match decoder.finish() {
        Ok(payload) => Ok(StreamAttemptSuccess {
            payload,
            time_to_first_text_delta_ms: decoder.time_to_first_text_delta_ms,
        }),
        Err(error) => Err(StreamAttemptFailure {
            error,
            emitted_text_delta: decoder.emitted_text_delta,
            time_to_first_text_delta_ms: decoder.time_to_first_text_delta_ms,
        }),
    }
}

pub struct ChatToolAccumulator {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

/// Incremental, total-size-bounded Chat SSE decoder. It emits only visible
/// content deltas; reasoning and tool-call fragments remain provider-private
/// until the final normalized response is parsed.
pub struct ChatSseDecoder<'a> {
    pending: Vec<u8>,
    event_data: Vec<u8>,
    event_name: Option<String>,
    total_bytes: usize,
    response_id: Option<String>,
    content: String,
    reasoning_content: String,
    tool_calls: BTreeMap<usize, ChatToolAccumulator>,
    finish_reason: Option<String>,
    usage: Option<Value>,
    saw_choice: bool,
    done: bool,
    pub emitted_text_delta: bool,
    attempt_started_at: Instant,
    pub time_to_first_text_delta_ms: Option<u64>,
    on_event: &'a mut dyn FnMut(ProviderStreamEvent),
}

impl<'a> ChatSseDecoder<'a> {
    pub fn new(
        on_event: &'a mut dyn FnMut(ProviderStreamEvent),
        attempt_started_at: Instant,
    ) -> Self {
        Self {
            pending: Vec::new(),
            event_data: Vec::new(),
            event_name: None,
            total_bytes: 0,
            response_id: None,
            content: String::new(),
            reasoning_content: String::new(),
            tool_calls: BTreeMap::new(),
            finish_reason: None,
            usage: None,
            saw_choice: false,
            done: false,
            emitted_text_delta: false,
            attempt_started_at,
            time_to_first_text_delta_ms: None,
            on_event,
        }
    }

    pub fn push(&mut self, chunk: &[u8]) -> Result<(), ProviderError> {
        self.total_bytes = self
            .total_bytes
            .checked_add(chunk.len())
            .ok_or_else(provider_response_stream_too_large_error)?;
        if self.total_bytes > MAX_PROVIDER_RESPONSE_BODY_BYTES {
            return Err(provider_response_stream_too_large_error());
        }
        self.pending.extend_from_slice(chunk);
        while let Some(newline) = self.pending.iter().position(|byte| *byte == b'\n') {
            let mut line = self.pending.drain(..=newline).collect::<Vec<_>>();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            self.process_line(&line)?;
        }
        Ok(())
    }

    fn process_line(&mut self, line: &[u8]) -> Result<(), ProviderError> {
        if line.is_empty() {
            return self.dispatch_event();
        }
        if line.first() == Some(&b':') {
            return Ok(());
        }
        let (field, value) = if let Some(separator) = line.iter().position(|byte| *byte == b':') {
            let value = line.get(separator + 1..).unwrap_or_default();
            let value = if value.first() == Some(&b' ') {
                value.get(1..).unwrap_or_default()
            } else {
                value
            };
            (line.get(..separator).unwrap_or_default(), value)
        } else {
            (line, &[] as &[u8])
        };
        match field {
            b"data" => {
                let additional = value.len().saturating_add(1);
                if self.event_data.len().saturating_add(additional)
                    > MAX_PROVIDER_RESPONSE_BODY_BYTES
                {
                    return Err(provider_response_stream_too_large_error());
                }
                if !self.event_data.is_empty() {
                    self.event_data.push(b'\n');
                }
                self.event_data.extend_from_slice(value);
            }
            b"event" => {
                let event = std::str::from_utf8(value)
                    .map_err(|_| provider_chat_stream_malformed_error("event_name_invalid"))?;
                self.event_name = Some(event.to_string());
            }
            b"id" | b"retry" => {}
            _ => {}
        }
        Ok(())
    }

    fn dispatch_event(&mut self) -> Result<(), ProviderError> {
        if self.event_data.is_empty() {
            self.event_name = None;
            return Ok(());
        }
        let raw = std::str::from_utf8(&self.event_data)
            .map_err(|_| provider_chat_stream_malformed_error("event_data_invalid_utf8"))?
            .trim()
            .to_string();
        self.event_data.clear();
        self.event_name = None;
        if raw == "[DONE]" {
            if self.done {
                return Err(provider_chat_stream_malformed_error("event_after_done"));
            }
            self.done = true;
            return Ok(());
        }
        if self.done {
            return Err(provider_chat_stream_malformed_error("event_after_done"));
        }
        let payload = serde_json::from_str::<Value>(&raw)
            .map_err(|_| provider_chat_stream_malformed_error("event_data_invalid_json"))?;
        if payload.get("error").is_some_and(|error| !error.is_null()) {
            return Err(provider_chat_stream_terminal_error(
                "chat_stream_error",
                "provider Chat stream returned an error",
            ));
        }
        if let Some(id) = payload.get("id").and_then(Value::as_str)
            && self.response_id.is_none()
        {
            self.response_id = Some(id.to_string());
        }
        if let Some(usage) = payload.get("usage").filter(|value| value.is_object()) {
            self.usage = Some(usage.clone());
        }
        let Some(choices) = payload.get("choices").and_then(Value::as_array) else {
            // A usage-only chunk is legal in the OpenAI include_usage extension.
            return Ok(());
        };
        for choice in choices {
            let index = choice.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
            if index != 0 {
                return Err(provider_chat_stream_malformed_error(
                    "multiple_choices_unsupported",
                ));
            }
            self.saw_choice = true;
            if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
                self.finish_reason = Some(reason.to_string());
            }
            let Some(delta) = choice.get("delta").and_then(Value::as_object) else {
                continue;
            };
            if let Some(text) = delta.get("content").and_then(Value::as_str)
                && !text.is_empty()
            {
                if self.time_to_first_text_delta_ms.is_none() {
                    self.time_to_first_text_delta_ms =
                        Some(duration_millis(self.attempt_started_at.elapsed()));
                }
                self.emitted_text_delta = true;
                self.content.push_str(text);
                (self.on_event)(ProviderStreamEvent::OutputTextDelta {
                    delta: text.to_string(),
                });
            }
            for key in ["reasoning_content", "reasoning"] {
                if let Some(reasoning) = delta.get(key).and_then(Value::as_str) {
                    self.reasoning_content.push_str(reasoning);
                }
            }
            if let Some(tool_calls) = delta.get("tool_calls").and_then(Value::as_array) {
                for call in tool_calls {
                    let index = call.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                    let entry =
                        self.tool_calls
                            .entry(index)
                            .or_insert_with(|| ChatToolAccumulator {
                                id: String::new(),
                                name: String::new(),
                                arguments: String::new(),
                            });
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
                }
            }
        }
        Ok(())
    }

    pub fn finish(&mut self) -> Result<Value, ProviderError> {
        if !self.pending.is_empty() || !self.event_data.is_empty() || self.event_name.is_some() {
            return Err(provider_chat_stream_malformed_error(
                "event_frame_unterminated",
            ));
        }
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
            .clone()
            .ok_or_else(|| provider_chat_stream_malformed_error("finish_reason_missing"))?;
        let mut message = serde_json::Map::new();
        message.insert("role".to_string(), Value::String("assistant".to_string()));
        let content = if self.content.is_empty() && !self.tool_calls.is_empty() {
            Value::Null
        } else {
            Value::String(self.content.clone())
        };
        message.insert("content".to_string(), content);
        if !self.reasoning_content.is_empty() {
            message.insert(
                "reasoning_content".to_string(),
                Value::String(self.reasoning_content.clone()),
            );
        }
        if !self.tool_calls.is_empty() {
            let calls = self
                .tool_calls
                .values()
                .map(|call| {
                    serde_json::json!({
                        "id": call.id,
                        "type": "function",
                        "function": {"name": call.name, "arguments": call.arguments},
                    })
                })
                .collect::<Vec<_>>();
            message.insert("tool_calls".to_string(), Value::Array(calls));
        }
        let choice = serde_json::json!({"index": 0, "message": Value::Object(message), "finish_reason": finish_reason});
        let mut payload = serde_json::json!({
            "id": self.response_id.clone().unwrap_or_else(|| "chat_stream".to_string()),
            "choices": [choice],
        });
        if let Some(usage) = self.usage.clone() {
            payload["usage"] = usage;
        }
        Ok(payload)
    }
}

/// Decode one Responses body while preserving arbitrary HTTP chunk and SSE frame boundaries.
pub fn read_openai_responses_sse(
    runtime: &ProviderRuntime,
    cancellation: &CancellationToken,
    request_timeout_seconds: u64,
    mut response: Response,
    on_event: &mut dyn FnMut(ProviderStreamEvent),
    attempt_started_at: Instant,
) -> Result<StreamAttemptSuccess, StreamAttemptFailure> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_PROVIDER_RESPONSE_BODY_BYTES as u64)
    {
        return Err(StreamAttemptFailure {
            error: provider_response_stream_too_large_error(),
            emitted_text_delta: false,
            time_to_first_text_delta_ms: None,
        });
    }
    let mut decoder = ResponsesSseDecoder::new(on_event, attempt_started_at);
    loop {
        let chunk = match block_on_provider_future(
            runtime,
            cancellation,
            "provider_response_body_read_failed",
            ProviderErrorStage::ResponseBodyRead,
            request_timeout_seconds,
            || response.chunk(),
        ) {
            Ok(chunk) => chunk,
            Err(error) => {
                return Err(StreamAttemptFailure {
                    error,
                    emitted_text_delta: decoder.emitted_text_delta,
                    time_to_first_text_delta_ms: decoder.time_to_first_text_delta_ms,
                });
            }
        };
        let Some(chunk) = chunk else {
            break;
        };
        if let Err(error) = decoder.push(&chunk) {
            return Err(StreamAttemptFailure {
                error,
                emitted_text_delta: decoder.emitted_text_delta,
                time_to_first_text_delta_ms: decoder.time_to_first_text_delta_ms,
            });
        }
    }
    match decoder.finish() {
        Ok(payload) => Ok(StreamAttemptSuccess {
            payload,
            time_to_first_text_delta_ms: decoder.time_to_first_text_delta_ms,
        }),
        Err(error) => Err(StreamAttemptFailure {
            error,
            emitted_text_delta: decoder.emitted_text_delta,
            time_to_first_text_delta_ms: decoder.time_to_first_text_delta_ms,
        }),
    }
}

/// Incremental, total-size-bounded SSE decoder for the Responses event contract.
pub struct ResponsesSseDecoder<'a> {
    pending: Vec<u8>,
    event_data: Vec<u8>,
    event_name: Option<String>,
    total_bytes: usize,
    terminal_response: Option<Value>,
    pub emitted_text_delta: bool,
    attempt_started_at: Instant,
    pub time_to_first_text_delta_ms: Option<u64>,
    on_event: &'a mut dyn FnMut(ProviderStreamEvent),
}

impl<'a> ResponsesSseDecoder<'a> {
    pub fn new(
        on_event: &'a mut dyn FnMut(ProviderStreamEvent),
        attempt_started_at: Instant,
    ) -> Self {
        Self {
            pending: Vec::new(),
            event_data: Vec::new(),
            event_name: None,
            total_bytes: 0,
            terminal_response: None,
            emitted_text_delta: false,
            attempt_started_at,
            time_to_first_text_delta_ms: None,
            on_event,
        }
    }

    pub fn push(&mut self, chunk: &[u8]) -> Result<(), ProviderError> {
        self.total_bytes = self
            .total_bytes
            .checked_add(chunk.len())
            .ok_or_else(provider_response_stream_too_large_error)?;
        if self.total_bytes > MAX_PROVIDER_RESPONSE_BODY_BYTES {
            return Err(provider_response_stream_too_large_error());
        }
        self.pending.extend_from_slice(chunk);
        while let Some(newline) = self.pending.iter().position(|byte| *byte == b'\n') {
            let mut line = self.pending.drain(..=newline).collect::<Vec<_>>();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            self.process_line(&line)?;
        }
        Ok(())
    }

    fn process_line(&mut self, line: &[u8]) -> Result<(), ProviderError> {
        if line.is_empty() {
            return self.dispatch_event();
        }
        if line.first() == Some(&b':') {
            return Ok(());
        }
        let (field, value) = if let Some(separator) = line.iter().position(|byte| *byte == b':') {
            let value = line.get(separator + 1..).unwrap_or_default();
            let value = if value.first() == Some(&b' ') {
                value.get(1..).unwrap_or_default()
            } else {
                value
            };
            (line.get(..separator).unwrap_or_default(), value)
        } else {
            (line, &[] as &[u8])
        };
        match field {
            b"data" => {
                let additional = value.len().saturating_add(1);
                if self.event_data.len().saturating_add(additional)
                    > MAX_PROVIDER_RESPONSE_BODY_BYTES
                {
                    return Err(provider_response_stream_too_large_error());
                }
                if !self.event_data.is_empty() {
                    self.event_data.push(b'\n');
                }
                self.event_data.extend_from_slice(value);
            }
            b"event" => {
                let event = std::str::from_utf8(value)
                    .map_err(|_| provider_responses_stream_malformed_error("event_name_invalid"))?;
                self.event_name = Some(event.to_string());
            }
            b"id" | b"retry" => {}
            _ => {}
        }
        Ok(())
    }

    fn dispatch_event(&mut self) -> Result<(), ProviderError> {
        if self.event_data.is_empty() {
            self.event_name = None;
            return Ok(());
        }
        let payload = serde_json::from_slice::<Value>(&self.event_data)
            .map_err(|_| provider_responses_stream_malformed_error("event_data_invalid_json"))?;
        self.event_data.clear();
        let payload_type = payload
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| provider_responses_stream_malformed_error("event_type_missing"))?;
        if self
            .event_name
            .as_deref()
            .is_some_and(|event_name| event_name != payload_type)
        {
            return Err(provider_responses_stream_malformed_error(
                "event_type_mismatch",
            ));
        }
        self.event_name = None;
        if payload_type == "ping" {
            return Ok(());
        }
        if self.terminal_response.is_some() {
            return Err(provider_responses_stream_malformed_error(
                "event_after_terminal",
            ));
        }
        match payload_type {
            "response.output_text.delta" => {
                let delta = payload
                    .get("delta")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        provider_responses_stream_malformed_error("output_text_delta_missing")
                    })?;
                if !delta.is_empty() {
                    if self.time_to_first_text_delta_ms.is_none() {
                        self.time_to_first_text_delta_ms =
                            Some(duration_millis(self.attempt_started_at.elapsed()));
                    }
                    self.emitted_text_delta = true;
                    (self.on_event)(ProviderStreamEvent::OutputTextDelta {
                        delta: delta.to_string(),
                    });
                }
            }
            "response.completed" => {
                let response = payload.get("response").cloned().ok_or_else(|| {
                    provider_responses_stream_malformed_error("completed_response_missing")
                })?;
                if !response.is_object() {
                    return Err(provider_responses_stream_malformed_error(
                        "completed_response_invalid",
                    ));
                }
                self.terminal_response = Some(response);
            }
            "error" => {
                return Err(provider_responses_stream_terminal_error(
                    "responses_stream_error",
                    "provider Responses stream returned an error",
                ));
            }
            "response.failed" => {
                return Err(provider_responses_stream_terminal_error(
                    "responses_stream_failed",
                    "provider Responses stream failed",
                ));
            }
            "response.incomplete" => {
                // The response object is still the authoritative partial fact.
                // `parse_openai_responses_response` maps max_output_tokens to
                // the typed length stop reason; other incomplete reasons fail
                // closed there without discarding visible/tool fragments.
                let response = payload.get("response").cloned().ok_or_else(|| {
                    provider_responses_stream_malformed_error("incomplete_response_missing")
                })?;
                if !response.is_object() {
                    return Err(provider_responses_stream_malformed_error(
                        "incomplete_response_invalid",
                    ));
                }
                self.terminal_response = Some(response);
            }
            _ => {}
        }
        Ok(())
    }

    pub fn finish(&mut self) -> Result<Value, ProviderError> {
        if !self.pending.is_empty() || !self.event_data.is_empty() || self.event_name.is_some() {
            return Err(provider_responses_stream_malformed_error(
                "event_frame_unterminated",
            ));
        }
        self.terminal_response
            .clone()
            .ok_or_else(provider_responses_stream_terminal_missing_error)
    }
}

pub fn provider_chat_stream_malformed_error(reason: &'static str) -> ProviderError {
    let mut error = ModelError::new(
        ModelErrorKind::JsonSchemaViolation,
        "provider Chat stream was malformed",
    )
    .with_provider_diagnostic(
        "chat_stream_malformed",
        ProviderErrorStage::ResponseValidation,
    );
    error.validation_errors.push(reason.to_string());
    ProviderError::from_model_error(error)
}

pub fn provider_chat_stream_terminal_error(code: &'static str, message: &'static str) -> ProviderError {
    let mut error = ModelError::new(ModelErrorKind::UnknownProviderError, message)
        .with_provider_diagnostic(code, ProviderErrorStage::ResponseValidation);
    error.validation_errors.push(code.to_string());
    ProviderError::from_model_error(error)
}

pub fn provider_responses_stream_malformed_error(reason: &'static str) -> ProviderError {
    let mut error = ModelError::new(
        ModelErrorKind::JsonSchemaViolation,
        "provider Responses stream was malformed",
    )
    .with_provider_diagnostic(
        "responses_stream_malformed",
        ProviderErrorStage::ResponseValidation,
    );
    error.validation_errors.push(reason.to_string());
    ProviderError::from_model_error(error)
}

pub fn provider_responses_stream_terminal_missing_error() -> ProviderError {
    let mut error = ModelError::new(
        ModelErrorKind::JsonSchemaViolation,
        "provider Responses stream did not contain a completed terminal",
    )
    .with_provider_diagnostic(
        "responses_stream_terminal_missing",
        ProviderErrorStage::ResponseValidation,
    );
    error
        .validation_errors
        .push("responses_stream_terminal_missing".to_string());
    ProviderError::from_model_error(error)
}

pub fn provider_responses_stream_terminal_error(
    code: &'static str,
    message: &'static str,
) -> ProviderError {
    let mut error = ModelError::new(ModelErrorKind::UnknownProviderError, message)
        .with_provider_diagnostic(code, ProviderErrorStage::ResponseValidation);
    error.validation_errors.push(code.to_string());
    ProviderError::from_model_error(error)
}

pub fn provider_response_stream_too_large_error() -> ProviderError {
    let mut error = ModelError::new(
        ModelErrorKind::JsonSchemaViolation,
        "provider Responses stream exceeded the fixed safety limit",
    )
    .with_provider_diagnostic(
        "provider_response_stream_too_large",
        ProviderErrorStage::ResponseBodyRead,
    );
    error
        .validation_errors
        .push("provider_response_stream_too_large".to_string());
    ProviderError::from_model_error(error)
}
