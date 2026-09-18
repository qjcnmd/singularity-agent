//! 协议无关的 SSE 帧切分、有界读取与共享流读取循环。
//!
//! 具体协议的帧分派与终态物化位于 openai 包各自的协议模块（chat/responses）；
//! 这里只提供它们共用的帧解码器、读取契约与错误构造核心。

use reqwest::Response;
use singularity_core::CancellationToken;

use crate::MAX_PROVIDER_RESPONSE_BODY_BYTES;
use crate::error::{ModelErrorKind, ProviderError};
use crate::transport::http::{block_on_provider_future, provider_cancelled_error};

pub(crate) struct SseFrame {
    pub(crate) event_name: Option<String>,
    pub(crate) data: Vec<u8>,
}

/// 协议无关的增量 SSE 帧切分，带单一总字节上限。
#[derive(Default)]
pub(crate) struct SseFrameDecoder {
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
                // 累计原始流字节已由 push 的单一上限约束：event_data 只累积
                // data 字段值，去掉前缀后必然小于该上限，不再重复检查。
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
pub(crate) trait SseStreamDecoder: Sized {
    type Terminal;
    /// 该协议的 malformed 构造器（帧边界失败的稳定词形）。
    fn frame_malformed() -> fn(&'static str) -> ProviderError;

    /// 单帧协议分派。
    fn dispatch_event(&mut self, frame: SseFrame) -> Result<(), ProviderError>;

    /// 终态物化：帧边界已校验后由默认 finish 调用。
    fn materialize_terminal(&mut self) -> Result<Self::Terminal, ProviderError>;

    /// 协议终态是否已经到达。driver 据此当即物化结果并停止读取该响应：
    /// 已完成响应的成功不再取决于 HTTP body 是否结束，EOF 只用于判断意外截断。
    fn protocol_complete(&self) -> bool;

    /// 是否已发射可见文本增量（失败路径的边界快照）。
    fn emitted_text_delta(&self) -> bool;

    /// 解码器持有的帧边界解码器（默认 push/finish 的共享输入）。
    fn sse_frames(&mut self) -> &mut SseFrameDecoder;

    fn push(&mut self, chunk: &[u8]) -> Result<(), ProviderError> {
        // 协议终态之后的帧不再参与任何判断：已完成的事实不因尾部无关帧被改判。
        if self.protocol_complete() {
            return Ok(());
        }
        for frame in self.sse_frames().push(chunk, Self::frame_malformed())? {
            self.dispatch_event(frame)?;
            if self.protocol_complete() {
                break;
            }
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

    let stream_result = (|| {
        loop {
            // 协议终态已到达：当即物化结果，不再等待 HTTP body 结束；终态之后
            // 的读取超时、断开或无关尾帧都不能把一个已完成的响应改判失败。
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
                // 没有终态而 body 已结束：只有这条路径才判定为意外截断。
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

/// malformed 构造器的统一核心：协议字面词保留在薄包装里，构造体只写一次。
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

#[cfg(test)]
mod frame_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)] // 测试断言惯例
    use super::*;

    /// 帧切分本身与协议无关：一个 chunk 里的多帧按源顺序产出。
    #[test]
    fn one_chunk_with_many_frames_is_split_in_source_order() {
        let mut chunk = Vec::new();
        for index in 0..4096 {
            chunk.extend_from_slice(format!("data: {index}\n\n").as_bytes());
        }
        let frames = SseFrameDecoder::default()
            .push(&chunk, |reason| {
                provider_stream_malformed_error("malformed", "test_malformed", reason)
            })
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
