use std::io::BufRead;
use std::io::BufReader;
use std::path::Path;

use serde_json::Value;

use super::format::{Result, SessionEntry, SessionError, SessionHeader};

/// 写打开修复尾行，只读打开只读取完整行。
#[derive(Clone, Copy)]
pub(super) enum TailPolicy {
    RepairAndRewrite,
    CompleteLines,
}

pub(super) struct ParsedSession {
    /// 磁盘上文件头的原始内容；修复写回时原样写回它的字段值。
    pub(super) header: SessionHeader,
    /// header 里 cwd 归一化后的唯一结果，供运行期使用。
    pub(super) cwd: std::path::PathBuf,
    pub(super) entries: Vec<SessionEntry>,
    pub(super) needs_repair: bool,
}

/// 逐行解析会话文件：普通行顺序迭代，尾部的撕裂行在这里被识别成待修复状态。
pub(super) fn parse_session_file(file: &Path, tail_policy: TailPolicy) -> Result<ParsedSession> {
    let handle = std::fs::File::open(file)?;
    let mut reader = BufReader::new(handle);
    let mut entries = Vec::new();
    let mut header = None;
    let mut needs_repair = false;
    let mut line_number = 1usize;
    let mut buffer: Vec<u8> = Vec::new();
    loop {
        buffer.clear();
        if reader.read_until(b'\n', &mut buffer)? == 0 {
            break;
        }
        let has_newline = buffer.ends_with(b"\n");
        if !has_newline && matches!(tail_policy, TailPolicy::CompleteLines) {
            break;
        }
        let mut line = &buffer[..];
        if has_newline {
            line = &line[..line.len() - 1];
        }
        if line.ends_with(b"\r") {
            line = &line[..line.len() - 1];
        }
        if line.iter().all(u8::is_ascii_whitespace) {
            if !has_newline {
                needs_repair = true;
                break;
            }
            line_number += 1;
            continue;
        }

        let text = match std::str::from_utf8(line) {
            Ok(text) => text,
            // 只把文件末尾被截断的 UTF-8 视为可修复的撕裂。
            Err(error) if !has_newline && error.error_len().is_none() => {
                needs_repair = true;
                break;
            }
            Err(error) => {
                return Err(SessionError::MalformedLine {
                    line: line_number,
                    cause: format!("invalid UTF-8: {error}"),
                });
            }
        };
        let value = match serde_json::from_str::<Value>(text) {
            Ok(value) => value,
            Err(error) if !has_newline && error.is_eof() => {
                needs_repair = true;
                break;
            }
            Err(error) => {
                return Err(SessionError::MalformedLine {
                    line: line_number,
                    cause: error.to_string(),
                });
            }
        };
        if header.is_none() {
            header = Some(SessionHeader::parse(value)?);
        } else {
            let entry = serde_json::from_value(value).map_err(|error| SessionError::InvalidEntry {
                line: line_number,
                cause: error.to_string(),
            })?;
            entries.push(entry);
        }
        // 末行没有换行符，后续追加会与它粘成一行，需要修复。
        if !has_newline {
            needs_repair = true;
            break;
        }
        line_number += 1;
    }
    let header = header.ok_or_else(|| {
        SessionError::InvalidSession(format!("Session file is not a valid session: {}", file.display()))
    })?;
    let cwd = header.canonical_cwd()?;
    Ok(ParsedSession { header, cwd, entries, needs_repair })
}

pub(super) fn rewrite_file(file: &Path, header: &SessionHeader, entries: &[SessionEntry]) -> Result<()> {
    // 序列化完成后交给共享的原子替换原语：与工具层（edit/write）走同一条安全管道。
    let mut bytes = Vec::new();
    serde_json::to_writer(&mut bytes, header)?;
    bytes.push(b'\n');
    for entry in entries {
        serde_json::to_writer(&mut bytes, entry)?;
        bytes.push(b'\n');
    }
    singularity_core::atomic_replace_bytes(file, &bytes).map_err(|error| {
        SessionError::Io(std::io::Error::new(
            error.kind(),
            format!("could not atomically replace session file {}: {error}", file.display()),
        ))
    })
}
