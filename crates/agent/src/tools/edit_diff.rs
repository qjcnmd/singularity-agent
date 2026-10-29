//! 编辑结果使用标准 unified diff；首行保存完整增删数量，展示与历史共用。

use similar::{ChangeTag, TextDiff};

use super::registry::{ToolDisplay, ToolExecution};
use super::truncate::{DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES};

pub(super) fn result(path: &str, before: &str, after: &str) -> ToolExecution {
    let before = before.strip_prefix('\u{feff}').unwrap_or(before).replace("\r\n", "\n");
    let after = after.strip_prefix('\u{feff}').unwrap_or(after).replace("\r\n", "\n");
    let diff = TextDiff::from_lines(&before, &after);
    let (mut added, mut deleted) = (0, 0);
    for change in diff.iter_all_changes() {
        match change.tag() {
            ChangeTag::Insert => added += 1,
            ChangeTag::Delete => deleted += 1,
            ChangeTag::Equal => {}
        }
    }
    let patch = diff.unified_diff().context_radius(2).to_string();
    let mut output = format!("+{added} -{deleted}\n");
    for (index, line) in patch.lines().enumerate() {
        if index >= DEFAULT_MAX_LINES || output.len() + line.len() + 1 > DEFAULT_MAX_BYTES {
            output.push_str("[Diff truncated]\n");
            break;
        }
        output.push_str(line);
        output.push('\n');
    }
    let mut execution = ToolExecution::text(format!("Edited {path}.\n{output}"));
    execution.display = Some(Box::new(ToolDisplay { output, background_task_id: None }));
    execution
}
