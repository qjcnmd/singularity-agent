//! read 工具：有界流式读取指定文件内容，支持基于 `offset` 与 `limit` 的行范围读取。

use std::fs::File;
use std::io::{BufRead, BufReader, Read};

use serde_json::{Value, json};

use super::registry::{ExecuteContext, ToolError, ToolExecution, error_result, resolve_path};
use super::truncate::{DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, format_size};

/// 单行硬上限：一行超过 4 MiB 视为不可安全读取的输入。
const MAX_READ_LINE_BYTES: usize = 4 * 1024 * 1024;
/// 单次 read 的扫描字节上限：避免在超大文件上无界流式扫描。
const MAX_READ_SCAN_BYTES: usize = 64 * 1024 * 1024;

pub(crate) const DESCRIPTION: &str = "Read the contents of a text file. Output is truncated to 2000 lines or 50KB (whichever is hit first). Use offset/limit for large files. When you need the full file, continue with offset until complete.";

pub(crate) fn parameters() -> Value {
    json!({
        "type": "object",
        "properties": {
            "path": { "type": "string", "description": "Path to the file to read (relative or absolute)" },
            "offset": { "type": "integer", "description": "Line number to start reading from (1-indexed)" },
            "limit": { "type": "integer", "description": "Maximum number of lines to read" },
        },
        "required": ["path"],
        "additionalProperties": false,
    })
}

pub(crate) fn spec() -> super::registry::ToolSpec {
    super::registry::ToolSpec {
        name: "read",
        description: DESCRIPTION,
        parameters: parameters(),
        execute,
    }
}

pub(crate) fn execute(ctx: ExecuteContext<'_>) -> Result<ToolExecution, ToolError> {
    let Some(path) = ctx.args.get("path").and_then(Value::as_str) else {
        return error_result("missing required parameter \"path\"");
    };
    let offset = ctx.args.get("offset").and_then(Value::as_u64);
    let limit = ctx.args.get("limit").and_then(Value::as_u64);
    if ctx.signal.is_some_and(|signal| signal.is_cancelled()) {
        return error_result("Operation aborted");
    }
    let full_path = resolve_path(ctx.cwd, path);
    let file = match File::open(&full_path) {
        Ok(file) => file,
        Err(error) => {
            return error_result(format!("Could not read file: {path}. {error}"));
        }
    };
    let mut reader = BufReader::with_capacity(64 * 1024, file);
    let start_line = offset.map_or(0, |offset| (offset as usize).saturating_sub(1));
    let start_line_display = start_line + 1;
    let mut state = ReadState {
        total_lines: 0,
        selected: Vec::new(),
        selected_bytes: 0,
        selected_truncated: false,
        first_line_exceeds_limit: false,
        first_line_len: None,
        scan_limit_hit: false,
        eof: true,
    };
    let mut line_number = 0usize;
    let mut scanned_bytes = 0usize;
    loop {
        if ctx.signal.is_some_and(|signal| signal.is_cancelled()) {
            return error_result("Operation aborted");
        }
        let Some(line) = (match read_bounded_line(&mut reader) {
            Ok(line) => line,
            Err(error) => {
                return error_result(format!("Could not read file: {path}. {error}"));
            }
        }) else {
            break;
        };
        scanned_bytes = scanned_bytes.saturating_add(line.len().saturating_add(1));
        line_number += 1;
        if scanned_bytes > MAX_READ_SCAN_BYTES {
            state.scan_limit_hit = true;
            state.eof = false;
            break;
        }
        let selected_position = line_number.saturating_sub(start_line);
        if selected_position == 0 {
            continue;
        }
        if selected_position == 1 && line.len() > DEFAULT_MAX_BYTES {
            state.first_line_exceeds_limit = true;
            state.first_line_len = Some(line.len());
        }
        if state.first_line_exceeds_limit || state.selected_truncated {
            continue;
        }
        let next_bytes = state
            .selected_bytes
            .saturating_add(line.len())
            .saturating_add(usize::from(!state.selected.is_empty()));
        let user_line_limit = limit.map_or(DEFAULT_MAX_LINES, |limit| {
            usize::try_from(limit)
                .unwrap_or(DEFAULT_MAX_LINES)
                .min(DEFAULT_MAX_LINES)
        });
        if next_bytes > DEFAULT_MAX_BYTES {
            state.selected_truncated = true;
            continue;
        }
        if state.selected.len() >= user_line_limit {
            if limit.is_none() {
                state.selected_truncated = true;
            }
            continue;
        }
        state
            .selected
            .push(String::from_utf8_lossy(&line).into_owned());
        state.selected_bytes = next_bytes;
    }
    state.total_lines = line_number;

    if line_number == 0 {
        if offset.is_some_and(|offset| offset > 1) {
            return error_result(format!(
                "Offset {} is beyond end of file (0 lines total)",
                offset.unwrap_or(0)
            ));
        }
        return Ok(ToolExecution {
            content: String::new(),
            is_error: false,
        });
    }

    if start_line >= line_number {
        return error_result(format!(
            "Offset {} is beyond end of file ({line_number} lines total)",
            offset.unwrap_or(0)
        ));
    }
    let output_text = render_read_output(path, start_line_display, limit, &state);
    Ok(ToolExecution {
        content: output_text,
        is_error: false,
    })
}

struct ReadState {
    total_lines: usize,
    selected: Vec<String>,
    selected_bytes: usize,
    selected_truncated: bool,
    first_line_exceeds_limit: bool,
    first_line_len: Option<usize>,
    scan_limit_hit: bool,
    eof: bool,
}

fn render_read_output(
    path: &str,
    start_line_display: usize,
    limit: Option<u64>,
    state: &ReadState,
) -> String {
    let selected_content = state.selected.join("\n");
    if state.first_line_exceeds_limit {
        let line_len = state
            .first_line_len
            .unwrap_or(DEFAULT_MAX_BYTES.saturating_add(1));
        return format!(
            "[Line {start_line_display} is {}, exceeds {} limit. Use bash: sed -n '{start_line_display}p' {path} | head -c {}]",
            format_size(line_len),
            format_size(DEFAULT_MAX_BYTES),
            DEFAULT_MAX_BYTES
        );
    }
    let end_line_display =
        start_line_display.saturating_add(state.selected.len().saturating_sub(1));
    if state.selected_truncated {
        let next_offset = end_line_display.saturating_add(1);
        let remainder = if state.scan_limit_hit || !state.eof {
            "file exceeds read scan limit".to_string()
        } else {
            format!(
                "{} more lines in file",
                state.total_lines.saturating_sub(end_line_display)
            )
        };
        return format!(
            "{selected_content}\n\n[Showing lines {start_line_display}-{end_line_display} ({} limit). {remainder}. Use offset={next_offset} to continue.]",
            format_size(DEFAULT_MAX_BYTES)
        );
    }
    if let Some(limit) = limit {
        let limit = usize::try_from(limit).unwrap_or(usize::MAX);
        let end = start_line_display.saturating_add(state.selected.len().saturating_sub(1));
        if end < state.total_lines || state.scan_limit_hit || !state.eof {
            let remainder = if state.scan_limit_hit || !state.eof {
                "file exceeds read scan limit".to_string()
            } else {
                format!(
                    "{} more lines in file",
                    state.total_lines.saturating_sub(end)
                )
            };
            let next_offset = end.saturating_add(1);
            return format!(
                "{selected_content}\n\n[{remainder}. Use offset={next_offset} to continue.]"
            );
        }
        let _ = limit;
    }
    selected_content
}

/// 有界读取一行：单行超过 `MAX_READ_LINE_BYTES` 时 fail closed，不分配无界内存。
fn read_bounded_line(reader: &mut impl BufRead) -> std::io::Result<Option<Vec<u8>>> {
    let mut bytes = Vec::new();
    let max = u64::try_from(MAX_READ_LINE_BYTES).unwrap_or(u64::MAX);
    let read = {
        let mut limited = reader.take(max.saturating_add(1));
        limited.read_until(b'\n', &mut bytes)?
    };
    if read == 0 {
        return Ok(None);
    }
    let newline_terminated = bytes.last() == Some(&b'\n');
    if !newline_terminated && read > max as usize {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("line exceeds {MAX_READ_LINE_BYTES} bytes"),
        ));
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
    use super::*;
    use crate::tools::registry::ToolRegistry;
    use crate::tools::test_support::context;
    use serde_json::json;
    use std::fs;
    use tempfile::tempdir;

    fn write_lines(dir: &std::path::Path, name: &str, lines: usize) {
        let content = (1..=lines)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(dir.join(name), content).expect("write fixture");
    }

    #[test]
    fn reads_file_content() {
        let dir = tempdir().expect("temp dir");
        fs::write(dir.path().join("sample.txt"), "line1\nline2\nline3").expect("fixture");
        let result = ToolRegistry::new()
            .execute("read", context(json!({ "path": "sample.txt" }), dir.path()))
            .expect("execute");
        assert!(!result.is_error, "content: {}", result.content);
        assert!(result.content.contains("line1\nline2\nline3"));
    }

    #[test]
    fn offset_and_limit_select_lines() {
        let dir = tempdir().expect("temp dir");
        write_lines(dir.path(), "sample.txt", 10);
        let result = ToolRegistry::new()
            .execute(
                "read",
                context(
                    json!({ "path": "sample.txt", "offset": 3, "limit": 2 }),
                    dir.path(),
                ),
            )
            .expect("execute");
        assert!(!result.is_error, "content: {}", result.content);
        assert_eq!(
            result.content,
            "line3\nline4\n\n[6 more lines in file. Use offset=5 to continue.]"
        );
    }

    #[test]
    fn offset_beyond_end_is_error() {
        let dir = tempdir().expect("temp dir");
        write_lines(dir.path(), "sample.txt", 3);
        let result = ToolRegistry::new()
            .execute(
                "read",
                context(json!({ "path": "sample.txt", "offset": 10 }), dir.path()),
            )
            .expect("execute");
        assert!(result.is_error);
        assert!(
            result
                .content
                .contains("Offset 10 is beyond end of file (3 lines total)")
        );
    }

    #[test]
    fn missing_file_is_error() {
        let dir = tempdir().expect("temp dir");
        let result = ToolRegistry::new()
            .execute(
                "read",
                context(json!({ "path": "missing.txt" }), dir.path()),
            )
            .expect("execute");
        assert!(result.is_error);
        assert!(result.content.contains("Could not read file"));
    }

    #[test]
    fn oversized_line_fails_closed_without_allocating_the_file() {
        let dir = tempdir().expect("temp dir");
        let path = dir.path().join("large.txt");
        std::fs::write(&path, "x".repeat(MAX_READ_LINE_BYTES + 1)).expect("fixture");
        let result = ToolRegistry::new()
            .execute("read", context(json!({ "path": "large.txt" }), dir.path()))
            .expect("execute");
        assert!(result.is_error, "content: {}", result.content);
        assert!(result.content.contains("line exceeds"));
    }

    #[test]
    fn reads_empty_file_successfully() {
        let dir = tempdir().expect("temp dir");
        let path = dir.path().join("empty.txt");
        std::fs::write(&path, "").expect("fixture");
        let result = ToolRegistry::new()
            .execute("read", context(json!({ "path": "empty.txt" }), dir.path()))
            .expect("execute");
        assert!(!result.is_error, "content: {}", result.content);
        assert_eq!(result.content, "");

        let with_offset_one = ToolRegistry::new()
            .execute(
                "read",
                context(json!({ "path": "empty.txt", "offset": 1 }), dir.path()),
            )
            .expect("execute");
        assert!(
            !with_offset_one.is_error,
            "content: {}",
            with_offset_one.content
        );
        assert_eq!(with_offset_one.content, "");
    }

    #[test]
    fn reads_empty_file_with_offset_beyond_end_is_error() {
        let dir = tempdir().expect("temp dir");
        let path = dir.path().join("empty.txt");
        std::fs::write(&path, "").expect("fixture");
        let result = ToolRegistry::new()
            .execute(
                "read",
                context(json!({ "path": "empty.txt", "offset": 2 }), dir.path()),
            )
            .expect("execute");
        assert!(result.is_error);
        assert!(
            result
                .content
                .contains("Offset 2 is beyond end of file (0 lines total)")
        );
    }
}
