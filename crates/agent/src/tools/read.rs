//! read 工具：读文件内容（可选 offset/limit 行范围），对齐 Pi read 语义（头部截断）。

use std::fs;

use serde_json::{Value, json};

use super::registry::{ExecuteContext, ToolError, ToolExecution, error_result, resolve_path};
use super::truncate::{DEFAULT_MAX_BYTES, TruncatedBy, format_size, truncate_head};

// 数字与 truncate::DEFAULT_MAX_LINES/DEFAULT_MAX_BYTES 保持一致（与 Pi 描述文本等价）。
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
        execute: execute,
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
    let bytes = match fs::read(&full_path) {
        Ok(bytes) => bytes,
        Err(error) => {
            return error_result(format!("Could not read file: {path}. {error}"));
        }
    };
    if ctx.signal.is_some_and(|signal| signal.is_cancelled()) {
        return error_result("Operation aborted");
    }
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let all_lines: Vec<&str> = text.split('\n').collect();
    let total_file_lines = all_lines.len();
    let start_line = offset.map_or(0, |offset| (offset as usize).saturating_sub(1));
    let start_line_display = start_line + 1;
    if start_line >= total_file_lines {
        return error_result(format!(
            "Offset {} is beyond end of file ({total_file_lines} lines total)",
            // 只有显式 offset 才会越过末尾（无 offset 时 start_line 恒为 0 < 总行数）。
            offset.unwrap_or(0)
        ));
    }
    let (selected_content, user_limited_lines) = match limit {
        Some(limit) => {
            let end = (start_line + limit as usize).min(total_file_lines);
            (
                all_lines[start_line..end].join("\n"),
                Some(end - start_line),
            )
        }
        None => (all_lines[start_line..].join("\n"), None),
    };
    let truncation = truncate_head(&selected_content);
    let output_text = if truncation.first_line_exceeds_limit {
        format!(
            "[Line {start_line_display} is {}, exceeds {} limit. Use bash: sed -n '{start_line_display}p' {path} | head -c {}]",
            format_size(all_lines[start_line].len()),
            format_size(DEFAULT_MAX_BYTES),
            DEFAULT_MAX_BYTES
        )
    } else if truncation.truncated {
        let end_line_display = start_line_display + truncation.output_lines - 1;
        let next_offset = end_line_display + 1;
        let note = match truncation.truncated_by {
            Some(TruncatedBy::Lines) => format!(
                "[Showing lines {start_line_display}-{end_line_display} of {total_file_lines}. Use offset={next_offset} to continue.]"
            ),
            _ => format!(
                "[Showing lines {start_line_display}-{end_line_display} of {total_file_lines} ({} limit). Use offset={next_offset} to continue.]",
                format_size(DEFAULT_MAX_BYTES)
            ),
        };
        format!("{}\n\n{note}", truncation.content)
    } else if let Some(limited) = user_limited_lines {
        if start_line + limited < total_file_lines {
            let remaining = total_file_lines - (start_line + limited);
            let next_offset = start_line + limited + 1;
            format!(
                "{}\n\n[{remaining} more lines in file. Use offset={next_offset} to continue.]",
                truncation.content
            )
        } else {
            truncation.content
        }
    } else {
        truncation.content
    };
    Ok(ToolExecution {
        content: output_text,
        is_error: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::registry::ToolRegistry;
    use crate::tools::test_support::context;
    use serde_json::json;
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
}
