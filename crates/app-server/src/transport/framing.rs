//! 有界 JSON-Lines 帧切分：每行读取一个 UTF-8 JSON 值，带硬字节上限。
use super::MAX_FRAME_BYTES;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt};

/// 有界读取一条 JSON-Lines frame（剥离末尾 `\n` / `\r\n`）。
///
/// `AsyncBufReadExt::lines` 会把单条超长 frame 无界读入内存；这里用 `take` 给
/// `read_until` 加硬上限，超限 frame 返回错误并终止连接（fail-closed）。
pub(crate) async fn read_bounded_line<R: AsyncBufRead + Unpin>(
    reader: &mut R,
) -> std::io::Result<Option<String>> {
    read_bounded_line_with_limit(reader, MAX_FRAME_BYTES).await
}

pub(crate) async fn read_bounded_line_with_limit<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    max_frame_bytes: usize,
) -> std::io::Result<Option<String>> {
    let mut bytes = Vec::new();
    let limit = u64::try_from(max_frame_bytes)
        .unwrap_or(u64::MAX)
        .saturating_add(1);
    let mut limited = reader.take(limit);
    let read = limited.read_until(b'\n', &mut bytes).await?;
    if read == 0 {
        return Ok(None);
    }
    if bytes.ends_with(b"\n") {
        bytes.pop();
        if bytes.ends_with(b"\r") {
            bytes.pop();
        }
    } else if read as usize >= max_frame_bytes.saturating_add(1) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("JSON-RPC frame exceeds {max_frame_bytes} bytes"),
        ));
    }
    String::from_utf8(bytes).map(Some).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "JSON-RPC frame is not UTF-8",
        )
    })
}
