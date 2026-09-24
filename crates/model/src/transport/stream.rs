//! 与具体协议无关的 SSE 帧切分、有界读取和共享的流读取循环；各协议的帧分派与终态
//! 物化在 openai 包的协议模块里，这里只提供共用的帧解码器、读取契约和错误构造核心。

use reqwest::Response;
use tokio_util::sync::CancellationToken;

use crate::MAX_PROVIDER_RESPONSE_BODY_BYTES;
use crate::error::{ModelErrorKind, ProviderError};
use crate::transport::http::{block_on_provider_future, provider_cancelled_error};

pub(crate) struct SseFrame {
    pub(crate) event_name: Option<String>,
    pub(crate) data: Vec<u8>,
}

/// 与协议无关的增量 SSE 帧切分，整体只有一个总字节上限。
#[derive(Default)]
pub(crate) struct SseFrameDecoder {
    pending: Vec<u8>,
    event_data: Vec<u8>,
    event_name: Option<String>,
    total_bytes: usize,
}

impl SseFrameDecoder {
    /// 接收一段 chunk，交出其中所有完整的行；没有结尾换行的部分留给下次读取。
    fn push(&mut self, chunk: &[u8]) -> Result<Vec<u8>, ProviderError> {
        self.total_bytes = self
            .total_bytes
            .checked_add(chunk.len())
            .ok_or_else(provider_response_stream_too_large_error)?;
        if self.total_bytes > MAX_PROVIDER_RESPONSE_BODY_BYTES {
            return Err(provider_response_stream_too_large_error());
        }
        self.pending.extend_from_slice(chunk);
        let Some(last_newline) = self.pending.iter().rposition(|byte| *byte == b'\n') else {
            return Ok(Vec::new());
        };
        let tail = self.pending.split_off(last_newline + 1);
        Ok(std::mem::replace(&mut self.pending, tail))
    }

    fn process_line(
        &mut self,
        line: &[u8],
        malformed: fn(&'static str) -> ProviderError,
    ) -> Result<Option<SseFrame>, ProviderError> {
        let line = line.strip_suffix(b"\n").unwrap_or(line);
        let line = line.strip_suffix(b"\r").unwrap_or(line);
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
        // 冒号开头的行是 SSE 注释（常作保活），整行忽略。
        if line.first() == Some(&b':') {
            return Ok(None);
        }
        let (field, value) = if let Some(separator) = line.iter().position(|byte| *byte == b':') {
            // 值前按 SSE 约定只去掉一个空格。
            let value = &line[separator + 1..];
            (
                &line[..separator],
                value.strip_prefix(b" ").unwrap_or(value),
            )
        } else {
            (line, &[] as &[u8])
        };
        match field {
            b"data" => {
                // 总字节上限已由 push 一处管住：event_data 只累积去前缀后的 data 值，必小于该上限。
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
            // id 与 retry 只服务断线续传，本实现和其他未知字段一起忽略。
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

/// 流式解码器统一的读取契约：read_sse_stream 用它作泛型参数驱动 chunk 循环，push/finish
/// 的默认实现收拢共同逻辑，各协议的差异只留在 malformed 构造器、单帧分派和终态物化里。
pub(crate) trait SseStreamDecoder: Sized {
    type Terminal;
    /// 该协议的 malformed 构造器（帧边界失败时用的稳定词形）。
    fn frame_malformed() -> fn(&'static str) -> ProviderError;

    fn dispatch_event(&mut self, frame: SseFrame) -> Result<(), ProviderError>;

    /// 物化终态：帧边界校验通过后由默认的 finish 调用。
    fn materialize_terminal(&mut self) -> Result<Self::Terminal, ProviderError>;

    /// 本协议的终态是否已经到达。读取循环据此立刻物化结果并停止读取这个响应：
    /// 已经完成的响应，成败不再取决于 HTTP body 是否结束；EOF 只用来判断意外截断。
    fn protocol_complete(&self) -> bool;

    fn sse_frames(&mut self) -> &mut SseFrameDecoder;

    fn push(&mut self, chunk: &[u8]) -> Result<(), ProviderError> {
        if self.protocol_complete() {
            return Ok(());
        }
        let complete = self.sse_frames().push(chunk)?;
        for line in complete.split_inclusive(|byte| *byte == b'\n') {
            if let Some(frame) = self
                .sse_frames()
                .process_line(line, Self::frame_malformed())?
            {
                self.dispatch_event(frame)?;
                if self.protocol_complete() {
                    break;
                }
            }
        }
        Ok(())
    }

    fn finish(&mut self) -> Result<Self::Terminal, ProviderError> {
        if !self.protocol_complete() {
            self.sse_frames().finish(Self::frame_malformed())?;
        }
        self.materialize_terminal()
    }
}

/// 通用的流读取循环：HTTP chunk 的任意切分和 SSE 帧边界都能正确保留。
///
/// 每轮只通过 helper 等一个 chunk，返回后才在同步上下文里调用 decoder，所以解码回调
/// 不会进入 block_on 的运行时上下文；取消、超时和 transport 错误仍由同一个 helper 映射。
pub(crate) fn read_sse_stream<D: SseStreamDecoder>(
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

    loop {
        // 协议终态已到达：立刻物化结果，不再等 HTTP body 结束。
        if decoder.protocol_complete() {
            return decoder.materialize_terminal();
        }
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
            // 没到终态而 body 就结束了：只有走这条路径才算意外截断。
            return decoder.finish();
        };
        decoder.push(&chunk)?;
    }
}

/// malformed 构造器的统一核心：各协议的字面词留在薄包装里，构造体只写这一份。
pub(crate) fn provider_stream_malformed_error(
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

pub(crate) fn provider_response_stream_too_large_error() -> ProviderError {
    ProviderError::new(
        ModelErrorKind::JsonSchemaViolation,
        "provider stream exceeded the fixed safety limit",
    )
    .with_code("provider_response_stream_too_large")
}
