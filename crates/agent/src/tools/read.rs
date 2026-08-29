//! read 工具：有界流式读取指定文件内容，支持基于 `offset` 与 `limit` 的行范围读取。

use std::fs::File;
use std::io::{BufRead, BufReader};

use serde::Deserialize;
use serde_json::{Value, json};
use singularity_core::CancellationToken;

use super::registry::{ABORTED_MESSAGE, ExecuteContext, ToolExecution, error_result, resolve_path};
use super::truncate::{DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES};

/// 单行硬上限：一行超过 4 MiB 视为不可安全读取的输入。
const MAX_READ_LINE_BYTES: usize = 4 * 1024 * 1024;
pub(crate) const DESCRIPTION: &str = "Read the contents of a text file. Output is truncated to 2000 lines or 50KB (whichever is hit first). Use offset/limit for large files. When you need the full file, continue with offset until complete.";
pub(crate) const NAME: &str = "read";

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReadArgs {
    pub(crate) path: String,
    pub(crate) offset: Option<u64>,
    pub(crate) limit: Option<u64>,
}

pub(crate) fn parameters() -> Value {
    json!({
        "type": "object",
        "properties": {
            "path": { "type": "string", "description": "Path to the file to read (relative or absolute)" },
            "offset": { "type": "integer", "description": "Line number to start reading from (1-indexed)" },
            "limit": { "type": "integer", "description": "Maximum number of lines to read (omitted: default 2000 lines)" },
        },
        "required": ["path"],
        "additionalProperties": false,
    })
}

pub(crate) fn spec() -> super::registry::ToolSpec {
    super::registry::ToolSpec {
        name: NAME,
        description: DESCRIPTION,
        parameters: parameters(),
        prepare: |raw| {
            super::registry::deserialize_args_or_error::<ReadArgs>(raw)
                .map(super::registry::PreparedTool::Read)
        },
    }
}

pub(crate) fn execute(args: &ReadArgs, ctx: ExecuteContext<'_>) -> ToolExecution {
    if let Some(aborted) = ctx.abort_if_cancelled() {
        return aborted;
    }
    let full_path = resolve_path(ctx.cwd, &args.path);
    let file = match File::open(&full_path) {
        Ok(file) => file,
        Err(error) => {
            return error_result(format!("Could not read file: {}. {error}", args.path));
        }
    };
    let mut reader = BufReader::with_capacity(64 * 1024, file);
    execute_reader(&args.path, args.offset, args.limit, &mut reader, ctx.signal)
}

fn execute_reader(
    path: &str,
    offset: Option<u64>,
    limit: Option<u64>,
    reader: &mut impl BufRead,
    signal: Option<&CancellationToken>,
) -> ToolExecution {
    if signal.is_some_and(CancellationToken::is_cancelled) {
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
    };
    let mut line_number = 0usize;
    loop {
        if signal.is_some_and(CancellationToken::is_cancelled) {
            return error_result(ABORTED_MESSAGE);
        }
        let line = match super::line::read_bounded_line(reader, MAX_READ_LINE_BYTES) {
            Ok(Some(line)) => line,
            Ok(None) => break,
            Err(ReadFailure::OverLimit { prefix, .. }) => {
                // 巨型行软化：以截断前缀 + 行尾标记占位，不失败、不跳过，
                // 后续行继续照常读取。
                line_number += 1;
                if line_number.saturating_sub(start_line) == 0 {
                    continue;
                }
                if state.selected.len() >= user_line_limit {
                    break;
                }
                push_oversized_line(&mut state, prefix);
                break;
            }
            Err(error) => {
                return error_result(format!("Could not read file: {path}. {error}"));
            }
        };
        if signal.is_some_and(CancellationToken::is_cancelled) {
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
            // 巨型行软化：单行超预算时截断 + 行尾标记，不失败、不跳过。
            push_oversized_line(&mut state, line);
            break;
        }
        state
            .selected
            .push(String::from_utf8_lossy(&line).into_owned());
        state.selected_bytes = next_bytes;
        if state.selected.len() >= user_line_limit {
            // 收集满 limit 即停：只需确认文件是否还有后续，不再扫到 EOF 统计行数。
            state.selected_truncated =
                match super::line::read_bounded_line(reader, MAX_READ_LINE_BYTES) {
                    Ok(Some(_)) => true,
                    Ok(None) => false,
                    Err(_) => true,
                };
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
    }
}

/// 把一条超出输出预算的行截断到剩余预算并追加行尾标记，作为选中窗口的
/// 最后一项；标记后窗口视为已满。
fn push_oversized_line(state: &mut ReadState, line: Vec<u8>) {
    let separator = usize::from(!state.selected.is_empty());
    let available = DEFAULT_MAX_BYTES.saturating_sub(state.selected_bytes + separator);
    let mut content = String::from_utf8_lossy(&line).into_owned();
    if content.len() > available {
        let mut end = available;
        while !content.is_char_boundary(end) {
            end -= 1;
        }
        content = content[..end].to_string();
    }
    state.selected.push(format!("{content}…[truncated]"));
    state.selected_bytes = DEFAULT_MAX_BYTES;
    state.selected_truncated = true;
}

struct ReadState {
    selected: Vec<String>,
    selected_bytes: usize,
    selected_truncated: bool,
}

fn render_read_output(start_line_display: usize, state: &ReadState) -> String {
    let selected_content = state.selected.join("\n");
    if state.selected_truncated && !state.selected.is_empty() {
        let end_line_display =
            start_line_display.saturating_add(state.selected.len().saturating_sub(1));
        let next_offset = end_line_display.saturating_add(1);
        return format!(
            "{selected_content}\n\n[Showing lines {start_line_display}-{end_line_display}. File continues; use offset={next_offset} to continue.]"
        );
    }
    selected_content
}

/// 有界读取一行：单行超过 MAX_READ_LINE_BYTES 时 fail closed，不分配无界内存。
type ReadFailure = super::line::LineFailure;
