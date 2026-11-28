use std::fmt::Write as _;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use serde::Deserialize;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use super::file_ops::FileReadTracker;
use super::registry::{
    ABORTED_MESSAGE, DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, READ_TOOL_NAME, ToolDisplay, ToolExecution,
    default_cap_summary, default_max_kb, error_result,
};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReadArgs {
    pub(crate) path: String,
    pub(crate) offset: Option<u64>,
    pub(crate) limit: Option<u64>,
}

pub(crate) fn spec() -> model::ToolDefinition {
    model::ToolDefinition {
        name: READ_TOOL_NAME.to_string(),
        description: format!(
            "Read a text file. Each output line is prefixed with its 1-indexed line number (e.g. \"1| ...\"). Text output is limited to {} (whichever is hit first). Use the returned offset to continue with unread lines. A line larger than {}KB is explicitly marked incomplete; use bash to read that line in byte ranges.",
            default_cap_summary(),
            default_max_kb()
        ),
        parameters_schema: json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path to the file to read (relative or absolute)" },
                "offset": { "type": "integer", "minimum": 1, "description": "Line number to start reading from (1-indexed)" },
                "limit": { "type": "integer", "minimum": 1, "description": format!("Maximum number of lines to read (omitted: default {DEFAULT_MAX_LINES} lines)") },
            },
            "required": ["path"],
            "additionalProperties": false,
        }),
    }
}

pub(crate) fn execute(
    args: &ReadArgs,
    cwd: &Path,
    file_reads: &FileReadTracker,
    signal: &CancellationToken,
) -> ToolExecution {
    if signal.is_cancelled() {
        return error_result(ABORTED_MESSAGE);
    }
    let full_path = cwd.join(&args.path);
    let file = match File::open(&full_path) {
        Ok(file) => file,
        Err(error) => return error_result(format!("Could not read file: {}. {error}", args.path)),
    };
    let metadata = file.metadata().ok();
    let canonical_key = full_path.canonicalize().ok().map(|path| app_core::workspace_key(&path));
    let mut reader = BufReader::with_capacity(64 * 1024, file);
    match reader.fill_buf() {
        Ok(bytes) if looks_like_image(bytes) => {
            return error_result(format!("{} is an image file, not text.", args.path));
        }
        Ok(bytes) if bytes.contains(&0) => {
            return error_result(format!("{} is a binary file, not text.", args.path));
        }
        Ok(_) => {}
        Err(error) => return error_result(format!("Could not read file: {}. {error}", args.path)),
    }
    let execution = execute_reader(&args.path, args.offset, args.limit, &mut reader, signal);
    if !execution.is_error
        && let (Some(key), Some(metadata)) = (canonical_key, metadata.as_ref())
    {
        file_reads.record_metadata(key, metadata);
    }
    execution
}

fn looks_like_image(bytes: &[u8]) -> bool {
    match image::guess_format(bytes) {
        Ok(
            image::ImageFormat::Png
            | image::ImageFormat::Jpeg
            | image::ImageFormat::WebP
            | image::ImageFormat::Gif,
        ) => true,
        Ok(image::ImageFormat::Bmp) => {
            bytes.len() >= 14
                && (bytes.contains(&0)
                    || std::str::from_utf8(bytes).is_err_and(|err| err.error_len().is_some()))
        }
        _ => false,
    }
}

fn execute_reader(
    path: &str,
    offset: Option<u64>,
    limit: Option<u64>,
    reader: &mut impl BufRead,
    signal: &CancellationToken,
) -> ToolExecution {
    if offset == Some(0) {
        return error_result("offset must be at least 1");
    }
    if limit == Some(0) {
        return error_result("limit must be at least 1");
    }
    let start_line =
        offset.map_or(0, |offset| usize::try_from(offset).unwrap_or(usize::MAX).saturating_sub(1));
    let start_line_display = start_line + 1;
    let user_line_limit = limit.map_or(DEFAULT_MAX_LINES, |limit| {
        usize::try_from(limit).unwrap_or(DEFAULT_MAX_LINES).min(DEFAULT_MAX_LINES)
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
        if line_number < start_line {
            match reader.skip_until(b'\n') {
                Ok(0) => break,
                Ok(_) => {
                    line_number += 1;
                    continue;
                }
                Err(error) => return error_result(format!("Could not read file: {path}. {error}")),
            }
        }
        let mut line_reader = std::io::Read::take(&mut *reader, (DEFAULT_MAX_BYTES + 4) as u64);
        let line = match read_line_bytes(&mut line_reader) {
            Ok(Some(line)) => line,
            Ok(None) => break,
            Err(error) => {
                return error_result(format!("Could not read file: {path}. {error}"));
            }
        };
        if signal.is_cancelled() {
            return error_result(ABORTED_MESSAGE);
        }
        let line_slice = if line_number == 0 {
            app_core::strip_utf8_bom_bytes(&line)
        } else {
            &line
        };
        line_number += 1;
        let text = String::from_utf8_lossy(line_slice);
        let prefix_len = line_number.ilog10() as usize + 3;
        let next_bytes =
            state.selected_bytes + prefix_len + text.len() + usize::from(!state.selected.is_empty());
        if next_bytes > DEFAULT_MAX_BYTES {
            if state.selected.is_empty() {
                let text_budget = DEFAULT_MAX_BYTES.saturating_sub(prefix_len);
                let prefix = &text[..text.floor_char_boundary(text_budget)];
                state.selected.push(format!("{prefix}…[truncated]"));
                state.incomplete_line = true;
            }
            state.selected_truncated = true;
            break;
        }
        state.selected.push(text.into_owned());
        state.selected_bytes = next_bytes;
        if state.selected.len() >= user_line_limit {
            state.selected_truncated = match reader.fill_buf() {
                Ok(remaining) => !remaining.is_empty(),
                Err(error) => return error_result(format!("Could not read file: {path}. {error}")),
            };
            break;
        }
    }

    if line_number == 0 && start_line_display == 1 {
        return ToolExecution::text(String::new()).with_read_source(read_source(start_line_display, &state));
    }

    if start_line >= line_number {
        return error_result(format!(
            "Offset {start_line_display} is beyond end of file ({line_number} lines total)"
        ));
    }
    let (llm_output, display_output) = render_read_output(start_line_display, &state);
    let mut execution =
        ToolExecution::text(llm_output).with_read_source(read_source(start_line_display, &state));
    execution.display = Some(Box::new(ToolDisplay {
        output: display_output,
        background_task_id: None,
    }));
    execution
}

fn read_line_bytes(reader: &mut impl BufRead) -> std::io::Result<Option<Vec<u8>>> {
    let mut bytes = Vec::new();
    if reader.read_until(b'\n', &mut bytes)? == 0 {
        return Ok(None);
    }
    if bytes.ends_with(b"\n") {
        bytes.pop();
        if bytes.last() == Some(&b'\r') {
            bytes.pop();
        }
    }
    Ok(Some(bytes))
}

fn read_source(start_line_display: usize, state: &ReadState) -> protocol::ReadSource {
    protocol::ReadSource {
        start_line: start_line_display as u64,
        line_count: state.selected.len() as u64,
    }
}

struct ReadState {
    selected: Vec<String>,
    selected_bytes: usize,
    selected_truncated: bool,
    incomplete_line: bool,
}

fn render_read_output(start_line_display: usize, state: &ReadState) -> (String, String) {
    let display_body = state.selected.join("\n");
    let mut numbered_body = String::with_capacity(display_body.len() + state.selected.len() * 6);
    for (index, line) in state.selected.iter().enumerate() {
        if index > 0 {
            numbered_body.push('\n');
        }
        let line_no = start_line_display + index;
        let _ = write!(numbered_body, "{line_no}| {line}");
    }
    if state.selected_truncated && !state.selected.is_empty() {
        let end_line_display = start_line_display + (state.selected.len() - 1);
        let next_offset = end_line_display + 1;
        let footer = if state.incomplete_line {
            format!(
                "[Line {start_line_display} exceeds {}KB; only its prefix is shown. Use bash to read this line in byte ranges. For following lines use offset={next_offset}.]",
                default_max_kb()
            )
        } else {
            format!(
                "[Showing lines {start_line_display}-{end_line_display}. File continues; use offset={next_offset} to continue.]"
            )
        };
        return (format!("{numbered_body}\n\n{footer}"), format!("{display_body}\n\n{footer}"));
    }
    (numbered_body, display_body)
}
