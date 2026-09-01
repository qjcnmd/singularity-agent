//! 有界行读取：read 与 grep 工具共用的单行读取原语。
//!
//! 单行超过 `max_bytes` 时返回 [`LineFailure::OverLimit`]（携带截断到上限内的
//! 前缀，并把该行剩余消费到换行，调用方因此可以继续读取后续行），不分配无界
//! 内存；这是两个工具面对不可信文件内容的共同保护。取消检查不在本原语内做，
//! 由调用方的逐行循环统一检查（read/grep 同一标准）。

use std::io::{self, BufRead, Read};

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

/// 单行硬上限：一行超过 4 MiB 视为不可安全读取的输入；read 与 grep 经
/// [`read_bounded_line`] 逐行读取不可信文件内容时共用这一个数值。
pub(super) const MAX_READ_LINE_BYTES: usize = 4 * 1024 * 1024;

/// 有界读取一行：单行超过 `max_bytes` 时返回 [`LineFailure::OverLimit`]，
/// 携带截断到上限内的前缀并把该行剩余消费到换行。返回行剥除末尾换行与 CR。
///
/// 前缀只保证「上限内的该行开头字节」：read 侧再按展示预算截断，grep 侧忽略
/// 该值，因此调用方不依赖其长度。
pub(super) fn read_bounded_line(
    reader: &mut impl BufRead,
    max_bytes: usize,
) -> Result<Option<Vec<u8>>, LineFailure> {
    // 读取预算即上限本身：最多取 max+1 字节，取满却没有换行就说明整行超限，
    // 内存占用因此有界，也不需要手工管理 fill_buf/consume 窗口。
    let mut bytes = Vec::new();
    let read = reader
        .by_ref()
        .take(max_bytes.saturating_add(1) as u64)
        .read_until(b'\n', &mut bytes)
        .map_err(LineFailure::Io)?;
    if read == 0 {
        return Ok(None);
    }
    let newline_terminated = bytes.ends_with(b"\n");
    if !newline_terminated && bytes.len() > max_bytes {
        bytes.truncate(max_bytes);
        // 该行剩余部分消费到换行（或 EOF），使 reader 定位到下一行开头。
        reader
            .read_until(b'\n', &mut Vec::new())
            .map_err(LineFailure::Io)?;
        return Err(LineFailure::OverLimit {
            limit: max_bytes,
            prefix: bytes,
        });
    }
    if newline_terminated {
        bytes.pop();
        if bytes.last() == Some(&b'\r') {
            bytes.pop();
        }
    }
    Ok(Some(bytes))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)] // 测试断言惯例
    use super::{LineFailure, read_bounded_line};
    use std::io::BufReader;

    /// 主路径：读满上限（raw 长度恰为 max+1）的一行仍被接受，行尾换行与 CR
    /// 被剥除；CR 计入上限，与换行同为行终止符而非内容。
    #[test]
    fn line_of_exact_limit_is_accepted() {
        let mut reader = BufReader::new(&b"abcd\r\nnext\n"[..]);
        assert_eq!(
            read_bounded_line(&mut reader, 5).expect("readable"),
            Some(b"abcd".to_vec())
        );
        assert_eq!(
            read_bounded_line(&mut reader, 5).expect("readable"),
            Some(b"next".to_vec())
        );
    }

    /// 关键失败路径：超限行返回上限内前缀，并把该行剩余消费到换行，
    /// 使后续行仍可读取（不中止整个文件）。
    #[test]
    fn over_limit_line_reports_prefix_and_resumes_next_line() {
        let mut reader = BufReader::new(&b"xxxxxxxxxxxx\nafter\n"[..]);
        let Err(LineFailure::OverLimit { limit, prefix }) = read_bounded_line(&mut reader, 5)
        else {
            panic!("oversized line must be reported as OverLimit");
        };
        assert_eq!(limit, 5);
        assert_eq!(prefix.len(), 5, "prefix must stay within the limit");
        assert_eq!(
            read_bounded_line(&mut reader, 5).expect("readable"),
            Some(b"after".to_vec()),
            "reader must be positioned at the next line"
        );
    }
}
