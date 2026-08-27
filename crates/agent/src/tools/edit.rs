//! edit 工具：单文件精确文本块替换。
//!
//! - **唯一性匹配约束**：入参包含 `path`、`oldString` 与 `newString`；`oldString` 必须在目标文件中严格唯一匹配一次，
//!   若未找到匹配或匹配到多个位置，均返回明确错误并拒绝修改。
//! - **编码与换行符保持**：自动识别并保留文件原始的 UTF-8 BOM 以及换行符风格（CRLF / LF）；匹配计算统一在 LF 归一化空间中执行。
//! - **变更补丁反馈**：修改成功后返回替换统计摘要以及 Unified Diff 格式的补丁文本供模型核对。

use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::{self, Read};

use serde::Deserialize;
use serde_json::{Value, json};

use super::registry::{ExecuteContext, ToolExecution, error_result, resolve_path};
use super::truncate::{format_size, split_lines};

const MAX_EDIT_BYTES: usize = 20 * 1024 * 1024;

pub(crate) const DESCRIPTION: &str = "Edit a single file using exact text replacement. oldString must match exactly once in the file (unique). If two changes affect the same block or nearby lines, merge them into one edit instead of emitting overlapping edits. Do not include large unchanged regions just to connect distant changes.";
pub(crate) const NAME: &str = "edit";

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct EditArgs {
    path: String,
    old_string: String,
    new_string: String,
}

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
        name: NAME,
        description: DESCRIPTION,
        parameters: parameters(),
        prepare: |raw| super::registry::prepare_typed(raw, execute),
    }
}

fn execute(args: &EditArgs, ctx: ExecuteContext<'_>) -> ToolExecution {
    let path = &args.path;
    let old_string = &args.old_string;
    let new_string = &args.new_string;
    if let Some(aborted) = ctx.abort_if_cancelled() {
        return aborted;
    }
    let full_path = resolve_path(ctx.cwd, path);
    if full_path.is_dir() {
        return error_result(format!("Could not edit file: {path}. Path is not a file."));
    }
    let original = match read_bounded_file(&full_path) {
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
    let (bom, text) = strip_bom(content);
    let normalized = normalize_with_map(text);
    let old_normalized = normalize_to_lf(old_string);
    let new_normalized = normalize_to_lf(new_string);
    if old_normalized.is_empty() {
        return error_result(format!("oldString must not be empty in {path}."));
    }
    let normalized_str: &str = &normalized.text;
    let mut matches = normalized_str.match_indices(old_normalized.as_ref());
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
    let (original_start, original_end) = normalized.original_range(text, match_start, match_end);
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
    if let Err(error) = singularity_core::atomic_replace_bytes(&full_path, &final_content) {
        return error_result(format!("Could not edit file: {path}. {error}"));
    }
    let mut projected_text = String::with_capacity(
        normalized_str
            .len()
            .saturating_sub(match_end.saturating_sub(match_start))
            .saturating_add(new_normalized.len()),
    );
    projected_text.push_str(&normalized_str[..match_start]);
    projected_text.push_str(new_normalized.as_ref());
    projected_text.push_str(&normalized_str[match_end..]);
    let patch = generate_patch(
        path,
        normalized_str,
        &projected_text,
        new_normalized.as_ref(),
        match_start,
        match_end,
    );
    ToolExecution {
        content: format!("Successfully replaced 1 block(s) in {path}.\n\n{patch}"),
        is_error: false,
    }
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

struct NormalizedText<'a> {
    /// LF 归一化后的文本。无 `\r` 时直接借用原文（零拷贝、零映射）。
    text: std::borrow::Cow<'a, str>,
    /// 原文是否含 `\r`，决定边界换算是否需要局部映射。
    had_cr: bool,
}

impl<'a> NormalizedText<'a> {
    /// 把归一化命中的原始区间映射回原文件字节区间。
    ///
    /// 无 `\r` 时归一化即恒等，直接用归一化索引作为原始索引（零映射）；
    /// 有 `\r` 时仅对命中区做局部换算，不构建全量边界表。
    fn original_range(&self, raw: &str, match_start: usize, match_end: usize) -> (usize, usize) {
        if !self.had_cr {
            (match_start, match_end)
        } else {
            map_match_region(raw, match_start, match_end)
        }
    }
}

fn normalize_with_map(text: &str) -> NormalizedText<'_> {
    if !text.contains('\r') {
        return NormalizedText {
            text: std::borrow::Cow::Borrowed(text),
            had_cr: false,
        };
    }
    let mut normalized = String::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index] == b'\r' {
            index += 1;
            if bytes.get(index) == Some(&b'\n') {
                index += 1;
            }
            normalized.push('\n');
            continue;
        }
        let character = text[index..]
            .chars()
            .next()
            .expect("byte index must be a character boundary");
        index += character.len_utf8();
        normalized.push(character);
    }
    NormalizedText {
        text: std::borrow::Cow::Owned(normalized),
        had_cr: true,
    }
}

/// 计算归一化字节区间 [match_start, match_end) 对应的原始字节区间。
///
/// 按序转换原文直到命中区结束，逐一记录命中起止处的原始偏移；不做全量映射，
/// 因此仅消耗命中区及其之前前缀的扫描量。匹配索引必为字符边界。
fn map_match_region(raw: &str, match_start: usize, match_end: usize) -> (usize, usize) {
    let bytes = raw.as_bytes();
    let mut original_index = 0usize;
    let mut normalized_len = 0usize;
    let mut original_start = 0usize;
    let mut original_end = match_end;
    loop {
        if normalized_len == match_end {
            original_end = original_index;
            break;
        }
        if original_index >= bytes.len() {
            break;
        }
        if normalized_len == match_start {
            original_start = original_index;
        }
        if bytes[original_index] == b'\r' {
            original_index += 1;
            if bytes.get(original_index) == Some(&b'\n') {
                original_index += 1;
            }
            normalized_len += 1;
        } else {
            let character = raw[original_index..]
                .chars()
                .next()
                .expect("byte index must be a character boundary");
            original_index += character.len_utf8();
            normalized_len += character.len_utf8();
        }
    }
    (original_start, original_end)
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
    let removed_line_start = line_start_before(old, match_start);
    let removed_line_end = line_end_after(old, match_end.saturating_sub(1));
    let before_start = back_n_line_start(old, removed_line_start, CONTEXT_LINES);
    let after_end = forward_n_line_end(old, removed_line_end, CONTEXT_LINES);

    let replacement_len = new_block.len();
    let new_added_end = if replacement_len > 0 {
        line_end_after(
            new,
            match_start.saturating_add(replacement_len.saturating_sub(1)),
        )
    } else {
        match_start
    };

    let context_before = &old[before_start..removed_line_start];
    let removed = &old[removed_line_start..removed_line_end];
    let added = &new[removed_line_start..new_added_end];
    let context_after = &old[removed_line_end..after_end];

    let old_start = line_number_at(old, before_start).saturating_add(1);
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

/// `position` 所在行（从 0 起）的行首字节偏移。
fn line_start_before(text: &str, position: usize) -> usize {
    let prefix = &text[..position.min(text.len())];
    prefix
        .rfind('\n')
        .map_or(0, |index| index.saturating_add(1))
}

/// `position` 所在行（含其换行，若存在）的结束偏移；无换行则到文本末尾。
fn line_end_after(text: &str, position: usize) -> usize {
    let bytes = text.as_bytes();
    let mut end = position.min(bytes.len());
    while end < bytes.len() && bytes[end] != b'\n' {
        end += 1;
    }
    if end < bytes.len() { end + 1 } else { end }
}

/// 从 `position` 所在行（第 1 行计）再往前 `lines` 个整行的行首偏移（下限为文件开头）。
fn back_n_line_start(text: &str, position: usize, lines: usize) -> usize {
    if lines == 0 {
        return position;
    }
    let prefix = &text[..position.min(text.len())];
    let current_line = prefix.bytes().filter(|byte| *byte == b'\n').count() + 1;
    let target_line = current_line.saturating_sub(lines).max(1);
    if target_line == 1 {
        return 0;
    }
    let target = target_line.saturating_sub(1);
    let mut seen = 0usize;
    for (index, byte) in prefix.bytes().enumerate() {
        if byte == b'\n' {
            seen += 1;
            if seen == target {
                return index + 1;
            }
        }
    }
    0
}

/// 从 `position` 起前进 `lines` 个完整行（含各自换行），返回结束偏移。
fn forward_n_line_end(text: &str, position: usize, lines: usize) -> usize {
    let bytes = text.as_bytes();
    let mut end = position.min(bytes.len());
    for _ in 0..lines {
        if end >= bytes.len() {
            break;
        }
        while end < bytes.len() && bytes[end] != b'\n' {
            end += 1;
        }
        if end < bytes.len() {
            end += 1;
        }
    }
    end
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
