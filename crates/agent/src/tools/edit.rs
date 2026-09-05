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
//! - **变更补丁反馈**：成功后返回替换统计及实际内容的 Unified Diff，覆盖单处和多处替换。

use std::fs;

use serde::Deserialize;
use serde_json::json;

use super::batch::path_key;
use super::observe::{Observed, current_version};
use super::registry::{ExecuteContext, ToolExecution, error_result};

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
    let Some(_) = matches.next() else {
        return error_result(format!(
            "Could not find the exact text in {path}. The old text must match exactly including all whitespace and newlines."
        ));
    };
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
    let projected_text = content.replace(old_string.as_str(), new_string.as_str());
    let patch = unified_diff(path, content, &projected_text);
    let summary = format!("Successfully replaced {occurrences} block(s) in {path}.\n\n{patch}");
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

/// 根据实际文件内容生成统一的变更展示；edit 与 write 共用。
pub(super) fn unified_diff(path: &str, before: &str, after: &str) -> String {
    similar::TextDiff::from_lines(before, after)
        .unified_diff()
        .context_radius(4)
        .header(path, path)
        .to_string()
}
