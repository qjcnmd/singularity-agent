//! 模型任务结果与工作台累计展示分开；游标只计算去除终端控制序列后的正文。

use std::fmt::Write as _;

use protocol::{BackgroundTask, BackgroundTaskState, CommandOutputSnapshot};

use crate::tools::registry::{DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, ToolDisplay, ToolExecution};

pub(super) fn output_text(output: &CommandOutputSnapshot, cursor: u64) -> String {
    let cursor = cursor.min(output.total_bytes);
    let retained_start = output.total_bytes.saturating_sub(output.text.len() as u64);
    let offset = usize::try_from(cursor.saturating_sub(retained_start)).unwrap_or(0);
    let offset = output.text.ceil_char_boundary(offset.min(output.text.len()));
    let unread = &output.text[offset..];
    let byte_start = unread.ceil_char_boundary(unread.len().saturating_sub(DEFAULT_MAX_BYTES));
    let line_start = unread
        .strip_suffix('\n')
        .unwrap_or(unread)
        .rmatch_indices('\n')
        .nth(DEFAULT_MAX_LINES - 1)
        .map_or(0, |(index, _)| index + 1);
    let text = &unread[byte_start.max(line_start)..];
    let start = output.total_bytes.saturating_sub(text.len() as u64);
    let mut result = format!(
        "Output bytes {start}–{}\n\n{}",
        output.total_bytes,
        if text.is_empty() { "(no new output)" } else { text }
    );
    if start > cursor {
        let _ = write!(result, "\n[Output bytes {cursor}–{start} omitted.]");
    }
    if output.incomplete {
        result.push('\n');
        result.push_str(super::exec::OUTPUT_TRUNCATED_BACKGROUND_NOTE);
    }
    if let Some(error) = &output.file_error {
        let _ = write!(result, "\nFull output could not be saved: {error}");
    } else if let Some(path) = &output.full_output {
        let _ = write!(result, "\nFull output: {path}");
    }
    result
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
