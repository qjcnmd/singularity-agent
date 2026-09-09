//! read 工具：有界流式读取指定文件内容，支持基于 offset 与 limit 的行范围读取。

use std::fs::File;
use std::io::{BufRead, BufReader};

use serde::Deserialize;
use serde_json::json;
use singularity_core::CancellationToken;

use super::line::MAX_READ_LINE_BYTES;
use super::registry::{ABORTED_MESSAGE, ExecuteContext, ToolExecution, error_result};
use super::truncate::{DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES};

pub(crate) const DESCRIPTION: &str = "Read the contents of a text file. Output is limited to 2000 lines or 50KB (whichever is hit first). Use the returned offset to continue with unread lines. A line larger than 50KB is explicitly marked incomplete; use bash to read that line in byte ranges.";
pub(crate) const NAME: &str = "read";
pub(crate) const SNIPPET: &str = "Read file contents";

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReadArgs {
    pub(crate) path: String,
    pub(crate) offset: Option<u64>,
    pub(crate) limit: Option<u64>,
}

pub(crate) fn spec() -> super::registry::ToolSpec {
    super::registry::ToolSpec {
        name: NAME,
        snippet: SNIPPET,
        description: DESCRIPTION,
        parameters: json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path to the file to read (relative or absolute)" },
                "offset": { "type": "integer", "description": "Line number to start reading from (1-indexed)" },
                "limit": { "type": "integer", "description": "Maximum number of lines to read (omitted: default 2000 lines)" },
            },
            "required": ["path"],
            "additionalProperties": false,
        }),
    }
}

pub(crate) fn execute(args: &ReadArgs, ctx: ExecuteContext<'_>) -> ToolExecution {
    let full_path = ctx.cwd.join(&args.path);
    let file = match File::open(&full_path) {
        Ok(file) => file,
        Err(error) => return error_result(format!("Could not read file: {}. {error}", args.path)),
    };
    let mut reader = BufReader::with_capacity(64 * 1024, file);
    execute_reader(&args.path, args.offset, args.limit, &mut reader, ctx.signal)
}

fn execute_reader(
    path: &str,
    offset: Option<u64>,
    limit: Option<u64>,
    reader: &mut impl BufRead,
    signal: &CancellationToken,
) -> ToolExecution {
    if signal.is_cancelled() {
        return error_result(ABORTED_MESSAGE);
    }
    let start_line = offset.map_or(0, |offset| (offset as usize).saturating_sub(1));
    let start_line_display = start_line + 1;
    let user_line_limit = limit.map_or(DEFAULT_MAX_LINES, |limit| {
        usize::try_from(limit).unwrap_or(DEFAULT_MAX_LINES)
    });
    let mut state = ReadState {
        selected: Vec::new(),
        selected_bytes: 0,
        selected_truncated: false,
        incomplete_line: false,
    };
    let mut line_number = 0usize;
    loop {
        if signal.is_cancelled() {
            return error_result(ABORTED_MESSAGE);
        }
        let line = match super::line::read_bounded_line(reader, MAX_READ_LINE_BYTES) {
            Ok(Some(line)) => line,
            Ok(None) => break,
            Err(ReadFailure::OverLimit { prefix, .. }) => {
                line_number += 1;
                if line_number.saturating_sub(start_line) == 0 {
                    continue;
                }
                if state.selected.len() >= user_line_limit {
                    break;
                }
                finish_at_byte_limit(&mut state, prefix);
                break;
            }
            Err(error) => {
                return error_result(format!("Could not read file: {path}. {error}"));
            }
        };
        if signal.is_cancelled() {
            return error_result(ABORTED_MESSAGE);
        }
        line_number += 1;
        let selected_position = line_number.saturating_sub(start_line);
        if selected_position == 0 {
            continue;
        }
        // 选中窗口已满（例如 limit 为 0）时无需再读取或换算后续行。
        if state.selected.len() >= user_line_limit {
            break;
        }
        let next_bytes = state
            .selected_bytes
            .saturating_add(line.len())
            .saturating_add(usize::from(!state.selected.is_empty()));
        if next_bytes > DEFAULT_MAX_BYTES {
            finish_at_byte_limit(&mut state, line);
            break;
        }
        state
            .selected
            .push(String::from_utf8_lossy(&line).into_owned());
        state.selected_bytes = next_bytes;
        if state.selected.len() >= user_line_limit {
            // 收集满 limit 即停：只需确认文件是否还有后续，无需扫到 EOF。
            state.selected_truncated = !matches!(
                super::line::read_bounded_line(reader, MAX_READ_LINE_BYTES),
                Ok(None)
            );
            break;
        }
    }

    if line_number == 0 {
        if offset.is_some_and(|offset| offset > 1) {
            return error_result(format!(
                "Offset {} is beyond end of file (0 lines total)",
                offset.unwrap_or(0)
            ));
        }
        return ToolExecution {
            content: String::new(),
            is_error: false,
            diff: None,
            duration_ms: None,
        };
    }

    if start_line >= line_number {
        return error_result(format!(
            "Offset {} is beyond end of file ({line_number} lines total)",
            offset.unwrap_or(0)
        ));
    }
    let output_text = render_read_output(start_line_display, &state);
    ToolExecution {
        content: output_text,
        is_error: false,
        diff: None,
        duration_ms: None,
    }
}

/// 已有完整行时把当前行留给下一页；只有单行本身超预算才返回不完整前缀。
fn finish_at_byte_limit(state: &mut ReadState, line: Vec<u8>) {
    if state.selected.is_empty() {
        let content = String::from_utf8_lossy(&line);
        let (content, _) = singularity_core::utf8_prefix(&content, DEFAULT_MAX_BYTES);
        state.selected.push(format!("{content}…[truncated]"));
        state.incomplete_line = true;
    }
    state.selected_truncated = true;
}

struct ReadState {
    selected: Vec<String>,
    selected_bytes: usize,
    selected_truncated: bool,
    incomplete_line: bool,
}

fn render_read_output(start_line_display: usize, state: &ReadState) -> String {
    let selected_content = state.selected.join("\n");
    if state.selected_truncated && !state.selected.is_empty() {
        let end_line_display =
            start_line_display.saturating_add(state.selected.len().saturating_sub(1));
        let next_offset = end_line_display.saturating_add(1);
        if state.incomplete_line {
            return format!(
                "{selected_content}\n\n[Line {start_line_display} exceeds 50KB; only its prefix is shown. Use bash to read this line in byte ranges. For following lines use offset={next_offset}.]"
            );
        }
        return format!(
            "{selected_content}\n\n[Showing lines {start_line_display}-{end_line_display}. File continues; use offset={next_offset} to continue.]"
        );
    }
    selected_content
}

/// 行读取失败类型：与 grep 共用 super::line 的有界读取原语。
type ReadFailure = super::line::LineFailure;
