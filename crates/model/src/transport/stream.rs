//! 与协议无关的 SSE 分帧和读取循环；每个事件的处理和最终回复的生成由 openai 模块里
//! 各协议的解码器实现。

use reqwest::Response;
use tokio_util::sync::CancellationToken;

use crate::error::{ModelErrorKind, ProviderError};
use crate::transport::http::{provider_cancelled_error, provider_future};

/// 把网络数据块切成 SSE 事件。
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
            // CR 已经结束上一行；紧随其后的 LF 即使跨 chunk 到达也直接跳过，避免多切出空行。
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
        // Chat 和 Responses 的流事件内容都在 data 行里，忽略 event、id、retry 等其他 SSE 字段。
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

/// 各协议的 SSE 解码器（Chat / Responses 各实现一个）。
pub(crate) trait SseStreamDecoder: Sized {
    /// 本协议的格式错误构造函数，参数是错误原因。
    fn frame_malformed() -> fn(&'static str) -> ProviderError;

    /// 处理一个完整的 SSE 事件。
    fn dispatch_event(&mut self, data: &[u8]) -> Result<(), ProviderError>;

    /// 是否已经收到表示结束的事件。
    fn protocol_complete(&self) -> bool;
}

/// SSE 读取循环：把网络数据块切成事件，交给解码器处理，直到结束。
pub(crate) async fn read_sse_stream<D: SseStreamDecoder>(
    cancellation: &CancellationToken,
    mut response: Response,
    decoder: &mut D,
) -> Result<(), ProviderError> {
    let mut frames = SseFrameDecoder::default();

    loop {
        let chunk = provider_future(cancellation, response.chunk()).await?;
        // 读取与取消同时就绪时若先拿到数据，仍优先按已取消退出，避免处理已取消请求的数据。
        if cancellation.is_cancelled() {
            return Err(provider_cancelled_error());
        }
        let Some(chunk) = chunk else {
            frames.finish(D::frame_malformed())?;
            return Ok(());
        };
        let complete = frames.push(&chunk);
        for line in complete.split_inclusive(|byte| *byte == b'\n') {
            if let Some(frame) = frames.process_line(line) {
                decoder.dispatch_event(&frame)?;
                if decoder.protocol_complete() {
                    return Ok(());
                }
            }
        }
    }
}

pub(crate) fn provider_stream_malformed_error(message: &'static str, reason: &'static str) -> ProviderError {
    ProviderError::diagnostic(ModelErrorKind::JsonSchemaViolation, message, vec![reason.to_string()])
}
