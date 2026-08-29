//! 有界行读取：read 与 grep 工具共用的单行读取原语。
//!
//! 单行超过 `max_bytes` 时返回 [`LineFailure::OverLimit`]（携带已读前缀，
//! 并把行剩余消费到换行，调用方因此可以继续读取后续行），不分配无界内存；
//! 这是两个工具面对不可信文件内容的共同保护。取消检查不在本原语内做，
//! 由调用方的逐行循环统一检查（read/grep 同一标准）。

use std::io::{self, BufRead};

/// 有界行读取失败。
#[derive(Debug)]
pub(crate) enum LineFailure {
    /// 行长度超过上限；`prefix` 为超限前已读入的、截断到上限内的前缀字节。
    OverLimit { limit: usize, prefix: Vec<u8> },
    /// 底层读取错误。
    Io(io::Error),
}

impl std::fmt::Display for LineFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OverLimit { limit, .. } => write!(formatter, "line exceeds {limit} bytes"),
            Self::Io(error) => error.fmt(formatter),
        }
    }
}

pub(crate) struct BoundedLine {
    pub(crate) bytes: Vec<u8>,
    pub(crate) has_newline: bool,
}

/// 有界读取一行：单行超过 `max_bytes` 时返回 [`LineFailure::OverLimit`]，
/// 携带截断到上限内的前缀并把该行剩余消费到换行。返回行剥除末尾换行与 CR。
pub(super) fn read_bounded_line(
    reader: &mut impl BufRead,
    max_bytes: usize,
) -> Result<Option<Vec<u8>>, LineFailure> {
    let Some(mut line) = read_bounded_line_with_termination(reader, max_bytes)? else {
        return Ok(None);
    };
    if line.has_newline && line.bytes.last() == Some(&b'\r') {
        line.bytes.pop();
    }
    Ok(Some(line.bytes))
}

pub(crate) fn read_bounded_line_with_termination(
    reader: &mut impl BufRead,
    max_bytes: usize,
) -> Result<Option<BoundedLine>, LineFailure> {
    let mut bytes = Vec::new();
    let newline_terminated = loop {
        let (take_len, newline_terminated) = {
            let buffer = reader.fill_buf().map_err(LineFailure::Io)?;
            if buffer.is_empty() {
                break false;
            }
            let newline = buffer.iter().position(|byte| *byte == b'\n');
            let take_len = newline.map_or(buffer.len(), |index| index.saturating_add(1));
            if bytes.len().saturating_add(take_len) > max_bytes.saturating_add(1) {
                let prefix_end = bytes.len().min(max_bytes);
                let prefix = truncated_prefix(&bytes, prefix_end);
                consume_rest_of_line(reader).map_err(LineFailure::Io)?;
                return Err(LineFailure::OverLimit {
                    limit: max_bytes,
                    prefix,
                });
            }
            bytes.extend_from_slice(&buffer[..take_len]);
            (take_len, newline.is_some())
        };
        reader.consume(take_len);
        if newline_terminated {
            break true;
        }
    };
    if bytes.is_empty() {
        return Ok(None);
    }
    if !newline_terminated && bytes.len() > max_bytes {
        return Err(LineFailure::OverLimit {
            limit: max_bytes,
            prefix: truncated_prefix(&bytes, bytes.len().min(max_bytes)),
        });
    }
    if newline_terminated {
        bytes.pop();
    }
    Ok(Some(BoundedLine {
        bytes,
        has_newline: newline_terminated,
    }))
}

/// 把当前行剩余部分（从 reader 当前缓冲位置开始）消费到换行或 EOF，
/// 使 reader 定位到下一行开头。
fn consume_rest_of_line(reader: &mut impl BufRead) -> io::Result<()> {
    loop {
        let buffer = reader.fill_buf()?;
        if buffer.is_empty() {
            return Ok(());
        }
        let newline = buffer.iter().position(|byte| *byte == b'\n');
        let take = newline.map_or(buffer.len(), |index| index.saturating_add(1));
        reader.consume(take);
        if newline.is_some() {
            return Ok(());
        }
    }
}

/// 截断到上限内的字节前缀（调用方经 `from_utf8_lossy` 展示，不要求 char 边界）。
fn truncated_prefix(bytes: &[u8], max_bytes: usize) -> Vec<u8> {
    bytes[..max_bytes.min(bytes.len())].to_vec()
}
