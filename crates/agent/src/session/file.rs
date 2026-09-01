use std::io::BufRead;
use std::io::BufReader;
use std::path::Path;

use serde_json::Value;
use time::OffsetDateTime;
use time::macros::format_description;
use uuid::Uuid;

use super::format::{Result, SessionError};

/// 单条 session JSONL 行（含 header）的字节硬上限（append 侧增长守卫）。
pub(super) const MAX_SESSION_LINE_BYTES: usize = 16 * 1024 * 1024;
/// 会话文件总字节上限（append 侧增长守卫）。
pub(super) const MAX_SESSION_FILE_BYTES: usize = 512 * 1024 * 1024;
/// 会话条目数上限（append 侧增长守卫）。
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
        let metadata = std::fs::metadata(path)?;
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
        .saturating_add(serialized_line_bytes as u64)
        .saturating_add(1);
    if attempted_file_bytes > limits.file_bytes {
        return Err(SessionError::AppendLimitExceeded {
            kind: "file bytes",
            limit: limits.file_bytes,
            actual: attempted_file_bytes,
        });
    }
    let attempted_entries = current_entries.saturating_add(1);
    if attempted_entries > limits.entries {
        return Err(SessionError::AppendLimitExceeded {
            kind: "entry count",
            limit: limits.entries as u64,
            actual: attempted_entries as u64,
        });
    }
    Ok(())
}

/// 解析会话文件的每一行：普通行迭代，尾部撕裂在此识别为修复状态。
pub(super) fn parse_session_lines(file: &Path) -> Result<ParsedSessionLines> {
    let handle = std::fs::File::open(file)?;
    let mut reader = BufReader::new(handle);
    let mut entries = Vec::new();
    let mut lines = Vec::new();
    let mut repair = TailRepair::None;
    let mut line_number = 1usize;
    let mut buffer: Vec<u8> = Vec::new();
    loop {
        buffer.clear();
        if reader.read_until(b'\n', &mut buffer)? == 0 {
            break;
        }
        let has_newline = buffer.ends_with(b"\n");
        let mut line = &buffer[..];
        if has_newline {
            line = &line[..line.len() - 1];
        }
        if line.ends_with(b"\r") {
            line = &line[..line.len() - 1];
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

/// 文件 mtime 投影为与 [`now_iso`] 同格式的 UTC 时间串（列表排序键）。
pub fn file_modified_iso(path: &std::path::Path) -> Option<String> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    OffsetDateTime::from(modified)
        .format(&format_description!(
            "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z"
        ))
        .ok()
}

pub(crate) fn normalize_cwd_string(cwd: &Path) -> String {
    cwd.to_string_lossy().replace('\\', "/")
}
