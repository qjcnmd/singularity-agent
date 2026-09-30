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
}

impl SseFrameDecoder {
    /// 接收一段 chunk，交出其中所有完整的行；没有结尾换行的部分留给下次读取。
    fn push(&mut self, chunk: &[u8]) -> Vec<u8> {
        let pending_len = self.pending.len();
        self.pending.extend_from_slice(chunk);
        // pending 只保留上次的未终止行；换行只能出现在新收到的 chunk 中。
        let Some(last_newline) = chunk.iter().rposition(|byte| *byte == b'\n') else {
            return Vec::new();
        };
        let tail = self.pending.split_off(pending_len + last_newline + 1);
        std::mem::replace(&mut self.pending, tail)
    }

    fn process_line(&mut self, line: &[u8]) -> Option<Vec<u8>> {
        let line = line.strip_suffix(b"\n").unwrap_or(line);
        let line = line.strip_suffix(b"\r").unwrap_or(line);
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
            (
                &line[..separator],
                value.strip_prefix(b" ").unwrap_or(value),
            )
        } else {
            (line, &[] as &[u8])
        };
        // Responses 使用 JSON type，其他 SSE 字段不参与执行。
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

/// 协议解码器只接收完整 SSE 帧并解释终态；字节缓冲与帧边界由读取循环持有。
pub(crate) trait SseStreamDecoder: Sized {
    type Terminal;
    /// 该协议的 malformed 构造器（帧边界失败时用的稳定词形）。
    fn frame_malformed() -> fn(&'static str) -> ProviderError;

    fn dispatch_event(&mut self, data: &[u8]) -> Result<(), ProviderError>;

    /// 消费解码器，一次性移交结果；EOF 时也由此报告缺失的协议终态。
    fn materialize_terminal(self) -> Result<Self::Terminal, ProviderError>;

    /// 本协议的终态是否已经到达。读取循环据此立刻物化结果并停止读取这个响应：
    /// 已经完成的响应，成败不再取决于 HTTP body 是否结束；EOF 只用来判断意外截断。
    fn protocol_complete(&self) -> bool;
}

/// 通用的流读取循环：HTTP chunk 的任意切分和 SSE 帧边界都能正确保留。
///
/// 每轮异步等待一个 chunk，随后同步解码已收到的字节；取消与 transport 错误由共同的
/// 等待函数映射。
pub(crate) async fn read_sse_stream<D: SseStreamDecoder>(
    cancellation: &CancellationToken,
    mut response: Response,
    mut decoder: D,
) -> Result<D::Terminal, ProviderError> {
    let mut frames = SseFrameDecoder::default();

    loop {
        let chunk = provider_future(
            cancellation,
            "provider_response_body_read_failed",
            response.chunk(),
        )
        .await?;
        if cancellation.is_cancelled() {
            return Err(provider_cancelled_error());
        }
        let Some(chunk) = chunk else {
            // 没到终态而 body 就结束了：只有走这条路径才算意外截断。
            frames.finish(D::frame_malformed())?;
            return decoder.materialize_terminal();
        };
        let complete = frames.push(&chunk);
        for line in complete.split_inclusive(|byte| *byte == b'\n') {
            if let Some(frame) = frames.process_line(line) {
                decoder.dispatch_event(&frame)?;
                // 协议终态后不再解析尾帧，也不等待 HTTP body 关闭。
                if decoder.protocol_complete() {
                    return decoder.materialize_terminal();
                }
            }
        }
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
