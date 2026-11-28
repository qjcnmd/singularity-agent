use std::fmt::Write as _;
use std::io::{Read, Seek, SeekFrom};

use protocol::{BackgroundTask, BackgroundTaskState, CommandOutputSnapshot};

use crate::tools::output::{
    COMMAND_HEAD_BYTES, format_head_tail_slice, format_spill_head_tail, format_spill_note,
};
use crate::tools::registry::{COMMAND_MAX_BYTES, COMMAND_MAX_LINES, ToolDisplay, ToolExecution};

pub(super) fn output_text(output: &CommandOutputSnapshot, cursor: u64) -> String {
    let cursor = cursor.min(output.total_bytes);
    let unread_bytes = output.total_bytes.saturating_sub(cursor);
    let retained_start = output.total_bytes.saturating_sub(output.text.len() as u64);
    let mut result = if unread_bytes == 0 {
        format!("Output bytes {cursor}–{}\n\n(no new output)", output.total_bytes)
    } else if cursor >= retained_start {
        let offset = usize::try_from(cursor - retained_start).unwrap_or(0);
        let offset = output.text.ceil_char_boundary(offset.min(output.text.len()));
        let body = format_head_tail_slice(&output.text[offset..]);
        format!("Output bytes {cursor}–{}\n\n{body}", output.total_bytes)
    } else if let Some(head_raw) =
        output.full_output.as_deref().and_then(|path| read_spill_head(path, cursor))
    {
        let body = format_spill_head_tail(&head_raw, &output.text, unread_bytes);
        format!("Output bytes {cursor}–{}\n\n{body}", output.total_bytes)
    } else {
        let byte_start = output.text.ceil_char_boundary(output.text.len().saturating_sub(COMMAND_MAX_BYTES));
        let line_start = output
            .text
            .strip_suffix('\n')
            .unwrap_or(&output.text)
            .rmatch_indices('\n')
            .nth(COMMAND_MAX_LINES - 1)
            .map_or(0, |(index, _)| index + 1);
        let text = &output.text[byte_start.max(line_start)..];
        let start = output.total_bytes.saturating_sub(text.len() as u64);
        let mut out = format!(
            "Output bytes {start}–{}\n\n{}",
            output.total_bytes,
            if text.is_empty() { "(no new output)" } else { text }
        );
        if start > cursor {
            let _ = write!(out, "\n[Output bytes {cursor}–{start} omitted.]");
        }
        out
    };
    if output.incomplete {
        result.push('\n');
        result.push_str(super::exec::OUTPUT_TRUNCATED_BACKGROUND_NOTE);
    }
    if let Some(note) = format_spill_note(output.full_output.as_deref(), output.file_error.as_deref()) {
        let _ = write!(result, "\n{note}");
    }
    result
}

fn read_spill_head(path: &str, offset: u64) -> Option<String> {
    let mut file = std::fs::File::open(path).ok()?;
    file.seek(SeekFrom::Start(offset)).ok()?;
    let mut buf = Vec::new();
    // Read 4 extra bytes (max UTF-8 char width) so valid_len still exceeds COMMAND_HEAD_BYTES when truncated mid-line.
    file.take((COMMAND_HEAD_BYTES + 4) as u64).read_to_end(&mut buf).ok()?;
    let valid_len = match std::str::from_utf8(&buf) {
        Ok(_) => buf.len(),
        Err(err) => err.valid_up_to(),
    };
    if valid_len == 0 {
        return None;
    }
    Some(String::from_utf8_lossy(&buf[..valid_len]).into_owned())
}

pub(super) fn task_result(task: &BackgroundTask, cursor: Option<u64>, include_output: bool) -> ToolExecution {
    let mut content = format!("Task {}: {}", task.task_id, state_text(&task.state));
    if let Some(cursor) = cursor {
        content.push('\n');
        content.push_str(&output_text(&task.model_output, cursor));
    }
    let mut summary = task_summary(task, &task.state);
    if include_output {
        summary["output"] = serde_json::json!(task.output);
    }
    let mut result = ToolExecution::text(content);
    result.display = Some(Box::new(ToolDisplay {
        output: serde_json::to_string(&summary).expect("task result serializes"),
        background_task_id: Some(task.task_id.clone()),
    }));
    result.is_error = matches!(task.state,
        BackgroundTaskState::Exited { exit_code, .. } if exit_code != 0)
        || matches!(task.state, BackgroundTaskState::Failed { .. } | BackgroundTaskState::Interrupted);
    result
}

pub(super) fn state_text(state: &BackgroundTaskState) -> String {
    match state {
        BackgroundTaskState::Running => "running".into(),
        BackgroundTaskState::Exited { exit_code, .. } => format!("exited (code {exit_code})"),
        BackgroundTaskState::Stopped { .. } => "stopped".into(),
        BackgroundTaskState::Failed { message, .. } => format!("failed: {message}"),
        BackgroundTaskState::Interrupted => "interrupted".into(),
    }
}

pub(super) fn task_summary(task: &BackgroundTask, state: &BackgroundTaskState) -> serde_json::Value {
    serde_json::json!({
        "taskId": task.task_id,
        "command": task.command,
        "description": task.description,
        "state": state,
    })
}
