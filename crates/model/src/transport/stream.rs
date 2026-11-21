//! 与协议无关的 SSE 分帧和读取循环；每个事件的处理和最终回复的生成由各协议的解码器实现。

use reqwest::Response;
use tokio_util::sync::CancellationToken;

use crate::error::{ModelErrorKind, ProviderError};
use crate::transport::http::{provider_cancelled_error, provider_future};

/// 单条 SSE 帧或单行缓冲的内存上限（16 MiB），足以容纳含加密推理块的终态事件，同时防止异常 HTTP 200 正文无限增长。
const MAX_SSE_FRAME_BYTES: usize = 16 * 1024 * 1024;

/// 把网络数据块切成 SSE 事件。
#[derive(Default)]
struct SseFrameDecoder {
    pending: Vec<u8>,
    event_data: Vec<u8>,
    previous_cr: bool,
    first_line_processed: bool,
}

impl SseFrameDecoder {
    /// 接收一段 chunk，把其中所有完整的行写入 `complete`；没有结尾换行的部分留给下次读取。
    fn push_into(&mut self, chunk: &[u8], complete: &mut Vec<u8>) -> Result<(), &'static str> {
        complete.clear();
        self.pending.reserve(chunk.len());
        let mut last_newline = None;
        let mut line_len = self.pending.len();
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
                line_len = 0;
            } else {
                line_len += 1;
                if line_len > MAX_SSE_FRAME_BYTES {
                    return Err("event_frame_too_large");
                }
            }
        }
        if let Some(end) = last_newline {
            complete.extend_from_slice(&self.pending[..end]);
            self.pending.drain(..end);
        }
        Ok(())
    }

    /// 处理一行；遇到空行且已累积 `data` 时返回 `Ok(true)`，由调用方读取 `event_data()` 后清空。
    fn process_line(&mut self, line: &[u8]) -> Result<bool, &'static str> {
        let line = line.strip_suffix(b"\n").unwrap_or(line);
        // 第一行完整后才检查 BOM，因此它的三个字节可以分散在不同 chunk 中。
        let line = if self.first_line_processed {
            line
        } else {
            self.first_line_processed = true;
            line.strip_prefix(b"\xef\xbb\xbf").unwrap_or(line)
        };
        if line.is_empty() {
            return Ok(!self.event_data.is_empty());
        }
        // 冒号开头的行是 SSE 注释（常作保活），整行忽略。
        if line.first() == Some(&b':') {
            return Ok(false);
        }
        let (field, value) = if let Some(separator) = line.iter().position(|byte| *byte == b':') {
            // 值前按 SSE 约定只去掉一个空格。
            let value = &line[separator + 1..];
            (&line[..separator], value.strip_prefix(b" ").unwrap_or(value))
        } else {
            (line, &[] as &[u8])
        };
        // Messages 和 Responses 的流事件内容都在 data 行里，忽略 event、id、retry 等其他 SSE 字段。
        if field == b"data" {
            let extra = usize::from(!self.event_data.is_empty()) + value.len();
            if self.event_data.len().saturating_add(extra) > MAX_SSE_FRAME_BYTES {
                return Err("event_frame_too_large");
            }
            if !self.event_data.is_empty() {
                self.event_data.push(b'\n');
            }
            self.event_data.extend_from_slice(value);
        }
        Ok(false)
    }

    fn event_data(&self) -> &[u8] {
        &self.event_data
    }

    fn clear_event(&mut self) {
        self.event_data.clear();
    }

    fn has_trailing_event(&self) -> bool {
        self.pending.is_empty() && !self.event_data.is_empty()
    }

    fn has_unterminated(&self) -> bool {
        !self.pending.is_empty() || !self.event_data.is_empty()
    }
}

/// 各协议的 SSE 解码器（Messages / Responses 各实现一个）。
pub(crate) trait SseStreamDecoder: Sized {
    /// 构造本协议的格式错误，参数是错误原因。
    fn frame_malformed(reason: &'static str) -> ProviderError;

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
    let mut complete = Vec::new();

    loop {
        let chunk = provider_future(cancellation, response.chunk()).await?;
        // 读取与取消同时就绪时若先拿到数据，仍优先按已取消退出，避免处理已取消请求的数据。
        if cancellation.is_cancelled() {
            return Err(provider_cancelled_error());
        }
        let Some(chunk) = chunk else {
            if frames.has_trailing_event() {
                decoder.dispatch_event(frames.event_data())?;
                frames.clear_event();
                if decoder.protocol_complete() {
                    return Ok(());
                }
                return Err(D::frame_malformed("event_frame_unterminated"));
            }
            if frames.has_unterminated() {
                return Err(D::frame_malformed("event_frame_unterminated"));
            }
            return Ok(());
        };
        frames.push_into(&chunk, &mut complete).map_err(D::frame_malformed)?;
        for line in complete.split_inclusive(|byte| *byte == b'\n') {
            if frames.process_line(line).map_err(D::frame_malformed)? {
                decoder.dispatch_event(frames.event_data())?;
                frames.clear_event();
                if decoder.protocol_complete() {
                    return Ok(());
                }
            }
        }
    }
}

pub(crate) fn provider_stream_malformed_error(message: &'static str, reason: &'static str) -> ProviderError {
    ProviderError::diagnostic(ModelErrorKind::Other, message, &[reason])
}

#[cfg(test)]
pub(crate) fn decode_sse_chunks(chunks: &[&[u8]]) -> (Vec<Vec<u8>>, bool) {
    let mut frames = SseFrameDecoder::default();
    let mut complete = Vec::new();
    let mut events = Vec::new();
    for chunk in chunks {
        if frames.push_into(chunk, &mut complete).is_err() {
            return (events, true);
        }
        for line in complete.split_inclusive(|byte| *byte == b'\n') {
            match frames.process_line(line) {
                Ok(true) => {
                    events.push(frames.event_data().to_vec());
                    frames.clear_event();
                }
                Ok(false) => {}
                Err(_) => return (events, true),
            }
        }
    }
    if frames.has_trailing_event() {
        events.push(frames.event_data().to_vec());
        frames.clear_event();
    }
    (events, frames.has_unterminated())
}
