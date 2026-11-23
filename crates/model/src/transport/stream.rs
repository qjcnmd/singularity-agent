use reqwest::Response;
use tokio_util::sync::CancellationToken;

use crate::error::{ModelErrorKind, ProviderError};
use crate::transport::http::{provider_cancelled_error, provider_future};

const MAX_SSE_FRAME_BYTES: usize = 16 * 1024 * 1024;

#[derive(Default)]
struct SseFrameDecoder {
    pending: Vec<u8>,
    event_data: Vec<u8>,
    previous_cr: bool,
    first_line_processed: bool,
}

impl SseFrameDecoder {
    fn push_into(&mut self, chunk: &[u8], complete: &mut Vec<u8>) -> Result<(), &'static str> {
        complete.clear();
        self.pending.reserve(chunk.len());
        let mut last_newline = None;
        let mut line_len = self.pending.len();
        for &byte in chunk {
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

    fn process_line(&mut self, line: &[u8]) -> Result<bool, &'static str> {
        let line = line.strip_suffix(b"\n").unwrap_or(line);
        let line = if self.first_line_processed {
            line
        } else {
            self.first_line_processed = true;
            line.strip_prefix(b"\xef\xbb\xbf").unwrap_or(line)
        };
        if line.is_empty() {
            return Ok(!self.event_data.is_empty());
        }
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

pub(crate) trait SseStreamDecoder: Sized {
    fn frame_malformed(reason: &'static str) -> ProviderError;

    fn dispatch_event(&mut self, data: &[u8]) -> Result<(), ProviderError>;

    fn protocol_complete(&self) -> bool;
}

pub(crate) async fn read_sse_stream<D: SseStreamDecoder>(
    cancellation: &CancellationToken,
    mut response: Response,
    decoder: &mut D,
) -> Result<(), ProviderError> {
    let mut frames = SseFrameDecoder::default();
    let mut complete = Vec::new();

    loop {
        let chunk = provider_future(cancellation, response.chunk()).await?;
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
