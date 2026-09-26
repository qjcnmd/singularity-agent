//! read 工具：有界地流式读取指定文件，支持按 offset 与 limit 读取一段行范围。

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::sync::LazyLock;

use serde::Deserialize;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use super::line::{LineFailure, MAX_READ_LINE_BYTES};
use super::registry::{ABORTED_MESSAGE, ExecuteContext, ToolExecution, error_result};
use super::truncate::{DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, default_cap_summary, default_max_kb};

static DESCRIPTION: LazyLock<String> = LazyLock::new(|| {
    format!(
        "Read the contents of a text file. Output is limited to {} (whichever is hit first). Use the returned offset to continue with unread lines. A line larger than {}KB is explicitly marked incomplete; use bash to read that line in byte ranges.",
        default_cap_summary(),
        default_max_kb()
    )
});
const NAME: &str = "read";
const SNIPPET: &str = "Read file contents";

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
        description: &DESCRIPTION,
        parameters: json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path to the file to read (relative or absolute)" },
                "offset": { "type": "integer", "description": "Line number to start reading from (1-indexed)" },
                "limit": { "type": "integer", "description": format!("Maximum number of lines to read (omitted: default {DEFAULT_MAX_LINES} lines)") },
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
    let start_line = offset.map_or(0, |offset| (offset as usize).saturating_sub(1));
    let start_line_display = start_line + 1;
    let user_line_limit = limit.map_or(DEFAULT_MAX_LINES, |limit| {
        usize::try_from(limit)
            .unwrap_or(DEFAULT_MAX_LINES)
            .min(DEFAULT_MAX_LINES)
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
            Err(LineFailure::OverLimit { prefix }) => {
                line_number += 1;
                // 超长行落在读取起点之前，跳过它继续读后面的行。
                if line_number.saturating_sub(start_line) == 0 {
                    continue;
                }
                if state.selected.len() >= user_line_limit {
                    break;
                }
                finish_at_byte_limit(&mut state, prefix);
                break;
            }
            Err(LineFailure::Io(error)) => {
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
        // 选中窗口已经满了（例如 limit 为 0）时，不必再读或换算后面的行。
        if state.selected.len() >= user_line_limit {
            break;
        }
        // 展示预算按实际发回的文本来算：非法 UTF-8 字节被替换成 U+FFFD 后会变长，
        // 若按原始字节数计算，就可能发出超过预算的正文。
        let text = String::from_utf8_lossy(&line);
        let next_bytes = state
            .selected_bytes
            .saturating_add(text.len())
            .saturating_add(usize::from(!state.selected.is_empty()));
        if next_bytes > DEFAULT_MAX_BYTES {
            finish_at_byte_limit(&mut state, line);
            break;
        }
        state.selected.push(text.into_owned());
        state.selected_bytes = next_bytes;
        if state.selected.len() >= user_line_limit {
            // 收满 limit 就停：这时只需确认文件后面还有没有内容，不必一直扫到 EOF。
            state.selected_truncated = match reader.fill_buf() {
                Ok(remaining) => !remaining.is_empty(),
                Err(error) => return error_result(format!("Could not read file: {path}. {error}")),
            };
            break;
        }
    }

    if line_number == 0 {
        if start_line_display > 1 {
            return error_result(format!(
                "Offset {start_line_display} is beyond end of file (0 lines total)"
            ));
        }
        return ToolExecution::text(String::new())
            .with_read_source(read_source(start_line_display, &state));
    }

    if start_line >= line_number {
        return error_result(format!(
            "Offset {start_line_display} is beyond end of file ({line_number} lines total)"
        ));
    }
    let output_text = render_read_output(start_line_display, &state);
    ToolExecution::text(output_text).with_read_source(read_source(start_line_display, &state))
}

/// read 实际读到的源文件范围；起始行和正文行数在这里算好，说明文字不计入正文行数。
fn read_source(start_line_display: usize, state: &ReadState) -> singularity_protocol::ReadSource {
    singularity_protocol::ReadSource {
        start_line: start_line_display as u64,
        line_count: state.selected.len() as u64,
    }
}

/// 已经收集到完整行时，把当前这行留给下一页；只有单行本身就超预算，才返回不完整的前缀。
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
                "{selected_content}\n\n[Line {start_line_display} exceeds {}KB; only its prefix is shown. Use bash to read this line in byte ranges. For following lines use offset={next_offset}.]",
                default_max_kb()
            );
        }
        return format!(
            "{selected_content}\n\n[Showing lines {start_line_display}-{end_line_display}. File continues; use offset={next_offset} to continue.]"
        );
    }
    selected_content
}
