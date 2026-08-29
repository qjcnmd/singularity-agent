use std::io::BufReader;
use std::path::Path;

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
            "session file exceeds bounded parse limits ({max_file_bytes} bytes / {max_content_entries} entries)"
        )));
    }

    let handle = std::fs::File::open(file)?;
    let mut reader = BufReader::new(handle);
    let mut entries = Vec::new();
    let mut lines = Vec::new();
    let mut repair = TailRepair::None;
    let mut line_number = 1usize;
    while let Some(bounded_line) =
        crate::tools::line::read_bounded_line_with_termination(&mut reader, max_line_bytes)
            .map_err(|error| match error {
                crate::tools::line::LineFailure::OverLimit { .. } => {
                    SessionError::InvalidSession(format!(
                        "session entry exceeds {max_line_bytes} bytes at line {line_number}"
                    ))
                }
                crate::tools::line::LineFailure::Io(error) => SessionError::Io(error),
            })?
    {
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
                "session file exceeds bounded parse limits ({max_file_bytes} bytes / {max_content_entries} entries)"
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

pub(super) fn rewrite_file(file: &Path, entries: &[Value]) -> Result<()> {
    // 序列化后委托共享原子替换原语：与工具层（edit/write）同一安全管道。
    let mut bytes = Vec::new();
    for entry in entries {
        serde_json::to_writer(&mut bytes, entry)?;
        bytes.push(b'\n');
    }
    singularity_core::atomic_replace_bytes(file, &bytes).map_err(|error| SessionError::Repair {
        context: "could not atomically replace session file".to_string(),
        source: error,
    })
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
    // 不变量：固定格式串格式化 UTC 时间戳恒不失败。
    #[allow(clippy::expect_used)]
    OffsetDateTime::now_utc()
        .format(&format_description!(
            "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z"
        ))
        .expect("utc timestamp always formats")
}

pub(crate) fn normalize_cwd_string(cwd: &Path) -> String {
    cwd.to_string_lossy().replace('\\', "/")
}
