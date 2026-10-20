//! 与具体协议无关的 SSE 帧切分和共享的流读取循环；各协议的帧分派与终态
//! 物化在 openai 包的协议模块里，这里只提供共用的帧解码器、读取契约和错误构造核心。

use reqwest::Response;
use tokio_util::sync::CancellationToken;

use crate::error::{ModelErrorKind, ProviderError};
use crate::transport::http::{provider_cancelled_error, provider_future};

/// 与协议无关的增量 SSE 帧切分。
#[derive(Default)]
struct SseFrameDecoder {
    pending: Vec<u8>,
    event_data: Vec<u8>,
    previous_cr: bool,
    first_line_processed: bool,
}

impl SseFrameDecoder {
    /// 接收一段 chunk，交出其中所有完整的行；没有结尾换行的部分留给下次读取。
    fn push(&mut self, chunk: &[u8]) -> Vec<u8> {
        let mut last_newline = None;
        for &byte in chunk {
            // CR 已经结束上一行；紧随其后的 LF 即使在下一段 chunk 中，也不再结束一行。
            let follows_cr = self.previous_cr;
            self.previous_cr = byte == b'\r';
            if follows_cr && byte == b'\n' {
                continue;
            }
            let byte = if byte == b'\r' { b'\n' } else { byte };
            self.pending.push(byte);
            if byte == b'\n' {
                last_newline = Some(self.pending.len());
            }
        }
        let Some(end) = last_newline else {
            return Vec::new();
        };
        let tail = self.pending.split_off(end);
        std::mem::replace(&mut self.pending, tail)
    }

    fn process_line(&mut self, line: &[u8]) -> Option<Vec<u8>> {
        let line = line.strip_suffix(b"\n").unwrap_or(line);
        // 第一行完整后才检查 BOM，因此它的三个字节可以分散在不同 chunk 中。
        let line = if self.first_line_processed {
            line
        } else {
            self.first_line_processed = true;
            line.strip_prefix(b"\xef\xbb\xbf").unwrap_or(line)
        };
        if line.is_empty() {
            if self.event_data.is_empty() {
                return None;
            }
            return Some(std::mem::take(&mut self.event_data));
        }
        // 冒号开头的行是 SSE 注释（常作保活），整行忽略。
        if line.first() == Some(&b':') {
            return None;
        }
        let (field, value) = if let Some(separator) = line.iter().position(|byte| *byte == b':') {
            // 值前按 SSE 约定只去掉一个空格。
            let value = &line[separator + 1..];
            (&line[..separator], value.strip_prefix(b" ").unwrap_or(value))
        } else {
            (line, &[] as &[u8])
        };
        // Responses 的事件内容都在 data 字段的 JSON 里，其他 SSE 字段不参与执行。
        if field == b"data" {
            if !self.event_data.is_empty() {
                self.event_data.push(b'\n');
            }
            self.event_data.extend_from_slice(value);
        }
        None
    }

    fn finish(&self, malformed: fn(&'static str) -> ProviderError) -> Result<(), ProviderError> {
        if !self.pending.is_empty() || !self.event_data.is_empty() {
            return Err(malformed("event_frame_unterminated"));
        }
        Ok(())
    }
}

/// 协议特定 SSE 解码器接口（由 Chat / Responses 协议模块实现）。
pub(crate) trait SseStreamDecoder: Sized {
    type Terminal;
    /// 构造协议格式错误
    fn frame_malformed() -> fn(&'static str) -> ProviderError;

    /// 处理一个完整的 SSE 数据帧
    fn dispatch_event(&mut self, data: &[u8]) -> Result<(), ProviderError>;

    /// 消费解码器，物化终结响应
    fn materialize_terminal(self) -> Result<Self::Terminal, ProviderError>;

    /// 当前协议响应是否已达终态
    fn protocol_complete(&self) -> bool;
}

/// 通用的 SSE 流读取循环：处理网络 chunk 拆包并分发事件。
pub(crate) async fn read_sse_stream<D: SseStreamDecoder>(
    cancellation: &CancellationToken,
    mut response: Response,
    mut decoder: D,
) -> Result<D::Terminal, ProviderError> {
    let mut frames = SseFrameDecoder::default();

    loop {
        let chunk =
            provider_future(cancellation, "provider_response_body_read_failed", response.chunk()).await?;
        // 读取与取消可能同时就绪；即使 select 返回数据，也不再分发已取消请求的帧。
        if cancellation.is_cancelled() {
            return Err(provider_cancelled_error());
        }
        let Some(chunk) = chunk else {
            frames.finish(D::frame_malformed())?;
            return decoder.materialize_terminal();
        };
        let complete = frames.push(&chunk);
        for line in complete.split_inclusive(|byte| *byte == b'\n') {
            if let Some(frame) = frames.process_line(line) {
                decoder.dispatch_event(&frame)?;
                if decoder.protocol_complete() {
                    return decoder.materialize_terminal();
                }
            }
        }
    }
}

/// 构造流式响应格式异常错误
pub(crate) fn provider_stream_malformed_error(
    message: &'static str,
    code: &'static str,
    reason: &'static str,
) -> ProviderError {
    ProviderError::diagnostic(ModelErrorKind::JsonSchemaViolation, message, code, vec![reason.to_string()])
}
