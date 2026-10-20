use std::io::BufRead;
use std::io::{BufReader, Seek, SeekFrom, Write};
use std::path::Path;

use serde_json::Value;

use super::format::{Result, SessionEntry, SessionError, SessionHeader};

/// 尾行未闭合换行时的处理策略（写入前修复截断行，或只读模式仅读取完整行）。
#[derive(Clone, Copy)]
pub(super) enum TailPolicy {
    RepairTail,
    CompleteLines,
}

pub(super) struct ParsedSession {
    pub(super) header: SessionHeader,
    /// 归一化后的工作目录路径
    pub(super) cwd: std::path::PathBuf,
    pub(super) entries: Vec<SessionEntry>,
    pub(super) repair: Option<(u64, bool)>,
}

/// 逐行解析会话 JSONL 文件，并检测未闭合的末尾行。
pub(super) fn parse_session_file(file: &Path, tail_policy: TailPolicy) -> Result<ParsedSession> {
    let handle = std::fs::File::open(file)?;
    let mut reader = BufReader::new(handle);
    let mut entries = Vec::new();
    let mut header = None;
    let mut repair = None;
    let mut offset = 0u64;
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
                repair = Some((offset, false));
                break;
            }
            offset += buffer.len() as u64;
            line_number += 1;
            continue;
        }

        let text = match std::str::from_utf8(line) {
            Ok(text) => text,
            // 只把文件末尾被截断的 UTF-8 视为可修复的撕裂。
            Err(error) if !has_newline && error.error_len().is_none() => {
                repair = Some((offset, false));
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
                repair = Some((offset, false));
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
        offset += buffer.len() as u64;
        // 完整末行只补换行；残缺末行截到上一条记录。
        if !has_newline {
            repair = Some((offset, true));
            break;
        }
        line_number += 1;
    }
    let header = header.ok_or_else(|| {
        SessionError::InvalidSession(format!("Session file is not a valid session: {}", file.display()))
    })?;
    let cwd = header.canonical_cwd()?;
    Ok(ParsedSession { header, cwd, entries, repair })
}

pub(super) fn repair_tail(file: &Path, offset: u64, add_newline: bool) -> Result<()> {
    let mut handle = std::fs::OpenOptions::new().write(true).open(file)?;
    handle.set_len(offset)?;
    if add_newline {
        handle.seek(SeekFrom::End(0))?;
        handle.write_all(b"\n")?;
    }
    handle.flush()?;
    Ok(())
}
