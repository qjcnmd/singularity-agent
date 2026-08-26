use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use serde_json::Value;
use time::OffsetDateTime;
use time::macros::format_description;
use uuid::Uuid;

pub use super::format::{Result, SessionError};
pub use super::manager::SessionManager;
/// 单条 session JSONL 行（含 header）的字节硬上限。
pub(super) const MAX_SESSION_LINE_BYTES: usize = 16 * 1024 * 1024;
/// 单次打开 session 文件允许解析的总字节上限（有界读取，超限 fail closed）。
pub(super) const MAX_SESSION_FILE_BYTES: usize = 512 * 1024 * 1024;
/// 单次打开 session 文件允许解析的条目数上限。
pub(super) const MAX_SESSION_ENTRIES: usize = 200_000;

#[derive(Debug, Clone, Copy)]
pub(crate) struct AppendLimits {
    pub(crate) line_bytes: usize,
    pub(crate) file_bytes: u64,
    pub(crate) entries: usize,
}

pub(super) const DEFAULT_APPEND_LIMITS: AppendLimits = AppendLimits {
    line_bytes: MAX_SESSION_LINE_BYTES,
    file_bytes: MAX_SESSION_FILE_BYTES as u64,
    entries: MAX_SESSION_ENTRIES,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SessionFileState {
    /// 最后一个已校验完整行之后的字节偏移。
    pub(super) len: u64,
}

impl SessionFileState {
    pub(super) fn capture(path: &Path) -> Result<Self> {
        let metadata = std::fs::symlink_metadata(path)?;
        Ok(Self {
            len: metadata.len(),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TailRepair {
    None,
    RemoveTornTail,
    AddFinalNewline,
}

pub(crate) struct ParsedSessionLines {
    pub(super) entries: Vec<Value>,
    pub(super) lines: Vec<usize>,
    pub(super) repair: TailRepair,
}

pub(super) fn validate_append_limits(
    current_file_bytes: u64,
    current_entries: usize,
    serialized_line_bytes: usize,
    limits: AppendLimits,
) -> Result<()> {
    if serialized_line_bytes > limits.line_bytes {
        return Err(SessionError::AppendLimitExceeded {
            kind: "line bytes",
            limit: limits.line_bytes as u64,
            actual: serialized_line_bytes as u64,
        });
    }
    let attempted_file_bytes = current_file_bytes
        .checked_add(serialized_line_bytes as u64)
        .and_then(|bytes| bytes.checked_add(1))
        .ok_or(SessionError::AppendLimitExceeded {
            kind: "file bytes",
            limit: limits.file_bytes,
            actual: u64::MAX,
        })?;
    if attempted_file_bytes > limits.file_bytes {
        return Err(SessionError::AppendLimitExceeded {
            kind: "file bytes",
            limit: limits.file_bytes,
            actual: attempted_file_bytes,
        });
    }
    let attempted_entries =
        current_entries
            .checked_add(1)
            .ok_or(SessionError::AppendLimitExceeded {
                kind: "entry count",
                limit: limits.entries as u64,
                actual: u64::MAX,
            })?;
    if attempted_entries > limits.entries {
        return Err(SessionError::AppendLimitExceeded {
            kind: "entry count",
            limit: limits.entries as u64,
            actual: attempted_entries as u64,
        });
    }
    Ok(())
}

pub(super) fn parse_session_lines(file: &Path) -> Result<ParsedSessionLines> {
    parse_session_lines_with_limits(
        file,
        MAX_SESSION_FILE_BYTES,
        MAX_SESSION_LINE_BYTES,
        MAX_SESSION_ENTRIES,
    )
}

pub(crate) fn parse_session_lines_with_limits(
    file: &Path,
    max_file_bytes: usize,
    max_line_bytes: usize,
    max_content_entries: usize,
) -> Result<ParsedSessionLines> {
    let metadata = std::fs::metadata(file)?;
    if metadata.len() > max_file_bytes as u64 {
        return Err(SessionError::InvalidSession(format!(
            "session file exceeds bounded parse limits ({} bytes / {max_content_entries} entries)",
            max_file_bytes
        )));
    }

    let handle = std::fs::File::open(file)?;
    let mut reader = BufReader::new(handle);
    let mut entries = Vec::new();
    let mut lines = Vec::new();
    let mut repair = TailRepair::None;
    let mut line_number = 1usize;
    while let Some(bounded_line) = read_bounded_session_line(&mut reader, max_line_bytes)? {
        if bounded_line.too_long {
            return Err(SessionError::InvalidSession(format!(
                "session entry exceeds {max_line_bytes} bytes at line {line_number}"
            )));
        }
        let has_newline = bounded_line.has_newline;
        let mut line = bounded_line.bytes.as_slice();
        if line.ends_with(b"\r") {
            line = &line[..line.len() - 1];
        }
        if line.len() > max_line_bytes {
            return Err(SessionError::InvalidSession(format!(
                "session entry exceeds {max_line_bytes} bytes at line {line_number}"
            )));
        }
        if line.iter().all(u8::is_ascii_whitespace) {
            if !has_newline {
                repair = TailRepair::AddFinalNewline;
                break;
            }
            line_number += 1;
            continue;
        }

        let text = match std::str::from_utf8(line) {
            Ok(text) => text,
            Err(error) if !has_newline && error.error_len().is_none() => {
                repair = TailRepair::RemoveTornTail;
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
                repair = TailRepair::RemoveTornTail;
                break;
            }
            Err(error) => {
                return Err(SessionError::MalformedLine {
                    line: line_number,
                    cause: error.to_string(),
                });
            }
        };
        if !value.is_object() {
            return Err(SessionError::InvalidEntry {
                line: line_number,
                cause: "session entry is not a JSON object".to_string(),
            });
        }
        let content_entries = entries.len().saturating_sub(1);
        if content_entries >= max_content_entries {
            return Err(SessionError::InvalidSession(format!(
                "session file exceeds bounded parse limits ({} bytes / {max_content_entries} entries)",
                max_file_bytes
            )));
        }
        entries.push(value);
        lines.push(line_number);
        if !has_newline {
            repair = TailRepair::AddFinalNewline;
            break;
        }
        line_number += 1;
    }
    Ok(ParsedSessionLines {
        entries,
        lines,
        repair,
    })
}

struct BoundedSessionLine {
    bytes: Vec<u8>,
    has_newline: bool,
    too_long: bool,
}

fn read_bounded_session_line<R: BufRead>(
    reader: &mut R,
    limit: usize,
) -> std::io::Result<Option<BoundedSessionLine>> {
    let mut bytes = Vec::with_capacity(limit.min(4096) + 1);
    loop {
        let buffer = reader.fill_buf()?;
        if buffer.is_empty() {
            if bytes.is_empty() {
                return Ok(None);
            }
            return Ok(Some(BoundedSessionLine {
                bytes,
                has_newline: false,
                too_long: false,
            }));
        }
        let newline = buffer.iter().position(|byte| *byte == b'\n');
        let content_len = newline.unwrap_or(buffer.len());
        if bytes.len().saturating_add(content_len) > limit.saturating_add(1) {
            return Ok(Some(BoundedSessionLine {
                bytes: Vec::new(),
                has_newline: newline.is_some(),
                too_long: true,
            }));
        }
        bytes.extend_from_slice(&buffer[..content_len]);
        let consumed = newline.map_or(content_len, |position| position + 1);
        reader.consume(consumed);
        if newline.is_some() {
            return Ok(Some(BoundedSessionLine {
                bytes,
                has_newline: true,
                too_long: false,
            }));
        }
    }
}

pub(super) fn rewrite_file(file: &Path, entries: &[Value]) -> Result<()> {
    let serialized: Vec<String> = entries
        .iter()
        .map(serde_json::to_string)
        .collect::<std::result::Result<_, _>>()?;
    let parent = file.parent().unwrap_or_else(|| Path::new("."));
    let name = file
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("session.jsonl");
    let temporary = parent.join(format!(".{name}.tmp-{}", Uuid::new_v4().simple()));
    let mut handle = singularity_core::create_owner_only_file(&temporary).map_err(|error| {
        SessionError::Repair {
            context: "could not create temporary session file".to_string(),
            source: error,
        }
    })?;
    let write_result = (|| -> std::io::Result<()> {
        for line in &serialized {
            handle.write_all(line.as_bytes())?;
            handle.write_all(b"\n")?;
        }
        handle.flush()?;
        handle.sync_all()?;
        Ok(())
    })();
    drop(handle);
    if let Err(error) = write_result {
        let _ = std::fs::remove_file(&temporary);
        return Err(SessionError::Repair {
            context: "could not write temporary session file".to_string(),
            source: error,
        });
    }
    if let Err(error) = singularity_core::atomic_replace(&temporary, file) {
        let _ = std::fs::remove_file(&temporary);
        return Err(SessionError::Repair {
            context: "could not atomically replace session file".to_string(),
            source: error,
        });
    }
    Ok(())
}

pub(super) fn generate_id(occupied: impl Fn(&str) -> bool) -> String {
    for _ in 0..100 {
        let id: String = Uuid::new_v4()
            .simple()
            .to_string()
            .chars()
            .take(8)
            .collect();
        if !occupied(&id) {
            return id;
        }
    }
    Uuid::new_v4().to_string()
}

pub fn now_iso() -> String {
    OffsetDateTime::now_utc()
        .format(&format_description!(
            "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z"
        ))
        .expect("utc timestamp always formats")
}

pub(super) fn normalize_abs_path(path: &Path) -> Result<PathBuf> {
    Ok(std::path::absolute(path)?)
}

pub(crate) fn normalize_cwd_string(cwd: &Path) -> String {
    cwd.to_string_lossy().replace('\\', "/")
}
