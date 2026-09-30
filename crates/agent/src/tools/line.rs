//! read 与 grep 共用的按行字节读取。

use std::io::{self, BufRead};

/// 读取下一行字节，剥掉换行与 CRLF 的 CR；EOF 返回 None。
pub(super) fn read_line_bytes(reader: &mut impl BufRead) -> io::Result<Option<Vec<u8>>> {
    let mut bytes = Vec::new();
    if reader.read_until(b'\n', &mut bytes)? == 0 {
        return Ok(None);
    }
    if bytes.ends_with(b"\n") {
        bytes.pop();
        if bytes.last() == Some(&b'\r') {
            bytes.pop();
        }
    }
    Ok(Some(bytes))
}
