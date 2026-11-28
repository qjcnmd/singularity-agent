use std::fmt::Write as _;

use protocol::{BackgroundTask, BackgroundTaskState, CommandOutputSnapshot};

use crate::tools::output::{format_size, format_spill_note};
use crate::tools::registry::{COMMAND_MAX_BYTES, COMMAND_MAX_LINES, ToolDisplay, ToolExecution};

pub(super) fn output_text(output: &CommandOutputSnapshot) -> String {
    let mut result = if output.text.is_empty() {
        "(no output)".to_owned()
    } else {
        let byte_start = output.text.ceil_char_boundary(output.text.len().saturating_sub(COMMAND_MAX_BYTES));
        let line_start = output
            .text
            .strip_suffix('\n')
            .unwrap_or(&output.text)
            .rmatch_indices('\n')
            .nth(COMMAND_MAX_LINES - 1)
            .map_or(0, |(index, _)| index + 1);
        let tail = &output.text[byte_start.max(line_start)..];
        if output.total_bytes > tail.len() as u64 {
            format!(
                "[... {} earlier output omitted; showing latest {} of {} ...]\n\n{tail}",
                format_size(output.total_bytes.saturating_sub(tail.len() as u64)),
                format_size(tail.len() as u64),
                format_size(output.total_bytes),
            )
        } else {
            tail.to_owned()
        }
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

pub(super) fn task_result(task: &BackgroundTask, include_output: bool) -> ToolExecution {
    let mut content = format!("Task {}: {}", task.task_id, state_text(&task.state));
    if include_output {
        content.push('\n');
        content.push_str(&output_text(&task.model_output));
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
