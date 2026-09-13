//! SSE 流式解码器（Chat Completions 与 Responses 格式）。

use std::collections::BTreeMap;

use reqwest::Response;
use serde_json::Value;
use singularity_core::CancellationToken;

use crate::MAX_PROVIDER_RESPONSE_BODY_BYTES;
use crate::error::{ModelErrorKind, ProviderError};
use crate::provider::contract::ProviderApiProtocol;
use crate::provider::telemetry::ProviderStreamEvent;
use crate::transport::http::{
    block_on_provider_future, provider_cancelled_error, provider_embedded_error,
    provider_error_fields,
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

/// 流式解码器的统一读取契约：read_sse_stream 以该 trait 泛型驱动 chunk
/// 循环。chunk→帧的泵（push）与终态前的帧边界校验（finish）由默认实现
/// 收敛；协议差异只保留在 malformed 构造器、单帧分派与终态物化里。
/// 只作泛型约束使用（无 trait 对象），Sized 供默认方法调用关联构造器。
trait SseStreamDecoder: Sized {
    type Terminal;
    /// 该协议的 malformed 构造器（帧边界失败的稳定词形）。
    fn frame_malformed() -> fn(&'static str) -> ProviderError
    where
        Self: Sized;

    /// 单帧协议分派。
    fn dispatch_event(&mut self, frame: SseFrame) -> Result<(), ProviderError>;

    /// 终态物化：帧边界已校验后由默认 finish 调用。
    fn materialize_terminal(&mut self) -> Result<Self::Terminal, ProviderError>;

    /// 是否已发射可见文本增量（失败路径的边界快照）。
    fn emitted_text_delta(&self) -> bool;

    /// 解码器持有的帧边界解码器（默认 push/finish 的共享输入）。
    fn sse_frames(&mut self) -> &mut SseFrameDecoder;

    fn push(&mut self, chunk: &[u8]) -> Result<(), ProviderError> {
        for frame in self.sse_frames().push(chunk, Self::frame_malformed())? {
            self.dispatch_event(frame)?;
        }
        Ok(())
    }

    fn finish(&mut self) -> Result<Self::Terminal, ProviderError> {
        self.sse_frames().finish(Self::frame_malformed())?;
        self.materialize_terminal()
    }
}

/// 通用流读取循环：保留任意 HTTP chunk 与 SSE 帧边界，失败路径携带
/// 解码器边界快照（是否已发射文本增量）。
///
/// 每次只通过已有 helper 等待一个 chunk；helper 返回后在普通同步上下文
/// 调用 decoder，因此解码回调（及其触发的同步事件出口）不会进入 block_on
/// 的运行时上下文。取消、超时和 transport 错误继续由同一 helper 映射。
fn read_sse_stream<D: SseStreamDecoder>(
    runtime: &tokio::runtime::Handle,
    cancellation: &CancellationToken,
    mut response: Response,
    mut decoder: D,
) -> Result<D::Terminal, ProviderError> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_PROVIDER_RESPONSE_BODY_BYTES as u64)
    {
        return Err(provider_response_stream_too_large_error());
    }

    if cancellation.is_cancelled() {
        return Err(provider_cancelled_error());
    }

    let stream_result = (|| {
        loop {
            let chunk = block_on_provider_future(
                runtime,
                cancellation,
                "provider_response_body_read_failed",
                || response.chunk(),
            )?;
            if cancellation.is_cancelled() {
                return Err(provider_cancelled_error());
            }
            let Some(chunk) = chunk else {
                return decoder.finish();
            };
            decoder.push(&chunk)?;
        }
    })();

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
    provider: &super::OpenAiProvider,
    request: &crate::ModelTurnRequest,
    cancellation: &CancellationToken,
    response: Response,
    on_event: &mut dyn FnMut(ProviderStreamEvent),
) -> Result<crate::openai::OpenAiCompletion, ProviderError> {
    let selection = &provider.selected_model;
    let config = &provider.config;
    let runtime = &provider.runtime;
    let (response, reasoning_content_present) = match selection.api_protocol {
        ProviderApiProtocol::Chat => {
            let parts = read_sse_stream(
                runtime,
                cancellation,
                response,
                ChatSseDecoder::new(on_event),
            )?;
            let present =
                !parts.reasoning_content.is_empty() || !parts.reasoning_details.is_empty();
            (
                crate::openai::finish_chat_response(
                    request,
                    config,
                    &selection.model_name,
                    selection.reasoning_variant.as_deref(),
                    parts,
                ),
                present,
            )
        }
        ProviderApiProtocol::Responses => {
            let payload = read_sse_stream(
                runtime,
                cancellation,
                response,
                ResponsesSseDecoder::new(on_event),
            )?;
            let present = crate::openai::openai_responses_reasoning_content_present(&payload);
            (
                crate::openai::parse_openai_responses_response(
                    request,
                    config,
                    payload,
                    &selection.model_name,
                    selection.reasoning_variant.as_deref(),
                ),
                present,
            )
        }
    };
    response
        .map(|response| crate::openai::OpenAiCompletion {
            response,
            reasoning_content_present,
        })
        .map_err(ProviderError::without_automatic_retry)
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
    type Terminal = crate::openai::ChatResponseParts;
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
            ));
        }
        if let Some(usage) = payload.get("usage").filter(|value| value.is_object()) {
            self.usage = Some(usage.clone());
        }
        let Some(choices) = payload.get("choices").and_then(Value::as_array) else {
            if payload.get("choices").is_some() {
                return Err(provider_chat_stream_malformed_error("choices_invalid"));
            }
            // 仅有 usage 的块在 OpenAI include_usage 扩展中是合法的。
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
            let index = choice.get("index").and_then(Value::as_u64).unwrap_or(0);
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
            // 兼容端点可能在同一块里用多个键携带相同 reasoning（实测
            // 双键同文）；按序取首个非空键，只累加一次。
            if let Some((field, reasoning)) = crate::openai::chat_reasoning_text(delta) {
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
                for detail in details {
                    if !detail.is_object() {
                        return Err(provider_chat_stream_malformed_error(
                            "reasoning_detail_not_object",
                        ));
                    }
                    if self.reasoning_field.is_none()
                        && let Some(text) = crate::openai::chat_reasoning_detail_text(detail)
                    {
                        (self.on_event)(ProviderStreamEvent::ReasoningTextDelta {
                            delta: text.to_string(),
                        });
                    }
                    append_reasoning_detail(&mut self.reasoning_details, detail);
                }
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
                    if !call.is_object()
                        || call
                            .get("type")
                            .is_some_and(|kind| kind.as_str() != Some("function"))
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
                        && ["name", "arguments"]
                            .iter()
                            .any(|key| function.get(*key).is_some_and(|value| !value.is_string()))
                    {
                        return Err(provider_chat_stream_malformed_error(
                            "tool_function_field_invalid",
                        ));
                    }
                    let index = call.get("index").and_then(Value::as_u64).unwrap_or(0);
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
            .clone()
            .ok_or_else(|| provider_chat_stream_malformed_error("finish_reason_missing"))?;
        let tool_calls = self
            .tool_calls
            .values()
            .map(|call| {
                let (arguments, validation_errors) =
                    crate::openai::parse_tool_arguments(&call.arguments);
                crate::ModelToolCall {
                    tool_call_id: call.id.clone(),
                    tool_name: call.name.clone(),
                    arguments,
                    raw_arguments: call.arguments.clone(),
                    validation_errors,
                }
            })
            .collect();
        Ok(crate::openai::ChatResponseParts {
            content: self.content.clone(),
            tool_calls,
            reasoning_content: self.reasoning_content.clone(),
            reasoning_field: self
                .reasoning_field
                .clone()
                .unwrap_or_else(|| "reasoning_content".into()),
            reasoning_details: self.reasoning_details.clone(),
            finish_reason: Some(finish_reason),
            usage: crate::openai::parse_usage(
                self.usage.as_ref(),
                "prompt_tokens",
                "completion_tokens",
                "/prompt_tokens_details/cached_tokens",
                "/completion_tokens_details/reasoning_tokens",
            ),
        })
    }

    fn emitted_text_delta(&self) -> bool {
        !self.content.is_empty()
            || !self.reasoning_content.is_empty()
            || self
                .reasoning_details
                .iter()
                .any(|detail| crate::openai::chat_reasoning_detail_text(detail).is_some())
    }

    fn sse_frames(&mut self) -> &mut SseFrameDecoder {
        &mut self.frames
    }
}

impl<'a> ChatSseDecoder<'a> {
    pub(super) fn new(on_event: &'a mut dyn FnMut(ProviderStreamEvent)) -> Self {
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
}

/// 合并同一个文本/摘要片段的增量；加密条目保持原始边界和字段。
fn append_reasoning_detail(details: &mut Vec<Value>, incoming: &Value) {
    let text_key = crate::openai::chat_reasoning_detail_text_field(incoming);
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
            && let (Some(before), Some(delta)) = (
                previous.get(key).and_then(Value::as_str),
                incoming.get(key).and_then(Value::as_str),
            )
        {
            previous[key] = Value::String(format!("{before}{delta}"));
            for (field, value) in incoming.as_object().into_iter().flatten() {
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
    details.push(incoming.clone());
}

/// 增量、总量有界的 Responses 事件契约 SSE 解码器。
pub(crate) struct ResponsesSseDecoder<'a> {
    frames: SseFrameDecoder,
    terminal_response: Option<Value>,
    pub emitted_text_delta: bool,
    on_event: &'a mut dyn FnMut(ProviderStreamEvent),
}

impl SseStreamDecoder for ResponsesSseDecoder<'_> {
    type Terminal = Value;
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
                    self.emitted_text_delta = true;
                    let delta = delta.to_string();
                    (self.on_event)(if reasoning {
                        ProviderStreamEvent::ReasoningTextDelta { delta }
                    } else {
                        ProviderStreamEvent::OutputTextDelta { delta }
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
            "response.incomplete" => {
                // response 对象仍是权威的部分事实；parse_openai_responses_response
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

    fn materialize_terminal(&mut self) -> Result<Self::Terminal, ProviderError> {
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
    ProviderError::diagnostic(
        ModelErrorKind::JsonSchemaViolation,
        message,
        code,
        vec![reason.to_string()],
    )
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
    ProviderError::new(
        ModelErrorKind::JsonSchemaViolation,
        "provider Responses stream did not contain a completed terminal",
    )
    .with_code("responses_stream_terminal_missing")
}

pub(super) fn provider_response_stream_too_large_error() -> ProviderError {
    ProviderError::new(
        ModelErrorKind::JsonSchemaViolation,
        "provider stream exceeded the fixed safety limit",
    )
    .with_code("provider_response_stream_too_large")
}

#[cfg(test)]
mod frame_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)] // 测试断言惯例
    use super::*;

    #[test]
    fn chat_rejects_invalid_wire_fields_before_normalization() {
        for delta in [
            serde_json::json!({"role":"user"}),
            serde_json::json!({"content":42}),
            serde_json::json!({"tool_calls":{}}),
            serde_json::json!({"tool_calls":[{"index":0,"type":"other"}]}),
            serde_json::json!({"tool_calls":[{"index":0,"function":{"arguments":{}}}]}),
        ] {
            let mut on_event = |_| {};
            let mut decoder = ChatSseDecoder::new(&mut on_event);
            let frame = serde_json::json!({"choices":[{"index":0,"delta":delta}]});
            let error = decoder
                .push(format!("data: {frame}\n\n").as_bytes())
                .unwrap_err();
            assert_eq!(error.code.as_deref(), Some("chat_stream_malformed"));
        }
    }

    #[test]
    fn chat_tool_fragments_preserve_raw_arguments_and_length_terminal() {
        let mut on_event = |_| {};
        let mut decoder = ChatSseDecoder::new(&mut on_event);
        for delta in [
            serde_json::json!({"role":"assistant","tool_calls":[{"index":0,"id":"call","type":"function","function":{"name":"read","arguments":"{"}}]}),
            serde_json::json!({"tool_calls":[{"index":0,"function":{"arguments":"\"path\":"}}]}),
        ] {
            let frame = serde_json::json!({"choices":[{"index":0,"delta":delta}]});
            decoder
                .push(format!("data: {frame}\n\n").as_bytes())
                .unwrap();
        }
        decoder.push(b"data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"length\"}]}\n\ndata: {\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":2}}\n\ndata: [DONE]\n\n").unwrap();
        let parts = decoder.finish().unwrap();
        assert_eq!(parts.finish_reason.as_deref(), Some("length"));
        assert_eq!(parts.tool_calls[0].raw_arguments, "{\"path\":");
        assert_eq!(parts.tool_calls[0].validation_errors, ["invalid_json"]);
        assert!(parts.usage.usage_present);
        assert_eq!(parts.usage.input_tokens, 10);
        let config = crate::provider::runtime::OpenAiProviderConfig {
            provider_name: "fixture".into(),
            base_url: "http://localhost/v1".into(),
            api_key: "unused".into(),
        };
        let mut request = crate::ModelTurnRequest::new("request", vec![]);
        request.tools.push(crate::ModelToolSchema {
            name: "read".into(),
            description: "read".into(),
            parameters_schema: serde_json::json!({"type":"object"}),
        });
        let response =
            crate::openai::finish_chat_response(&request, &config, "model", None, parts).unwrap();
        assert_eq!(response.stop_reason, Some(crate::ModelStopReason::Length));
        assert_eq!(response.tool_calls()[0].raw_arguments, "{\"path\":");
    }

    #[test]
    fn responses_error_preserves_top_level_and_nested_provider_fields() {
        for fields in [
            serde_json::json!({"type":"error", "code":"context_length_exceeded", "message":"input too long"}),
            serde_json::json!({"type":"error", "error":{"code":"context_length_exceeded", "message":"input too long"}}),
        ] {
            let mut on_event = |_| {};
            let mut decoder = ResponsesSseDecoder::new(&mut on_event);
            let error = decoder
                .push(format!("data: {fields}\n\n").as_bytes())
                .unwrap_err();
            assert!(error.is_context_overflow());
            assert!(error.message.starts_with("input too long"));
        }
    }

    #[test]
    fn responses_reasoning_summary_is_visible_before_completion() {
        let mut observed = Vec::new();
        let mut on_event = |event| observed.push(event);
        let mut decoder = ResponsesSseDecoder::new(&mut on_event);
        let event = serde_json::json!({"type":"response.reasoning_summary_text.delta", "delta":"checking the file"});
        decoder
            .push(format!("data: {event}\n\n").as_bytes())
            .unwrap();
        assert!(decoder.emitted_text_delta());
        assert!(decoder.finish().is_err(), "there is no terminal yet");
        drop(decoder);
        assert_eq!(
            observed,
            vec![ProviderStreamEvent::ReasoningTextDelta {
                delta: "checking the file".into()
            }]
        );
    }

    #[test]
    fn structured_reasoning_merges_text_preserves_opaque_items_and_marks_visible_output() {
        let mut observed = Vec::new();
        let mut on_event = |event| observed.push(event);
        let mut decoder = ChatSseDecoder::new(&mut on_event);
        let details = [
            serde_json::json!({"type":"reasoning.text", "index":0, "text":"first ", "format":"provider-v1"}),
            serde_json::json!({"type":"reasoning.text", "index":0, "id":"r1", "text":"second"}),
            serde_json::json!({"type":"reasoning.encrypted", "data":"opaque-1", "id":"r2"}),
            serde_json::json!({"type":"reasoning.encrypted", "data":"opaque-2", "id":"r2"}),
        ];
        for detail in &details {
            let frame = format!(
                "data: {}\n\n",
                serde_json::json!({"choices":[{"index":0,"delta":{"reasoning_details":[detail]}}]})
            );
            decoder.push(frame.as_bytes()).unwrap();
        }
        assert!(
            decoder.emitted_text_delta(),
            "a failed stream cannot transparently replay visible thinking"
        );
        decoder.push(b"data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n").unwrap();
        let response = decoder.finish().unwrap();
        assert_eq!(
            serde_json::json!(response.reasoning_details),
            serde_json::json!([
                {"type":"reasoning.text", "index":0, "id":"r1", "text":"first second", "format":"provider-v1"},
                details[2], details[3]
            ])
        );
        drop(decoder);
        assert_eq!(
            observed,
            vec![
                ProviderStreamEvent::ReasoningTextDelta {
                    delta: "first ".into()
                },
                ProviderStreamEvent::ReasoningTextDelta {
                    delta: "second".into()
                },
            ]
        );
    }

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
        let mut observed = Vec::new();
        let mut on_event = |event: ProviderStreamEvent| observed.push(event);
        let mut decoder = ChatSseDecoder::new(&mut on_event);
        decoder.push(br#"data: {"choices":[{"index":0,"delta":{"reasoning_content":"think","reasoning":"think"}}]}

data: {"choices":[{"index":0,"delta":{"reasoning_content":"","reasoning":"more"}}]}

"#).unwrap();
        assert_eq!(
            decoder.reasoning_content, "thinkmore",
            "dual keys must contribute once per chunk, empty values skipped"
        );
        drop(decoder);
        assert_eq!(
            observed,
            vec![
                ProviderStreamEvent::ReasoningTextDelta {
                    delta: "think".to_string()
                },
                ProviderStreamEvent::ReasoningTextDelta {
                    delta: "more".to_string()
                },
            ],
            "public thinking is emitted before a terminal frame exists"
        );
    }

    /// [DONE] 之后网关可能追加计费尾帧（实测形如
    /// {"choices":[],"cost":"0"}）：DONE 即流终点，尾帧忽略，
    /// 终态物化不受影响。
    #[test]
    fn trailing_frames_after_done_are_ignored() {
        let mut on_event = |_event: ProviderStreamEvent| {};
        let mut decoder = ChatSseDecoder::new(&mut on_event);
        decoder.push(br#"data: {"id":"c1","choices":[{"index":0,"delta":{"content":"OK"},"finish_reason":null}]}

data: {"id":"c1","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}

data: [DONE]

data: {"choices":[],"cost":"0"}

"#).unwrap();
        let terminal = decoder
            .finish()
            .expect("trailing frame must not invalidate the reply");
        assert_eq!(terminal.content, "OK");
    }
}
