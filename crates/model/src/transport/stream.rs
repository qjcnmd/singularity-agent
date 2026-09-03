//! SSE 流式解码器（Chat Completions 与 Responses 格式）。

use std::collections::BTreeMap;

use reqwest::Response;
use serde_json::Value;
use singularity_core::CancellationToken;

use crate::MAX_PROVIDER_RESPONSE_BODY_BYTES;
use crate::error::{ModelError, ModelErrorKind, ProviderError, ProviderErrorStage};
use crate::provider::telemetry::ProviderStreamEvent;
use crate::transport::ProtocolAdapter;
use crate::transport::http::{
    provider_cancelled_error, provider_embedded_error, provider_error_fields,
    provider_transport_error,
};

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
/// 循环。chunk→帧的泵（`push`）与终态前的帧边界校验（`finish`）由默认实现
/// 收敛；协议差异只保留在 malformed 构造器、单帧分派与终态物化里。
/// 只作泛型约束使用（无 trait 对象），`Sized` 供默认方法调用关联构造器。
trait SseStreamDecoder: Sized {
    /// 该协议的 malformed 构造器（帧边界失败的稳定词形）。
    fn frame_malformed() -> fn(&'static str) -> ProviderError
    where
        Self: Sized;

    /// 单帧协议分派。
    fn dispatch_event(&mut self, frame: SseFrame) -> Result<(), ProviderError>;

    /// 终态物化：帧边界已校验后由默认 `finish` 调用。
    fn materialize_terminal(&mut self) -> Result<Value, ProviderError>;

    /// 是否已发射可见文本增量（失败路径的边界快照）。
    fn emitted_text_delta(&self) -> bool;

    /// 解码器持有的帧边界解码器（默认 `push`/`finish` 的共享输入）。
    fn sse_frames(&mut self) -> &mut SseFrameDecoder;

    fn push(&mut self, chunk: &[u8]) -> Result<(), ProviderError> {
        for frame in self.sse_frames().push(chunk, Self::frame_malformed())? {
            self.dispatch_event(frame)?;
        }
        Ok(())
    }

    fn finish(&mut self) -> Result<Value, ProviderError> {
        self.sse_frames().finish(Self::frame_malformed())?;
        self.materialize_terminal()
    }
}

/// 字节泵与解码消费之间的通道容量：有界背压同时防解码侧失控内存。
const SSE_CHUNK_CHANNEL_CAPACITY: usize = 8;

/// 通用流读取循环：保留任意 HTTP chunk 与 SSE 帧边界，失败路径携带
/// 解码器边界快照（是否已发射文本增量）。
///
/// HTTP chunk 由 runtime 上的字节泵任务读取，经有界通道交给本线程解码；
/// 解码回调（及其触发的同步事件出口）从不进入 `block_on` 的运行时上下文，
/// 调用线程因此能在通道背压上阻塞等待。
fn read_sse_stream<D: SseStreamDecoder>(
    runtime: &tokio::runtime::Handle,
    cancellation: &CancellationToken,
    mut response: Response,
    mut decoder: D,
) -> Result<Value, ProviderError> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_PROVIDER_RESPONSE_BODY_BYTES as u64)
    {
        return Err(provider_response_stream_too_large_error());
    }

    if cancellation.is_cancelled() {
        return Err(provider_cancelled_error());
    }

    let pump_cancellation = cancellation.clone();
    let (chunk_tx, mut chunk_rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, ProviderError>>(
        SSE_CHUNK_CHANNEL_CAPACITY,
    );
    runtime.spawn(async move {
        loop {
            tokio::select! {
                _ = pump_cancellation.cancelled_notified() => {
                    let _ = chunk_tx.send(Err(provider_cancelled_error())).await;
                    return;
                }
                result = response.chunk() => match result {
                    Ok(Some(chunk)) => {
                        if chunk_tx.send(Ok(chunk)).await.is_err() {
                            return;
                        }
                    }
                    Ok(None) => return,
                    Err(error) => {
                        let _ = chunk_tx
                            .send(Err(provider_transport_error(
                                error,
                                "provider_response_body_read_failed",
                                ProviderErrorStage::ResponseBodyRead,
                            )))
                            .await;
                        return;
                    }
                }
            }
        }
    });

    let stream_result = loop {
        match chunk_rx.blocking_recv() {
            Some(Ok(chunk)) => {
                if cancellation.is_cancelled() {
                    break Err(provider_cancelled_error());
                }
                if let Err(error) = decoder.push(&chunk) {
                    break Err(error);
                }
            }
            Some(Err(error)) => break Err(error),
            None => break decoder.finish(),
        }
    };

    stream_result.map_err(|error| {
        if decoder.emitted_text_delta() {
            error.without_automatic_retry()
        } else {
            error
        }
    })
}

/// 按已选 wire 协议解码 SSE body，保留任意 HTTP chunk 与帧边界。
pub(super) fn read_openai_sse(
    adapter: ProtocolAdapter,
    runtime: &tokio::runtime::Handle,
    cancellation: &CancellationToken,
    response: Response,
    on_event: &mut dyn FnMut(ProviderStreamEvent),
) -> Result<Value, ProviderError> {
    match adapter {
        ProtocolAdapter::Chat => read_sse_stream(
            runtime,
            cancellation,
            response,
            ChatSseDecoder::new(on_event),
        ),
        ProtocolAdapter::Responses => read_sse_stream(
            runtime,
            cancellation,
            response,
            ResponsesSseDecoder::new(on_event),
        ),
    }
}

#[derive(Default)]
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
    on_event: &'a mut dyn FnMut(ProviderStreamEvent),
}

impl SseStreamDecoder for ChatSseDecoder<'_> {
    fn frame_malformed() -> fn(&'static str) -> ProviderError {
        provider_chat_stream_malformed_error
    }

    fn dispatch_event(&mut self, frame: SseFrame) -> Result<(), ProviderError> {
        let raw = std::str::from_utf8(&frame.data)
            .map_err(|_| provider_chat_stream_malformed_error("event_data_invalid_utf8"))?
            .trim()
            .to_string();
        // [DONE] 是流终点：此后到达的尾帧（如网关追加的计费帧）
        // 不参与终态物化，一律忽略。
        if raw == "[DONE]" {
            self.done = true;
            return Ok(());
        }
        if self.done {
            return Ok(());
        }
        let payload = serde_json::from_str::<Value>(&raw)
            .map_err(|_| provider_chat_stream_malformed_error("event_data_invalid_json"))?;
        if let Some(error) = payload.get("error").filter(|error| !error.is_null()) {
            return Err(provider_embedded_error(
                &provider_error_fields(error),
                "provider Chat stream returned an error",
                "chat_stream_error",
                None,
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
                self.content.push_str(text);
                (self.on_event)(ProviderStreamEvent::OutputTextDelta {
                    delta: text.to_string(),
                });
            }
            // 兼容端点可能在同一块里用多个键携带相同 reasoning（实测
            // 双键同文）；按序取首个非空键，只累加一次。
            if let Some(reasoning) = ["reasoning_content", "reasoning", "reasoning_text"]
                .iter()
                .find_map(|key| {
                    delta
                        .get(*key)
                        .and_then(Value::as_str)
                        .filter(|text| !text.is_empty())
                })
            {
                self.reasoning_content.push_str(reasoning);
            }
            if let Some(tool_calls) = delta.get("tool_calls").and_then(Value::as_array) {
                for call in tool_calls {
                    let index = call.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
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
                }
            }
        }
        Ok(())
    }

    fn materialize_terminal(&mut self) -> Result<Value, ProviderError> {
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

    fn emitted_text_delta(&self) -> bool {
        !self.content.is_empty()
    }

    fn sse_frames(&mut self) -> &mut SseFrameDecoder {
        &mut self.frames
    }
}

impl<'a> ChatSseDecoder<'a> {
    pub(super) fn new(on_event: &'a mut dyn FnMut(ProviderStreamEvent)) -> Self {
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
            on_event,
        }
    }
}

/// 增量、总量有界的 Responses 事件契约 SSE 解码器。
pub(crate) struct ResponsesSseDecoder<'a> {
    frames: SseFrameDecoder,
    terminal_response: Option<Value>,
    pub emitted_text_delta: bool,
    on_event: &'a mut dyn FnMut(ProviderStreamEvent),
}

impl SseStreamDecoder for ResponsesSseDecoder<'_> {
    fn frame_malformed() -> fn(&'static str) -> ProviderError {
        provider_responses_stream_malformed_error
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
                let fields = payload
                    .get("error")
                    .map(provider_error_fields)
                    .unwrap_or_default();
                return Err(provider_embedded_error(
                    &fields,
                    "provider Responses stream returned an error",
                    "responses_stream_error",
                    None,
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
                    None,
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

    fn materialize_terminal(&mut self) -> Result<Value, ProviderError> {
        self.terminal_response
            .clone()
            .ok_or_else(provider_responses_stream_terminal_missing_error)
    }

    fn emitted_text_delta(&self) -> bool {
        self.emitted_text_delta
    }

    fn sse_frames(&mut self) -> &mut SseFrameDecoder {
        &mut self.frames
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
}

/// malformed 构造器的统一核心：协议字面词保留在薄包装里，构造体只写一次。
fn provider_stream_malformed_error(
    message: &'static str,
    code: &'static str,
    reason: &'static str,
) -> ProviderError {
    ProviderError::from_model_error(ModelError::diagnostic(
        ModelErrorKind::JsonSchemaViolation,
        message,
        code,
        ProviderErrorStage::ResponseValidation,
        vec![reason.to_string()],
    ))
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

pub(super) fn provider_responses_stream_terminal_missing_error() -> ProviderError {
    ProviderError::from_model_error(ModelError::diagnostic(
        ModelErrorKind::JsonSchemaViolation,
        "provider Responses stream did not contain a completed terminal",
        "responses_stream_terminal_missing",
        ProviderErrorStage::ResponseValidation,
        vec!["responses_stream_terminal_missing".to_string()],
    ))
}

pub(super) fn provider_response_stream_too_large_error() -> ProviderError {
    ProviderError::from_model_error(ModelError::diagnostic(
        ModelErrorKind::JsonSchemaViolation,
        "provider stream exceeded the fixed safety limit",
        "provider_response_stream_too_large",
        ProviderErrorStage::ResponseBodyRead,
        vec!["provider_response_stream_too_large".to_string()],
    ))
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

    /// 兼容端点在同一 delta 里以多个键携带相同 reasoning（实测
    /// 双键同文）：每块按序只取首个非空键、只累加一次；空串键跳过。
    #[test]
    fn reasoning_delta_accumulates_once_per_chunk_across_keys() {
        let mut on_event = |_event: ProviderStreamEvent| {};
        let mut decoder = ChatSseDecoder {
            frames: SseFrameDecoder::default(),
            response_id: None,
            content: String::new(),
            reasoning_content: String::new(),
            tool_calls: BTreeMap::new(),
            finish_reason: None,
            usage: None,
            saw_choice: false,
            done: false,
            on_event: &mut on_event,
        };
        let mut dispatch = |payload: &str| {
            decoder
                .dispatch_event(SseFrame {
                    event_name: None,
                    data: payload.as_bytes().to_vec(),
                })
                .expect("delta frame dispatches")
        };
        dispatch(
            r#"{"choices":[{"index":0,"delta":{"reasoning_content":"think","reasoning":"think"}}]}"#,
        );
        dispatch(
            r#"{"choices":[{"index":0,"delta":{"reasoning_content":"","reasoning":"more"}}]}"#,
        );
        assert_eq!(
            decoder.reasoning_content, "thinkmore",
            "dual keys must contribute once per chunk, empty values skipped"
        );
    }

    /// [DONE] 之后网关可能追加计费尾帧（实测形如
    /// `{"choices":[],"cost":"0"}`）：DONE 即流终点，尾帧忽略，
    /// 终态物化不受影响。
    #[test]
    fn trailing_frames_after_done_are_ignored() {
        let mut on_event = |_event: ProviderStreamEvent| {};
        let mut decoder = ChatSseDecoder {
            frames: SseFrameDecoder::default(),
            response_id: None,
            content: String::new(),
            reasoning_content: String::new(),
            tool_calls: BTreeMap::new(),
            finish_reason: None,
            usage: None,
            saw_choice: false,
            done: false,
            on_event: &mut on_event,
        };
        let mut dispatch = |payload: &str| {
            decoder
                .dispatch_event(SseFrame {
                    event_name: None,
                    data: payload.as_bytes().to_vec(),
                })
                .expect("frame dispatches")
        };
        dispatch(
            r#"{"id":"c1","choices":[{"index":0,"delta":{"content":"OK"},"finish_reason":null}]}"#,
        );
        dispatch(r#"{"id":"c1","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#);
        dispatch("[DONE]");
        dispatch(r#"{"choices":[],"cost":"0"}"#);
        let terminal = decoder
            .materialize_terminal()
            .expect("terminal materializes despite trailing frame");
        assert_eq!(terminal["choices"][0]["message"]["content"], "OK");
    }
}
