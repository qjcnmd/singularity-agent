use std::io::BufRead;
use std::io::BufReader;
use std::path::{Path, PathBuf};

use super::format::{Result, SessionEntry, SessionError, SessionHeader, SessionMetadata};
use super::manager::SessionData;

/// 逐行解析会话日志；最后一行没有换行时不解析，返回它的起始位置供写者截掉。
pub(super) fn parse_session_file(file: &Path) -> Result<(SessionData, Option<u64>)> {
    let mut entries = Vec::new();
    let (header, cwd, repair) = scan_session_file(file, |entry| entries.push(entry))?;
    Ok((
        SessionData {
            file: file.to_path_buf(),
            cwd,
            entries,
            session_id: header.id,
            header_timestamp: header.timestamp,
        },
        repair,
    ))
}

pub(super) fn scan_session_file(
    file: &Path,
    mut visit: impl FnMut(SessionEntry),
) -> Result<(SessionHeader, PathBuf, Option<u64>)> {
    let handle = std::fs::File::open(file)?;
    let mut reader = BufReader::new(handle);
    let mut header = None;
    let mut first_is_settings = None;
    let mut repair = None;
    let mut offset = 0u64;
    let mut line_number = 1usize;
    let mut buffer: Vec<u8> = Vec::new();
    loop {
        buffer.clear();
        if reader.read_until(b'\n', &mut buffer)? == 0 {
            break;
        }
        if !buffer.ends_with(b"\n") {
            repair = Some(offset);
            break;
        }
        let mut line = &buffer[..buffer.len() - 1];
        if line.ends_with(b"\r") {
            line = &line[..line.len() - 1];
        }
        if line.iter().all(u8::is_ascii_whitespace) {
            offset += buffer.len() as u64;
            line_number += 1;
            continue;
        }

        let text = match std::str::from_utf8(line) {
            Ok(text) => text,
            Err(error) => {
                return Err(SessionError::InvalidEntry {
                    line: line_number,
                    source: Box::new(error),
                });
            }
        };
        let parse_error = |error: serde_json::Error| SessionError::InvalidEntry {
            line: line_number,
            source: Box::new(error),
        };
        if header.is_none() {
            let parsed: SessionHeader = serde_json::from_str(text).map_err(parse_error)?;
            header = Some(parsed.validate()?);
        } else {
            let entry = serde_json::from_str(text).map_err(parse_error)?;
            first_is_settings.get_or_insert(matches!(
                entry,
                SessionEntry::Metadata {
                    metadata: SessionMetadata::ThreadSettings { .. },
                    ..
                }
            ));
            visit(entry);
        }
        offset += buffer.len() as u64;
        line_number += 1;
    }
    let header = header.ok_or_else(|| {
        SessionError::InvalidSession(format!("Session file is not a valid session: {}", file.display()))
    })?;
    let cwd = header.canonical_cwd()?;
    if first_is_settings != Some(true) {
        return Err(SessionError::InvalidSession(format!("会话日志缺少初始模型选择：{}", file.display())));
    }
    Ok((header, cwd, repair))
}

pub(super) fn repair_tail(file: &Path, offset: u64) -> Result<()> {
    let handle = std::fs::OpenOptions::new().write(true).open(file)?;
    handle.set_len(offset)?;
    Ok(())
}
