//! edit 工具：单文件精确文本块替换。
//!
//! - **唯一性匹配约束**：入参包含 `path`、`oldString` 与 `newString`；`oldString` 必须在目标文件中严格唯一匹配一次，
//!   若未找到匹配或匹配到多个位置，均返回明确错误并拒绝修改。
//! - **编码与换行符保持**：自动识别并保留文件原始的 UTF-8 BOM 以及换行符风格（CRLF / LF）；匹配计算统一在 LF 归一化空间中执行。
//! - **变更补丁反馈**：修改成功后返回替换统计摘要以及 Unified Diff 格式的补丁文本供模型核对。

use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::{self, Read};

use serde_json::{Value, json};

use super::registry::{ExecuteContext, ToolError, ToolExecution, error_result, resolve_path};
use super::truncate::format_size;

const MAX_EDIT_BYTES: usize = 20 * 1024 * 1024;

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
    let Some(queue) = ctx.mutation_queue.as_ref() else {
        return error_result("file mutation queue is unavailable");
    };
    let _mutation_lease = match queue.lock(ctx.cwd, path) {
        Ok(lease) => lease,
        Err(error) => return error_result(format!("Could not edit file: {path}. {error}")),
    };
    if full_path.is_dir() {
        return error_result(format!("Could not edit file: {path}. Path is not a file."));
    }
    let original = match read_bounded_file(&full_path) {
        Ok(content) => content,
        Err(error) => {
            return error_result(format!("Could not edit file: {path}. {error}"));
        }
    };
    if ctx.signal.is_some_and(|signal| signal.is_cancelled()) {
        return error_result("Operation aborted");
    }
    let content = match std::str::from_utf8(&original) {
        Ok(content) => content,
        Err(error) => {
            return error_result(format!("Could not edit file: {path}. {error}"));
        }
    };
    let (bom, text) = strip_bom(content);
    let normalized = normalize_with_map(text);
    let old_normalized = normalize_to_lf(old_string);
    let new_normalized = normalize_to_lf(new_string);
    if old_normalized.is_empty() {
        return error_result(format!("oldString must not be empty in {path}."));
    }
    let mut matches = normalized.text.match_indices(old_normalized.as_ref());
    let Some((match_start, match_end)) = matches
        .next()
        .map(|(start, value)| (start, start.saturating_add(value.len())))
    else {
        return error_result(format!(
            "Could not find the exact text in {path}. The old text must match exactly including all whitespace and newlines."
        ));
    };
    if matches.next().is_some() {
        let occurrences = 2usize.saturating_add(matches.count());
        return error_result(format!(
            "Found {occurrences} occurrences of the text in {path}. The text must be unique. Please provide more context to make it unique."
        ));
    }
    let original_start = normalized
        .boundaries
        .get(match_start)
        .copied()
        .unwrap_or_default();
    let original_end = normalized
        .boundaries
        .get(match_end)
        .copied()
        .unwrap_or(text.len());
    let ending = choose_replacement_ending(text, original_start, original_end);
    let replacement = restore_line_endings(new_normalized.as_ref(), ending);
    let bom_bytes = if bom { '\u{FEFF}'.len_utf8() } else { 0 };
    if original[bom_bytes..].get(original_start..original_end) == Some(replacement.as_bytes()) {
        return error_result(format!(
            "No changes made to {path}. The replacement produced identical content. This might indicate an issue with special characters or the text not existing as expected."
        ));
    }
    let raw_start = bom_bytes + original_start;
    let raw_end = bom_bytes + original_end;
    let projected_len = raw_start
        .saturating_add(replacement.len())
        .saturating_add(original.len().saturating_sub(raw_end));
    if projected_len > MAX_EDIT_BYTES {
        return error_result(format!(
            "Could not edit file: projected result exceeds {} limit.",
            format_size(MAX_EDIT_BYTES)
        ));
    }
    let mut final_content = Vec::with_capacity(projected_len);
    final_content.extend_from_slice(&original[..raw_start]);
    final_content.extend_from_slice(replacement.as_bytes());
    final_content.extend_from_slice(&original[raw_end..]);
    if let Err(error) = fs::write(&full_path, &final_content) {
        return error_result(format!("Could not edit file: {path}. {error}"));
    }
    let mut projected_text = String::with_capacity(
        text.len()
            .saturating_sub(original_end.saturating_sub(original_start))
            .saturating_add(new_normalized.len()),
    );
    projected_text.push_str(&normalized.text[..match_start]);
    projected_text.push_str(new_normalized.as_ref());
    projected_text.push_str(&normalized.text[match_end..]);
    let patch = generate_patch(path, &normalized.text, &projected_text);
    Ok(ToolExecution {
        content: format!("Successfully replaced 1 block(s) in {path}.\n\n{patch}"),
        is_error: false,
    })
}

/// 分离 UTF-8 BOM 头，返回 BOM 标志与文件文本正文。
fn strip_bom(content: &str) -> (bool, &str) {
    if let Some(text) = content.strip_prefix('\u{FEFF}') {
        (true, text)
    } else {
        (false, content)
    }
}

fn read_bounded_file(path: &std::path::Path) -> io::Result<Vec<u8>> {
    let metadata = fs::metadata(path)?;
    if metadata.len() > MAX_EDIT_BYTES as u64 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "file exceeds {} limit",
                super::truncate::format_size(MAX_EDIT_BYTES)
            ),
        ));
    }
    let mut file = File::open(path)?;
    let capacity = usize::try_from(metadata.len()).unwrap_or(MAX_EDIT_BYTES);
    let mut content = Vec::with_capacity(capacity.min(MAX_EDIT_BYTES));
    let read = file
        .by_ref()
        .take(MAX_EDIT_BYTES as u64 + 1)
        .read_to_end(&mut content)?;
    if read > MAX_EDIT_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "file exceeds {} limit",
                super::truncate::format_size(MAX_EDIT_BYTES)
            ),
        ));
    }
    Ok(content)
}

struct NormalizedText {
    text: String,
    /// Each normalized byte boundary maps to the corresponding original byte
    /// boundary. CRLF therefore maps one normalized byte to two source bytes.
    boundaries: Vec<usize>,
}

fn normalize_with_map(text: &str) -> NormalizedText {
    let mut normalized = String::with_capacity(text.len());
    let mut boundaries = Vec::with_capacity(text.len().saturating_add(1));
    boundaries.push(0);
    let bytes = text.as_bytes();
    let mut index = 0usize;
    while index < bytes.len() {
        let original_start = index;
        if bytes[index] == b'\r' {
            index += 1;
            if bytes.get(index) == Some(&b'\n') {
                index += 1;
            }
            normalized.push('\n');
            boundaries.push(index);
            continue;
        }
        let character = text[index..]
            .chars()
            .next()
            .expect("byte index must be a character boundary");
        index += character.len_utf8();
        normalized.push(character);
        for offset in 1..=character.len_utf8() {
            boundaries.push(original_start + offset);
        }
    }
    NormalizedText {
        text: normalized,
        boundaries,
    }
}

/// 将文本中的换行符统一归一化为 LF，避免无意义的副本。
fn normalize_to_lf(text: &str) -> std::borrow::Cow<'_, str> {
    if !text.contains('\r') {
        std::borrow::Cow::Borrowed(text)
    } else {
        std::borrow::Cow::Owned(text.replace("\r\n", "\n").replace('\r', "\n"))
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum LineEnding {
    #[default]
    Lf,
    CrLf,
    Cr,
}

#[derive(Debug, Default)]
struct LineEndingCounts {
    lf: usize,
    crlf: usize,
    cr: usize,
}

impl LineEndingCounts {
    fn add_assign(&mut self, other: Self) {
        self.lf = self.lf.saturating_add(other.lf);
        self.crlf = self.crlf.saturating_add(other.crlf);
        self.cr = self.cr.saturating_add(other.cr);
    }

    fn unique(&self) -> Option<LineEnding> {
        let values = [
            (LineEnding::Lf, self.lf),
            (LineEnding::CrLf, self.crlf),
            (LineEnding::Cr, self.cr),
        ];
        let max = values.iter().map(|(_, count)| *count).max().unwrap_or(0);
        (max > 0 && values.iter().filter(|(_, count)| *count == max).count() == 1)
            .then(|| values.iter().find(|(_, count)| *count == max).unwrap().0)
    }
}

fn count_line_endings(text: &str) -> LineEndingCounts {
    let bytes = text.as_bytes();
    let mut counts = LineEndingCounts::default();
    let mut index = 0usize;
    while index < bytes.len() {
        match bytes[index] {
            b'\r' if bytes.get(index + 1) == Some(&b'\n') => {
                counts.crlf = counts.crlf.saturating_add(1);
                index += 2;
            }
            b'\r' => {
                counts.cr = counts.cr.saturating_add(1);
                index += 1;
            }
            b'\n' => {
                counts.lf = counts.lf.saturating_add(1);
                index += 1;
            }
            _ => index += 1,
        }
    }
    counts
}

fn choose_replacement_ending(text: &str, start: usize, end: usize) -> LineEnding {
    if let Some(ending) = count_line_endings(&text[start..end]).unique() {
        return ending;
    }
    let left = text[..start]
        .rfind(['\r', '\n'])
        .map(|index| index.saturating_add(1))
        .unwrap_or(0);
    let right = text[end..]
        .find(['\r', '\n'])
        .map(|index| end.saturating_add(index).saturating_add(1))
        .unwrap_or(text.len());
    let mut adjacent = count_line_endings(&text[left..start]);
    adjacent.add_assign(count_line_endings(&text[end..right]));
    if let Some(ending) = adjacent.unique() {
        return ending;
    }
    count_line_endings(text).unique().unwrap_or_default()
}

fn restore_line_endings(text: &str, ending: LineEnding) -> String {
    match ending {
        LineEnding::Lf => text.to_string(),
        LineEnding::CrLf => text.replace('\n', "\r\n"),
        LineEnding::Cr => text.replace('\n', "\r"),
    }
}

/// 生成单文本块修改前后的 Unified Diff 补丁展示文本（包含前后各 4 行上下文）。
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

    #[test]
    fn exact_twenty_mib_input_and_result_are_accepted() {
        let dir = tempdir().expect("temp dir");
        let path = dir.path().join("limit.txt");
        let content = format!("{}END", "a".repeat(MAX_EDIT_BYTES - 3));
        assert_eq!(content.len(), MAX_EDIT_BYTES);
        fs::write(&path, &content).expect("fixture");
        let result = ToolRegistry::new()
            .execute(
                "edit",
                context(
                    json!({ "path": "limit.txt", "oldString": "END", "newString": "XYZ" }),
                    dir.path(),
                ),
            )
            .expect("execute");
        assert!(!result.is_error, "content: {}", result.content);
        assert_eq!(
            fs::metadata(&path).expect("metadata").len(),
            MAX_EDIT_BYTES as u64
        );
        assert!(
            fs::read_to_string(&path)
                .expect("read back")
                .ends_with("XYZ")
        );
    }

    #[test]
    fn input_over_twenty_mib_is_rejected_before_edit() {
        let dir = tempdir().expect("temp dir");
        let path = dir.path().join("over-limit.txt");
        let content = "a".repeat(MAX_EDIT_BYTES + 1);
        fs::write(&path, &content).expect("fixture");
        let result = ToolRegistry::new()
            .execute(
                "edit",
                context(
                    json!({ "path": "over-limit.txt", "oldString": "a", "newString": "b" }),
                    dir.path(),
                ),
            )
            .expect("execute");
        assert!(result.is_error, "content: {}", result.content);
        assert!(
            result.content.contains("exceeds 20.0MB limit"),
            "content: {}",
            result.content
        );
        assert_eq!(
            fs::metadata(&path).expect("metadata").len(),
            (MAX_EDIT_BYTES + 1) as u64
        );
    }

    #[test]
    fn projected_result_over_twenty_mib_is_rejected_before_write() {
        let dir = tempdir().expect("temp dir");
        let path = dir.path().join("projected-limit.txt");
        let content = format!("{}END", "a".repeat(MAX_EDIT_BYTES - 3));
        fs::write(&path, &content).expect("fixture");
        let result = ToolRegistry::new()
            .execute(
                "edit",
                context(
                    json!({ "path": "projected-limit.txt", "oldString": "END", "newString": "ENDS" }),
                    dir.path(),
                ),
            )
            .expect("execute");
        assert!(result.is_error, "content: {}", result.content);
        assert!(
            result.content.contains("projected result exceeds"),
            "content: {}",
            result.content
        );
        assert_eq!(fs::read_to_string(&path).expect("read back"), content);
    }

    #[test]
    fn mixed_newlines_preserve_untouched_bytes_and_choose_adjacent_style() {
        let dir = tempdir().expect("temp dir");
        let path = dir.path().join("mixed.txt");
        let original = b"\xEF\xBB\xBFa\r\nb\nc\r\nd";
        fs::write(&path, original).expect("fixture");
        let result = ToolRegistry::new()
            .execute(
                "edit",
                context(
                    json!({ "path": "mixed.txt", "oldString": "b", "newString": "B\nX" }),
                    dir.path(),
                ),
            )
            .expect("execute");
        assert!(!result.is_error, "content: {}", result.content);
        assert_eq!(
            fs::read(&path).expect("read back"),
            b"\xEF\xBB\xBFa\r\nB\nX\nc\r\nd"
        );
    }

    #[test]
    fn replacement_region_style_wins_over_adjacent_style() {
        let dir = tempdir().expect("temp dir");
        let path = dir.path().join("region-ending.txt");
        fs::write(&path, b"a\r\nb\r\nc\n").expect("fixture");
        let result = ToolRegistry::new()
            .execute(
                "edit",
                context(
                    json!({ "path": "region-ending.txt", "oldString": "b\r\n", "newString": "B\nX\n" }),
                    dir.path(),
                ),
            )
            .expect("execute");
        assert!(!result.is_error, "content: {}", result.content);
        assert_eq!(fs::read(&path).expect("read back"), b"a\r\nB\r\nX\r\nc\n");
    }

    #[test]
    fn tied_region_and_file_newlines_fall_back_to_lf() {
        let dir = tempdir().expect("temp dir");
        let path = dir.path().join("tie-ending.txt");
        fs::write(&path, b"a\r\nb\n").expect("fixture");
        let result = ToolRegistry::new()
            .execute(
                "edit",
                context(
                    json!({ "path": "tie-ending.txt", "oldString": "a\r\nb\n", "newString": "x\ny" }),
                    dir.path(),
                ),
            )
            .expect("execute");
        assert!(!result.is_error, "content: {}", result.content);
        assert_eq!(fs::read(&path).expect("read back"), b"x\ny");
    }
}
