//! 有界行读取：read 与 grep 工具共用的单行读取原语。
//!
//! 单行超过 `max_bytes` 时 fail closed（返回 [`LineFailure::OverLimit`]），
//! 不分配无界内存；这是两个工具面对不可信文件内容的共同保护。

use std::io::{self, BufRead};

use singularity_core::CancellationToken;

/// 有界行读取失败。
#[derive(Debug)]
pub(crate) enum LineFailure {
    /// 行长度超过上限。
    OverLimit(usize),
    /// 取消信号触发。
    Cancelled,
    /// 底层读取错误。
    Io(io::Error),
}

pub(crate) struct BoundedLine {
    pub(crate) bytes: Vec<u8>,
    pub(crate) has_newline: bool,
}

impl std::fmt::Display for LineFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OverLimit(limit) => write!(formatter, "line exceeds {limit} bytes"),
            Self::Cancelled => formatter.write_str(super::registry::ABORTED_MESSAGE),
            Self::Io(error) => error.fmt(formatter),
        }
    }
}

/// 有界读取一行：单行超过 `max_bytes` 时 fail closed，不分配无界内存。
/// 返回行剥除末尾换行与 CR；`signal` 取消时返回 [`LineFailure::Cancelled`]。
pub(super) fn read_bounded_line(
    reader: &mut impl BufRead,
    max_bytes: usize,
    signal: Option<&CancellationToken>,
) -> Result<Option<Vec<u8>>, LineFailure> {
    let Some(mut line) = read_bounded_line_with_termination(reader, max_bytes, signal)? else {
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
    signal: Option<&CancellationToken>,
) -> Result<Option<BoundedLine>, LineFailure> {
    let mut bytes = Vec::new();
    let newline_terminated = loop {
        if signal.is_some_and(CancellationToken::is_cancelled) {
            return Err(LineFailure::Cancelled);
        }
        let (take_len, newline_terminated) = {
            let buffer = reader.fill_buf().map_err(LineFailure::Io)?;
            if buffer.is_empty() {
                break false;
            }
            let newline = buffer.iter().position(|byte| *byte == b'\n');
            let take_len = newline.map_or(buffer.len(), |index| index.saturating_add(1));
            if bytes.len().saturating_add(take_len) > max_bytes.saturating_add(1) {
                return Err(LineFailure::OverLimit(max_bytes));
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
        return Err(LineFailure::OverLimit(max_bytes));
    }
    if newline_terminated {
        bytes.pop();
    }
    Ok(Some(BoundedLine {
        bytes,
        has_newline: newline_terminated,
    }))
}
