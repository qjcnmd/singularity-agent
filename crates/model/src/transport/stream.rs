//! SSE 流式解码器（Chat Completions 与 Responses 格式）。

use std::collections::BTreeMap;

use reqwest::Response;
use serde_json::Value;
use singularity_core::CancellationToken;

use crate::MAX_PROVIDER_RESPONSE_BODY_BYTES;
use crate::error::{ModelError, ModelErrorKind, ProviderError, ProviderErrorStage};
use crate::provider::telemetry::ProviderStreamEvent;
use crate::transport::http::{provider_cancelled_error, provider_transport_error};

/// 流 attempt 错误 + 重试是否可能重复可见文本。
pub(super) struct StreamAttemptFailure {
    pub(super) error: ProviderError,
    pub(super) emitted_text_delta: bool,
}

/// 一次完成的流解码。
pub(super) struct StreamAttemptSuccess {
    pub(super) payload: Value,
}

struct SseFrame {
    event_name: Option<String>,
    data: Vec<u8>,
}

/// 协议无关的增量 SSE 帧切分，带单一总字节上限。
#[derive(Default)]
struct SseFrameDecoder {
    pending: Vec<u8>,
    event_data: Vec<u8>,
    event_name: Option<String>,
    total_bytes: usize,
}

impl SseFrameDecoder {
    fn push(
        &mut self,
        chunk: &[u8],
        malformed: fn(&'static str) -> ProviderError,
    ) -> Result<Vec<SseFrame>, ProviderError> {
        self.total_bytes = self
            .total_bytes
            .checked_add(chunk.len())
            .ok_or_else(provider_response_stream_too_large_error)?;
        if self.total_bytes > MAX_PROVIDER_RESPONSE_BODY_BYTES {
            return Err(provider_response_stream_too_large_error());
        }
        self.pending.extend_from_slice(chunk);
        let mut frames = Vec::new();
        let Some(last_newline) = self.pending.iter().rposition(|byte| *byte == b'\n') else {
            return Ok(frames);
        };
        let tail = self.pending.split_off(last_newline + 1);
        let complete = std::mem::replace(&mut self.pending, tail);
        for terminated_line in complete.split_inclusive(|byte| *byte == b'\n') {
            let mut line = terminated_line
                .strip_suffix(b"\n")
                .unwrap_or(terminated_line);
            if line.last() == Some(&b'\r') {
                line = &line[..line.len() - 1];
            }
            if let Some(frame) = self.process_line(line, malformed)? {
                frames.push(frame);
            }
        }
        Ok(frames)
    }

    fn process_line(
        &mut self,
        line: &[u8],
        malformed: fn(&'static str) -> ProviderError,
    ) -> Result<Option<SseFrame>, ProviderError> {
        if line.is_empty() {
            if self.event_data.is_empty() {
                self.event_name = None;
                return Ok(None);
            }
            return Ok(Some(SseFrame {
                event_name: self.event_name.take(),
                data: std::mem::take(&mut self.event_data),
            }));
        }
        if line.first() == Some(&b':') {
            return Ok(None);
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
                let event =
                    std::str::from_utf8(value).map_err(|_| malformed("event_name_invalid"))?;
                self.event_name = Some(event.to_string());
            }
            b"id" | b"retry" => {}
            _ => {}
        }
        Ok(None)
    }

    fn finish(&self, malformed: fn(&'static str) -> ProviderError) -> Result<(), ProviderError> {
        if !self.pending.is_empty() || !self.event_data.is_empty() || self.event_name.is_some() {
            return Err(malformed("event_frame_unterminated"));
        }
        Ok(())
    }
}

/// 流式解码器的统一读取契约：`read_sse_stream` 以该 trait 泛型驱动 chunk
/// 循环；协议差异只保留在 `push`/`finish` 与各自的 malformed 构造器里。
trait SseStreamDecoder {
    fn push(&mut self, chunk: &[u8]) -> Result<(), ProviderError>;
    fn finish(&mut self) -> Result<Value, ProviderError>;
    fn emitted_text_delta(&self) -> bool;
}

/// 通用流读取循环：保留任意 HTTP chunk 与 SSE 帧边界，失败路径携带
/// 解码器边界快照（是否已发射文本增量）。
fn read_sse_stream<D: SseStreamDecoder>(
    runtime: &tokio::runtime::Handle,
    cancellation: &CancellationToken,
    mut response: Response,
    mut decoder: D,
) -> Result<StreamAttemptSuccess, StreamAttemptFailure> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_PROVIDER_RESPONSE_BODY_BYTES as u64)
    {
        return Err(StreamAttemptFailure {
            error: provider_response_stream_too_large_error(),
            emitted_text_delta: false,
        });
    }

    if cancellation.is_cancelled() {
        return Err(StreamAttemptFailure {
            error: provider_cancelled_error(),
            emitted_text_delta: false,
        });
    }

    let stream_result = runtime.block_on(async {
        loop {
            let chunk = tokio::select! {
                _ = cancellation.cancelled_notified() => {
                    return Err(provider_cancelled_error());
                }
                result = response.chunk() => {
                    match result {
                        Ok(Some(chunk)) => chunk,
                        Ok(None) => break,
                        Err(error) => {
                            return Err(provider_transport_error(
                                error,
                                "provider_response_body_read_failed",
                                ProviderErrorStage::ResponseBodyRead,
                            ));
                        }
                    }
                }
            };
            decoder.push(&chunk)?;
        }
        decoder.finish()
    });

    match stream_result {
        Ok(payload) => Ok(StreamAttemptSuccess { payload }),
        Err(error) => Err(StreamAttemptFailure {
            error,
            emitted_text_delta: decoder.emitted_text_delta(),
        }),
    }
}

/// 解码一个 Chat Completions body，保留任意 HTTP chunk 与 SSE 帧边界。
pub(super) fn read_openai_chat_sse(
    runtime: &tokio::runtime::Handle,
    cancellation: &CancellationToken,
    response: Response,
    on_event: &mut dyn FnMut(ProviderStreamEvent),
) -> Result<StreamAttemptSuccess, StreamAttemptFailure> {
    read_sse_stream(
        runtime,
        cancellation,
        response,
        ChatSseDecoder::new(on_event),
    )
}

pub(super) struct ChatToolAccumulator {
    pub(super) id: String,
    pub(super) name: String,
    pub(super) arguments: String,
}

/// 增量、总量有界的 Chat SSE 解码器。只发射可见内容增量；reasoning 与
/// 工具调用片段保持 provider 私有，直到最终规范化响应解析。
pub(super) struct ChatSseDecoder<'a> {
    frames: SseFrameDecoder,
    response_id: Option<String>,
    content: String,
    reasoning_content: String,
    tool_calls: BTreeMap<usize, ChatToolAccumulator>,
    finish_reason: Option<String>,
    usage: Option<Value>,
    saw_choice: bool,
    done: bool,
    pub(super) emitted_text_delta: bool,
    on_event: &'a mut dyn FnMut(ProviderStreamEvent),
}

impl SseStreamDecoder for ChatSseDecoder<'_> {
    fn push(&mut self, chunk: &[u8]) -> Result<(), ProviderError> {
        for frame in self
            .frames
            .push(chunk, provider_chat_stream_malformed_error)?
        {
            self.dispatch_event(frame)?;
        }
        Ok(())
    }

    fn finish(&mut self) -> Result<Value, ProviderError> {
        ChatSseDecoder::finish(self)
    }

    fn emitted_text_delta(&self) -> bool {
        self.emitted_text_delta
    }
}

impl<'a> ChatSseDecoder<'a> {
    pub(super) fn new(
        on_event: &'a mut dyn FnMut(ProviderStreamEvent),
    ) -> Self {
        Self {
            frames: SseFrameDecoder::default(),
            response_id: None,
            content: String::new(),
            reasoning_content: String::new(),
            tool_calls: BTreeMap::new(),
            finish_reason: None,
            usage: None,
            saw_choice: false,
            done: false,
            emitted_text_delta: false,
            on_event,
        }
    }

    fn dispatch_event(&mut self, frame: SseFrame) -> Result<(), ProviderError> {
        let raw = std::str::from_utf8(&frame.data)
            .map_err(|_| provider_chat_stream_malformed_error("event_data_invalid_utf8"))?
            .trim()
            .to_string();
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
            return Err(provider_stream_terminal_error(
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
            // 仅有 usage 的块在 OpenAI include_usage 扩展中是合法的。
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
        self.frames.finish(provider_chat_stream_malformed_error)?;
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

/// 解码一个 Responses body，保留任意 HTTP chunk 与 SSE 帧边界。
pub(super) fn read_openai_responses_sse(
    runtime: &tokio::runtime::Handle,
    cancellation: &CancellationToken,
    response: Response,
    on_event: &mut dyn FnMut(ProviderStreamEvent),
) -> Result<StreamAttemptSuccess, StreamAttemptFailure> {
    read_sse_stream(
        runtime,
        cancellation,
        response,
        ResponsesSseDecoder::new(on_event),
    )
}

/// 增量、总量有界的 Responses 事件契约 SSE 解码器。
pub struct ResponsesSseDecoder<'a> {
    frames: SseFrameDecoder,
    terminal_response: Option<Value>,
    pub emitted_text_delta: bool,
    on_event: &'a mut dyn FnMut(ProviderStreamEvent),
}

impl SseStreamDecoder for ResponsesSseDecoder<'_> {
    fn push(&mut self, chunk: &[u8]) -> Result<(), ProviderError> {
        for frame in self
            .frames
            .push(chunk, provider_responses_stream_malformed_error)?
        {
            self.dispatch_event(frame)?;
        }
        Ok(())
    }

    fn finish(&mut self) -> Result<Value, ProviderError> {
        ResponsesSseDecoder::finish(self)
    }

    fn emitted_text_delta(&self) -> bool {
        self.emitted_text_delta
    }
}

impl<'a> ResponsesSseDecoder<'a> {
    pub fn new(on_event: &'a mut dyn FnMut(ProviderStreamEvent)) -> Self {
        Self {
            frames: SseFrameDecoder::default(),
            terminal_response: None,
            emitted_text_delta: false,
            on_event,
        }
    }

    fn dispatch_event(&mut self, frame: SseFrame) -> Result<(), ProviderError> {
        let payload = serde_json::from_slice::<Value>(&frame.data)
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
                return Err(provider_stream_terminal_error(
                    "responses_stream_error",
                    "provider Responses stream returned an error",
                ));
            }
            "response.failed" => {
                return Err(provider_stream_terminal_error(
                    "responses_stream_failed",
                    "provider Responses stream failed",
                ));
            }
            "response.incomplete" => {
                // response 对象仍是权威的部分事实；`parse_openai_responses_response`
                // 把 max_output_tokens 映射为类型化 length 终止原因；其他不完整
                // 原因在此 fail closed，不丢弃可见/工具片段。
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
        self.frames
            .finish(provider_responses_stream_malformed_error)?;
        self.terminal_response
            .clone()
            .ok_or_else(provider_responses_stream_terminal_missing_error)
    }
}

/// malformed 构造器的统一核心：协议字面词保留在薄包装里，构造体只写一次。
fn provider_stream_malformed_error(
    message: &'static str,
    code: &'static str,
    reason: &'static str,
) -> ProviderError {
    let mut error = ModelError::new(ModelErrorKind::JsonSchemaViolation, message)
        .with_provider_diagnostic(code, ProviderErrorStage::ResponseValidation);
    error.validation_errors.push(reason.to_string());
    ProviderError::from_model_error(error)
}

pub fn provider_chat_stream_malformed_error(reason: &'static str) -> ProviderError {
    provider_stream_malformed_error(
        "provider Chat stream was malformed",
        "chat_stream_malformed",
        reason,
    )
}

pub(super) fn provider_responses_stream_malformed_error(reason: &'static str) -> ProviderError {
    provider_stream_malformed_error(
        "provider Responses stream was malformed",
        "responses_stream_malformed",
        reason,
    )
}

pub(super) fn provider_stream_terminal_error(
    code: &'static str,
    message: &'static str,
) -> ProviderError {
    let mut error = ModelError::new(ModelErrorKind::UnknownProviderError, message)
        .with_provider_diagnostic(code, ProviderErrorStage::ResponseValidation);
    error.validation_errors.push(code.to_string());
    ProviderError::from_model_error(error)
}

pub(super) fn provider_responses_stream_terminal_missing_error() -> ProviderError {
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

pub(super) fn provider_response_stream_too_large_error() -> ProviderError {
    let mut error = ModelError::new(
        ModelErrorKind::JsonSchemaViolation,
        "provider stream exceeded the fixed safety limit",
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

#[cfg(test)]
mod frame_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)] // 测试断言惯例
    use super::*;

    #[test]
    fn one_chunk_with_many_frames_is_split_in_source_order() {
        let mut chunk = Vec::new();
        for index in 0..4096 {
            chunk.extend_from_slice(format!("data: {index}\n\n").as_bytes());
        }
        let frames = SseFrameDecoder::default()
            .push(&chunk, provider_chat_stream_malformed_error)
            .expect("decode frames");
        assert_eq!(frames.len(), 4096);
        assert_eq!(
            frames.first().map(|frame| frame.data.as_slice()),
            Some(b"0".as_slice())
        );
        assert_eq!(
            frames.last().map(|frame| frame.data.as_slice()),
            Some(b"4095".as_slice())
        );
    }
}
