//! 有界行读取：read 与 grep 共用的单行读取原语。
//!
//! 单行超过 max_bytes 时返回 LineFailure::OverLimit，并带上截断到上限内的前缀，同时
//! 把这一行剩余字节消费到换行——调用方因此还能继续读后面的行，内存占用始终有界。取消
//! 检查不在这里做，由调用方的逐行循环统一检查（read 和 grep 同一标准）。

use std::io::{self, BufRead, Read};

/// 有界行读取失败。
#[derive(Debug)]
pub(crate) enum LineFailure {
    /// 这一行超过了长度上限；prefix 是截断到上限内、超限前已读入的前缀字节。
    OverLimit { prefix: Vec<u8> },
    /// 底层读取错误。
    Io(io::Error),
}

/// 单行的硬上限：一行超过 4 MiB 就视为无法安全读取的输入；read 与 grep 共用这一个数值。
pub(super) const MAX_READ_LINE_BYTES: usize = 4 * 1024 * 1024;

/// 有界读取一行：单行超过 max_bytes 就返回 LineFailure::OverLimit，并带上截断到上限内
/// 的前缀，同时把这一行剩余的部分消费到换行。返回的行已剥掉末尾的换行与 CR。
///
/// 前缀只保证是「上限内这一行的开头字节」：read 侧会再按展示预算截断，grep 侧直接忽略
/// 它，所以调用方不能依赖它的长度。
pub(super) fn read_bounded_line(
    reader: &mut impl BufRead,
    max_bytes: usize,
) -> Result<Option<Vec<u8>>, LineFailure> {
    // 读取预算就取上限本身加一：最多读 max+1 字节；读满却没有换行，就说明整行
    // 超限。这样内存占用有界，也不必手工管理 fill_buf/consume 窗口。
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
        // 把这一行剩下的部分消费到换行（或 EOF），让 reader 停在下一行的开头。
        reader.skip_until(b'\n').map_err(LineFailure::Io)?;
        return Err(LineFailure::OverLimit { prefix: bytes });
    }
    if newline_terminated {
        bytes.pop();
        if bytes.last() == Some(&b'\r') {
            bytes.pop();
        }
    }
    Ok(Some(bytes))
}
