//! edit 工具：单文件精确文本块替换。
//!
//! - **先读后改**：目标必须是本会话 `read` 过（或本会话刚 `write`/`edit` 过）且
//!   版本未变的文件，防止把没见过或已被外部改动的内容冲掉；事实源见 `observe`。
//!   这是防误覆盖的正确性防护，不限制路径，也不是权限边界。
//! - **唯一性匹配约束**：入参包含 `path`、`oldString` 与 `newString`；`oldString`
//!   必须在目标文件中严格唯一匹配一次，若未找到匹配或匹配到多个位置，均返回明确
//!   错误并拒绝修改。`replaceAll` 为 true 时改为替换全部匹配位置。
//! - **原文字节匹配**：`oldString` 与文件原始文本逐字节匹配并替换；文件编码（含
//!   UTF-8 BOM）与换行风格原样保持，不做任何转换。
//! - **变更补丁反馈**：单处替换成功后返回替换统计以及 Unified Diff 格式的补丁文本
//!   供模型核对；`replaceAll` 命中多处时只回替换数量。

use std::fmt::Write as _;
use std::fs;

use serde::Deserialize;
use serde_json::json;

use super::batch::path_key;
use super::observe::{Observed, current_version};
use super::registry::{ExecuteContext, ToolExecution, error_result};
use super::truncate::split_lines;

pub(crate) const DESCRIPTION: &str = "Edit a single file using exact text replacement. The file must have been read earlier in this session. oldString must match exactly once in the file (unique) unless replaceAll is true, in which case every match is replaced. If two changes affect the same block or nearby lines, merge them into one edit instead of emitting overlapping edits. Do not include large unchanged regions just to connect distant changes.";
pub(crate) const NAME: &str = "edit";

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct EditArgs {
    pub(crate) path: String,
    pub(crate) old_string: String,
    pub(crate) new_string: String,
    #[serde(default)]
    pub(crate) replace_all: bool,
}

pub(crate) fn spec() -> super::registry::ToolSpec {
    super::registry::ToolSpec {
        name: NAME,
        snippet: "Make precise file edits with exact text replacement",
        description: DESCRIPTION,
        parameters: json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path to the file to edit (relative or absolute)" },
                "oldString": { "type": "string", "description": "Exact text for one targeted replacement. It must be unique in the original file." },
                "newString": { "type": "string", "description": "Replacement text for this targeted edit." },
                "replaceAll": { "type": "boolean", "description": "Replace every match instead of requiring a unique one (default: false)." },
            },
            "required": ["path", "oldString", "newString"],
            "additionalProperties": false,
        }),
    }
}

pub(crate) fn execute(args: &EditArgs, ctx: ExecuteContext<'_>) -> ToolExecution {
    let path = &args.path;
    let old_string = &args.old_string;
    let new_string = &args.new_string;
    if let Some(aborted) = ctx.abort_if_cancelled() {
        return aborted;
    }
    let full_path = ctx.cwd.join(path);
    if full_path.is_dir() {
        return error_result(format!("Could not edit file: {path}. Path is not a file."));
    }
    // 防误覆盖闸门：本会话没见过这个文件，或见过的版本已经不是眼下这一版，就不许改。
    let key = path_key(ctx.cwd, path);
    match ctx.observed.observed(&key) {
        Observed::Unseen => {
            return error_result(format!(
                "Could not edit file: {path}. It has not been read in this session; read it first, then retry."
            ));
        }
        Observed::Absent => {
            return error_result(format!(
                "Could not edit file: {path}. It was confirmed missing earlier in this session; read it first, then retry."
            ));
        }
        Observed::Present(version) => {
            if current_version(&full_path) != Some(version) {
                return error_result(format!(
                    "Could not edit file: {path}. It changed since it was read; read it again, then retry."
                ));
            }
        }
    }
    let original = match fs::read(&full_path) {
        Ok(content) => content,
        Err(error) => {
            return error_result(format!("Could not edit file: {path}. {error}"));
        }
    };
    if let Some(aborted) = ctx.abort_if_cancelled() {
        return aborted;
    }
    let content = match std::str::from_utf8(&original) {
        Ok(content) => content,
        Err(error) => {
            return error_result(format!("Could not edit file: {path}. {error}"));
        }
    };
    if old_string.is_empty() {
        return error_result(format!("oldString must not be empty in {path}."));
    }
    let mut matches = content.match_indices(old_string.as_str());
    let Some((match_start, matched)) = matches.next() else {
        return error_result(format!(
            "Could not find the exact text in {path}. The old text must match exactly including all whitespace and newlines."
        ));
    };
    let match_end = match_start.saturating_add(matched.len());
    let occurrences = 1usize.saturating_add(matches.count());
    if occurrences > 1 && !args.replace_all {
        return error_result(format!(
            "Found {occurrences} occurrences of the text in {path}. The text must be unique. Please provide more context to make it unique, or set replaceAll to true."
        ));
    }
    if old_string == new_string {
        return error_result(format!(
            "No changes made to {path}. The replacement produced identical content. This might indicate an issue with special characters or the text not existing as expected."
        ));
    }
    // 单处替换给出局部补丁供核对；replaceAll 命中多处时只给数量，多站点整文件
    // diff 会把正文淹没，模型也拿不到比原文更有用的信息。
    let (projected_text, summary) = if occurrences == 1 {
        let mut projected = String::with_capacity(
            content
                .len()
                .saturating_sub(matched.len())
                .saturating_add(new_string.len()),
        );
        projected.push_str(&content[..match_start]);
        projected.push_str(new_string);
        projected.push_str(&content[match_end..]);
        let patch = generate_patch(
            path,
            content,
            &projected,
            new_string,
            match_start,
            match_end,
        );
        (
            projected,
            format!("Successfully replaced 1 block(s) in {path}.\n\n{patch}"),
        )
    } else {
        (
            content.replace(old_string.as_str(), new_string.as_str()),
            format!("Successfully replaced {occurrences} block(s) in {path}."),
        )
    };
    if let Err(error) =
        singularity_core::atomic_replace_bytes(&full_path, projected_text.as_bytes())
    {
        return error_result(format!("Could not edit file: {path}. {error}"));
    }
    // 本会话刚改出的内容不必重读：补记新版本，后续 edit 直接接着改。
    if let Some(version) = current_version(&full_path) {
        ctx.observed.record(&key, Observed::Present(version));
    }
    ToolExecution {
        content: summary,
        is_error: false,
    }
}

/// 生成单文本块修改前后的 Unified Diff 补丁展示文本（包含前后各 4 行上下文）。
///
/// 仅围绕已知命中区做局部上下文 diff，不对整份文件做全量双端 split。
fn generate_patch(
    path: &str,
    old: &str,
    new: &str,
    new_block: &str,
    match_start: usize,
    match_end: usize,
) -> String {
    const CONTEXT_LINES: usize = 4;
    // 行边界只认 `\n`：`\r` 计入行内容，与 split_lines 的展示口径一致。
    let line_start = |text: &str, position: usize| -> usize {
        let prefix = &text[..position.min(text.len())];
        prefix.rfind('\n').map_or(0, |index| index + 1)
    };
    let line_end = |text: &str, position: usize| -> usize {
        let from = position.min(text.len());
        text[from..]
            .find('\n')
            .map_or(text.len(), |index| from + index + 1)
    };

    let removed_line_start = line_start(old, match_start);
    let removed_line_end = line_end(old, match_end.saturating_sub(1));
    // 上下文各自向两侧扩展至多 CONTEXT_LINES 个整行，遇文件边界即停。
    let mut before_start = removed_line_start;
    for _ in 0..CONTEXT_LINES {
        if before_start == 0 {
            break;
        }
        before_start = line_start(old, before_start - 1);
    }
    let mut after_end = removed_line_end;
    for _ in 0..CONTEXT_LINES {
        if after_end >= old.len() {
            break;
        }
        after_end = line_end(old, after_end);
    }

    let new_added_end = if new_block.is_empty() {
        match_start
    } else {
        line_end(
            new,
            match_start.saturating_add(new_block.len().saturating_sub(1)),
        )
    };

    let context_before = &old[before_start..removed_line_start];
    let removed = &old[removed_line_start..removed_line_end];
    let added = &new[removed_line_start..new_added_end];
    let context_after = &old[removed_line_end..after_end];

    let old_start = line_number_at(old, before_start);
    let old_count = split_lines(context_before).len()
        + split_lines(removed).len()
        + split_lines(context_after).len();
    let new_count = split_lines(context_before).len()
        + split_lines(added).len()
        + split_lines(context_after).len();

    let mut patch = String::new();
    let _ = writeln!(patch, "--- {path}");
    let _ = writeln!(patch, "+++ {path}");
    let _ = writeln!(
        patch,
        "@@ -{} +{} @@",
        range(old_start, old_count),
        range(old_start, new_count)
    );
    for line in split_lines(context_before) {
        let _ = writeln!(patch, " {line}");
    }
    for line in split_lines(removed) {
        let _ = writeln!(patch, "-{line}");
    }
    for line in split_lines(added) {
        let _ = writeln!(patch, "+{line}");
    }
    for line in split_lines(context_after) {
        let _ = writeln!(patch, " {line}");
    }
    patch
}

/// 文本中 `position` 处所在行（1 起）的绝对行号。
fn line_number_at(text: &str, position: usize) -> usize {
    text[..position.min(text.len())]
        .bytes()
        .filter(|byte| *byte == b'\n')
        .count()
        .saturating_add(1)
}

/// unified patch hunk 头中的行号范围（count 为 1 时省略 ",1"）。
fn range(start: usize, count: usize) -> String {
    if count == 1 {
        start.to_string()
    } else {
        format!("{start},{count}")
    }
}
