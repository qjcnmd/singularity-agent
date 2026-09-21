//! edit 工具：在单个文件里做精确的文本块替换。
//!
//! oldString 必须在目标文件里严格唯一地匹配一次（replaceAll 为 true 时替换全部匹配）；
//! LF 与 CRLF 视为同一种换行，其余字符仍要求精确匹配，替换文本沿用目标块的行尾，未
//! 替换部分和 UTF-8 BOM 保持原字节；成功后返回替换统计与实际内容的 Unified Diff。

use std::fs;

use serde::Deserialize;
use serde_json::json;

use super::mutation::{acquire_mutation_lock, mutation_lock};
use super::registry::{ExecuteContext, ToolExecution, error_result};

const DESCRIPTION: &str = "Edit a single file using exact text replacement. oldString must match exactly once in the file (unique) unless replaceAll is true, in which case every match is replaced. LF and CRLF line endings are equivalent for matching; replacement text preserves the file's line-ending style. All other whitespace must match exactly. If two changes affect the same block or nearby lines, merge them into one edit instead of emitting overlapping edits. Do not include large unchanged regions just to connect distant changes.";
const NAME: &str = "edit";

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
    let full_path = ctx.cwd.join(path);
    if full_path.is_dir() {
        return error_result(format!("Could not edit file: {path}. Path is not a file."));
    }
    let file_lock = match mutation_lock(&full_path) {
        Ok(lock) => lock,
        Err(error) => return error_result(format!("Could not edit file: {path}. {error}")),
    };
    let _guard = acquire_mutation_lock(&file_lock);
    if let Some(aborted) = ctx.abort_if_cancelled() {
        return aborted;
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
    let (projected_text, occurrences) = match prepare_edit(path, content, args) {
        Ok(prepared) => prepared,
        Err(message) => return error_result(message),
    };
    let patch = super::mutation::unified_diff(path, content, &projected_text);
    let summary = format!("Successfully replaced {occurrences} block(s) in {path}.");
    // 替换文本和 diff 都已备好，提交之前再做最后一次取消判定：一旦停止就不再产生
    // 文件副作用；已经提交的替换不回滚，也不伪造一个撤销动作。
    if let Some(aborted) = ctx.abort_if_cancelled() {
        return aborted;
    }
    if let Err(error) =
        singularity_core::atomic_replace_workspace_file(&full_path, projected_text.as_bytes())
    {
        return error_result(format!("Could not edit file: {path}. {error}"));
    }
    ToolExecution::text(summary).with_diff(patch)
}

fn line_ending(text: &str) -> Option<&'static str> {
    text.find('\n').map(|offset| {
        if offset > 0 && text.as_bytes()[offset - 1] == b'\r' {
            "\r\n"
        } else {
            "\n"
        }
    })
}

/// 纯文本的替换算法：完成匹配、唯一性判定和替换，返回新文本与替换块数，不做任何
/// 文件 I/O（加锁、读取、构造结果和原子提交都留在 [`execute`]）。`path` 只用于失败
/// 文案里的上下文；判定次序和文案与提取之前保持一致。
pub(super) fn prepare_edit(
    path: &str,
    content: &str,
    args: &EditArgs,
) -> Result<(String, usize), String> {
    let old_string = args.old_string.replace("\r\n", "\n");
    let new_string = args.new_string.replace("\r\n", "\n");
    if old_string.is_empty() {
        return Err(format!("oldString must not be empty in {path}."));
    }
    // read 逐行输出用的是 LF；这里只把行尾统一后再匹配，不放宽其他空白或唯一性要求。
    let normalized_content = content.replace("\r\n", "\n");
    let matches: Vec<_> = normalized_content
        .match_indices(old_string.as_str())
        .collect();
    if matches.is_empty() {
        return Err(format!(
            "Could not find the exact text in {path}. The old text must match exactly including whitespace; LF and CRLF line endings are equivalent."
        ));
    }
    let occurrences = matches.len();
    if occurrences > 1 && !args.replace_all {
        return Err(format!(
            "Found {occurrences} occurrences of the text in {path}. The text must be unique. Please provide more context to make it unique, or set replaceAll to true."
        ));
    }
    if old_string == new_string {
        // 命中已经确认过了，此时唯一可能的原因就是：把行尾归一化之后，新旧两段文本相同。
        return Err(format!(
            "No changes made to {path}. The old and new text are identical once LF and CRLF line endings are normalized."
        ));
    }
    // 把归一化后的边界映射回原文，只改命中的块，避免重写混合行尾文件里无关的行。
    let crlf_positions: Vec<_> = content
        .match_indices("\r\n")
        .enumerate()
        .map(|(removed, (offset, _))| offset - removed)
        .collect();
    let original_offset =
        |offset| offset + crlf_positions.partition_point(|position| *position < offset);
    // 文件级兜底行尾在循环外只算一次；每个命中块各自取自己的行尾，混合行尾按块保留。
    let file_ending = line_ending(content);
    let mut projected_text = String::with_capacity(content.len());
    let mut previous_end = 0;
    // 替换文本不随命中块变化，CRLF 版本在同一次调用里只生成一次。
    let crlf_new_string = new_string.replace('\n', "\r\n");
    for (offset, matched) in matches {
        let start = original_offset(offset);
        let end = original_offset(offset + matched.len());
        projected_text.push_str(&content[previous_end..start]);
        let ending = line_ending(&content[start..end])
            .or(file_ending)
            .unwrap_or("\n");
        if ending == "\r\n" {
            projected_text.push_str(&crlf_new_string);
        } else {
            projected_text.push_str(&new_string);
        }
        previous_end = end;
    }
    projected_text.push_str(&content[previous_end..]);
    Ok((projected_text, occurrences))
}
