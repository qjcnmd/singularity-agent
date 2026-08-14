//! edit 工具：单文件精确文本替换（对齐 Pi edit 语义）。
//!
//! - 参数 path/oldString/newString（本规格定稿的参数名）；oldString 必须在文件中恰好
//!   匹配一次，否则 is_error（不匹配 / 多匹配都拒绝）。
//! - 保留 BOM 与原始换行风格（CRLF/LF）；匹配在 LF 归一化空间进行。
//! - 返回替换摘要 + unified patch 文本。

use std::fmt::Write as _;
use std::fs;

use serde_json::{Value, json};

use super::registry::{ExecuteContext, ToolError, ToolExecution, error_result, resolve_path};

pub(crate) const DESCRIPTION: &str = "Edit a single file using exact text replacement. oldString must match exactly once in the file (unique). If two changes affect the same block or nearby lines, merge them into one edit instead of emitting overlapping edits. Do not include large unchanged regions just to connect distant changes.";

pub(crate) fn parameters() -> Value {
    json!({
        "type": "object",
        "properties": {
            "path": { "type": "string", "description": "Path to the file to edit (relative or absolute)" },
            "oldString": { "type": "string", "description": "Exact text for one targeted replacement. It must be unique in the original file." },
            "newString": { "type": "string", "description": "Replacement text for this targeted edit." },
        },
        "required": ["path", "oldString", "newString"],
        "additionalProperties": false,
    })
}

pub(crate) fn spec() -> super::registry::ToolSpec {
    super::registry::ToolSpec {
        name: "edit",
        description: DESCRIPTION,
        parameters: parameters(),
        execute,
    }
}

pub(crate) fn execute(ctx: ExecuteContext<'_>) -> Result<ToolExecution, ToolError> {
    let Some(path) = ctx.args.get("path").and_then(Value::as_str) else {
        return error_result("missing required parameter \"path\"");
    };
    let Some(old_string) = ctx.args.get("oldString").and_then(Value::as_str) else {
        return error_result("missing required parameter \"oldString\"");
    };
    let Some(new_string) = ctx.args.get("newString").and_then(Value::as_str) else {
        return error_result("missing required parameter \"newString\"");
    };
    if ctx.signal.is_some_and(|signal| signal.is_cancelled()) {
        return error_result("Operation aborted");
    }
    let full_path = resolve_path(ctx.cwd, path);
    let content = match fs::read_to_string(&full_path) {
        Ok(content) => content,
        Err(error) => {
            return error_result(format!("Could not edit file: {path}. {error}"));
        }
    };
    if full_path.is_dir() {
        return error_result(format!("Could not edit file: {path}. Path is not a file."));
    }
    if ctx.signal.is_some_and(|signal| signal.is_cancelled()) {
        return error_result("Operation aborted");
    }
    let (bom, text) = strip_bom(&content);
    let original_ending = detect_line_ending(&text);
    let normalized = normalize_to_lf(&text);
    let old_normalized = normalize_to_lf(old_string);
    let new_normalized = normalize_to_lf(new_string);
    if old_normalized.is_empty() {
        return error_result(format!("oldString must not be empty in {path}."));
    }
    let occurrences = normalized.matches(old_normalized.as_str()).count();
    if occurrences == 0 {
        return error_result(format!(
            "Could not find the exact text in {path}. The old text must match exactly including all whitespace and newlines."
        ));
    }
    if occurrences > 1 {
        return error_result(format!(
            "Found {occurrences} occurrences of the text in {path}. The text must be unique. Please provide more context to make it unique."
        ));
    }
    let new_content = normalized.replacen(old_normalized.as_str(), new_normalized.as_str(), 1);
    if new_content == normalized {
        return error_result(format!(
            "No changes made to {path}. The replacement produced identical content. This might indicate an issue with special characters or the text not existing as expected."
        ));
    }
    let final_content = format!(
        "{bom}{}",
        restore_line_endings(&new_content, original_ending)
    );
    if let Err(error) = fs::write(&full_path, final_content) {
        return error_result(format!("Could not edit file: {path}. {error}"));
    }
    let patch = generate_patch(path, &normalized, &new_content);
    Ok(ToolExecution {
        content: format!("Successfully replaced 1 block(s) in {path}.\n\n{patch}"),
        is_error: false,
    })
}

/// 去掉 UTF-8 BOM，返回 BOM 与正文（对齐 Pi `stripBom`）。
fn strip_bom(content: &str) -> (String, String) {
    if let Some(text) = content.strip_prefix('\u{FEFF}') {
        ("\u{FEFF}".to_string(), text.to_string())
    } else {
        (String::new(), content.to_string())
    }
}

/// 检测原始换行风格：先出现的换行类型为准（对齐 Pi `detectLineEnding`）。
fn detect_line_ending(content: &str) -> &'static str {
    let crlf = content.find("\r\n");
    let lf = content.find('\n');
    match (crlf, lf) {
        (Some(crlf), Some(lf)) if crlf < lf => "\r\n",
        _ => "\n",
    }
}

/// 统一为 LF（对齐 Pi `normalizeToLF`）。
fn normalize_to_lf(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

/// 恢复原始换行风格（对齐 Pi `restoreLineEndings`）。
fn restore_line_endings(text: &str, ending: &str) -> String {
    if ending == "\r\n" {
        text.replace('\n', "\r\n")
    } else {
        text.to_string()
    }
}

/// 单块替换的 unified patch 文本（context 4 行），供模型阅读（对齐 Pi
/// `generateUnifiedPatch` 的展示目的；不处理 "\ No newline at end of file" 标记）。
fn generate_patch(path: &str, old_content: &str, new_content: &str) -> String {
    const CONTEXT_LINES: usize = 4;
    let old_lines: Vec<&str> = old_content.split('\n').collect();
    let new_lines: Vec<&str> = new_content.split('\n').collect();
    let mut first = 0;
    while first < old_lines.len() && first < new_lines.len() && old_lines[first] == new_lines[first]
    {
        first += 1;
    }
    let mut old_end = old_lines.len();
    let mut new_end = new_lines.len();
    while old_end > first && new_end > first && old_lines[old_end - 1] == new_lines[new_end - 1] {
        old_end -= 1;
        new_end -= 1;
    }
    let before = first.min(CONTEXT_LINES);
    let after_old = (old_lines.len() - old_end).min(CONTEXT_LINES);
    let after_new = (new_lines.len() - new_end).min(CONTEXT_LINES);
    let old_count = (old_end - first) + before + after_old;
    let new_count = (new_end - first) + before + after_new;

    let mut patch = String::new();
    let _ = writeln!(patch, "--- {path}");
    let _ = writeln!(patch, "+++ {path}");
    let _ = writeln!(
        patch,
        "@@ -{} +{} @@",
        range(first - before + 1, old_count),
        range(first - before + 1, new_count)
    );
    for line in &old_lines[first - before..first] {
        let _ = writeln!(patch, " {line}");
    }
    for line in &old_lines[first..old_end] {
        let _ = writeln!(patch, "-{line}");
    }
    for line in &new_lines[first..new_end] {
        let _ = writeln!(patch, "+{line}");
    }
    for line in &old_lines[old_end..old_end + after_old] {
        let _ = writeln!(patch, " {line}");
    }
    patch
}

/// unified patch hunk 头中的行号范围（count 为 1 时省略 ",1"）。
fn range(start: usize, count: usize) -> String {
    if count == 1 {
        start.to_string()
    } else {
        format!("{start},{count}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::registry::ToolRegistry;
    use crate::tools::test_support::context;
    use serde_json::json;
    use tempfile::tempdir;

    #[test]
    fn unique_match_replaces_and_reports_patch() {
        let dir = tempdir().expect("temp dir");
        fs::write(dir.path().join("sample.txt"), "aaa\nbbb\nccc").expect("fixture");
        let result = ToolRegistry::new()
            .execute(
                "edit",
                context(
                    json!({ "path": "sample.txt", "oldString": "bbb", "newString": "B2" }),
                    dir.path(),
                ),
            )
            .expect("execute");
        assert!(!result.is_error, "content: {}", result.content);
        assert!(result.content.contains("Successfully replaced 1 block(s)"));
        assert!(result.content.contains("-bbb"), "patch shows removed line");
        assert!(result.content.contains("+B2"), "patch shows added line");
        assert_eq!(
            fs::read_to_string(dir.path().join("sample.txt")).expect("read back"),
            "aaa\nB2\nccc"
        );
    }

    #[test]
    fn multiple_matches_are_rejected() {
        let dir = tempdir().expect("temp dir");
        fs::write(dir.path().join("sample.txt"), "aaa\nbbb\naaa").expect("fixture");
        let result = ToolRegistry::new()
            .execute(
                "edit",
                context(
                    json!({ "path": "sample.txt", "oldString": "aaa", "newString": "x" }),
                    dir.path(),
                ),
            )
            .expect("execute");
        assert!(result.is_error);
        assert!(result.content.contains("Found 2 occurrences"));
        assert_eq!(
            fs::read_to_string(dir.path().join("sample.txt")).expect("read back"),
            "aaa\nbbb\naaa",
            "file must remain unchanged"
        );
    }

    #[test]
    fn missing_file_is_error() {
        let dir = tempdir().expect("temp dir");
        let result = ToolRegistry::new()
            .execute(
                "edit",
                context(
                    json!({ "path": "missing.txt", "oldString": "a", "newString": "b" }),
                    dir.path(),
                ),
            )
            .expect("execute");
        assert!(result.is_error);
        assert!(result.content.contains("Could not edit file"));
    }

    #[test]
    fn crlf_line_endings_are_preserved() {
        let dir = tempdir().expect("temp dir");
        fs::write(dir.path().join("sample.txt"), "a\r\nb\r\nc").expect("fixture");
        let result = ToolRegistry::new()
            .execute(
                "edit",
                context(
                    json!({ "path": "sample.txt", "oldString": "b", "newString": "B" }),
                    dir.path(),
                ),
            )
            .expect("execute");
        assert!(!result.is_error, "content: {}", result.content);
        assert_eq!(
            fs::read_to_string(dir.path().join("sample.txt")).expect("read back"),
            "a\r\nB\r\nc"
        );
    }
}
