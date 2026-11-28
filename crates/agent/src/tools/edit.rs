use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Deserialize;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use super::file_ops::{FileReadTracker, format_diff_result, read_opened_text_file};
use super::registry::{ABORTED_MESSAGE, EDIT_TOOL_NAME, ToolExecution, error_result};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EditArgs {
    path: String,
    edits: Vec<Replacement>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Replacement {
    old_text: String,
    new_text: String,
}

pub(crate) fn spec() -> model::ToolDefinition {
    model::ToolDefinition {
        name: EDIT_TOOL_NAME.into(),
        description: "Edit an existing UTF-8 text file using one or more replacements. Exact match runs first; if it misses, matching falls back to ignoring trailing whitespace, common Unicode quote/dash/space variants, or line indentation differences. Each old_text must be nonempty and match uniquely in the original file; edits must not overlap. All replacements are checked before writing once. Read the relevant content first. LF and CRLF match equivalently. Use an empty new_text to delete text. Keep enough context to identify each location. Combine independent changes to one file in a single call; do not issue dependent edits concurrently.".into(),
        parameters_schema: json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Existing text file, relative to the working directory or absolute" },
                "edits": {
                    "type": "array", "minItems": 1,
                    "items": {
                        "type": "object",
                        "properties": { "old_text": { "type": "string", "minLength": 1 }, "new_text": { "type": "string" } },
                        "required": ["old_text", "new_text"], "additionalProperties": false
                    }
                }
            },
            "required": ["path", "edits"],
            "additionalProperties": false
        }),
    }
}

pub(crate) async fn execute(
    args: EditArgs,
    cwd: PathBuf,
    output_directory: PathBuf,
    file_reads: Arc<FileReadTracker>,
    signal: CancellationToken,
) -> ToolExecution {
    if signal.is_cancelled() {
        return error_result(ABORTED_MESSAGE);
    }
    if args.edits.is_empty() {
        return error_result("edits must contain at least one replacement");
    }
    let target = cwd.join(&args.path);
    tokio::task::spawn_blocking(move || {
        execute_locked(&args, &target, &output_directory, &file_reads, &signal).unwrap_or_else(error_result)
    })
    .await
    .expect("tool worker completes while the runtime is running")
}

fn execute_locked(
    args: &EditArgs,
    raw_target: &Path,
    output_directory: &Path,
    file_reads: &FileReadTracker,
    signal: &CancellationToken,
) -> Result<ToolExecution, String> {
    if signal.is_cancelled() {
        return Err(ABORTED_MESSAGE.into());
    }
    let target = raw_target
        .canonicalize()
        .map_err(|error| format!("Could not edit {}: {error}", args.path))?;
    let key = app_core::workspace_key(&target);
    let file = File::open(&target).map_err(|error| format!("Could not read {}: {error}", args.path))?;
    let metadata = file.metadata().map_err(|error| format!("Could not inspect {}: {error}", args.path))?;
    file_reads.verify_unchanged(&key, &metadata, &args.path, "edit")?;
    let content = read_opened_text_file(file, &args.path)?;
    let (view, updated) = apply_edits(&content, &args.path, &args.edits)?;
    if signal.is_cancelled() {
        return Err(ABORTED_MESSAGE.into());
    }
    app_core::write_bytes_atomic(&target, updated.as_bytes())
        .map_err(|error| format!("Could not write {}: {error}", args.path))?;
    if let Ok(updated_meta) = std::fs::metadata(&target) {
        file_reads.record_metadata(key, &updated_meta);
    }
    Ok(format_diff_result("Edited", &args.path, &view, &updated, output_directory))
}

fn apply_edits(content: &str, display_path: &str, edits: &[Replacement]) -> Result<(String, String), String> {
    let source = app_core::strip_utf8_bom(content);
    let view = source.replace("\r\n", "\n");
    let removed: Vec<_> = source
        .match_indices("\r\n")
        .enumerate()
        .map(|(index, (offset, _))| offset - index)
        .collect();
    let source_offset = |offset| offset + removed.partition_point(|removed| *removed < offset);
    let mut matches = Vec::with_capacity(edits.len());
    let mut cache = MatchCache::default();
    for (index, edit) in edits.iter().enumerate() {
        if edit.new_text.contains('\0') {
            return Err(format!("edits[{index}].new_text must not contain NUL bytes"));
        }
        let old = edit.old_text.replace("\r\n", "\n");
        if old.is_empty() {
            return Err(format!("edits[{index}].old_text must not be empty"));
        }
        let plan = find_unique_match(&view, &old, index, display_path, &mut cache)?;
        matches.push((source_offset(plan.start), source_offset(plan.end), index, plan.reindent));
    }
    matches.sort_unstable_by_key(|(start, end, index, _)| (*start, *end, *index));
    for pair in matches.windows(2) {
        if pair[0].1 > pair[1].0 {
            return Err(format!(
                "edits[{}] and edits[{}] overlap. Merge them into one replacement.",
                pair[0].2, pair[1].2
            ));
        }
    }
    let mut updated = String::with_capacity(content.len());
    if content.starts_with('\u{feff}') {
        updated.push('\u{feff}');
    }
    let mut cursor = 0;
    for (start, end, index, reindent) in matches {
        updated.push_str(&source[cursor..start]);
        let region = &source[start..end];
        let raw_replacement = edits[index].new_text.replace("\r\n", "\n");
        let replacement = match &reindent {
            Some((old_indent, file_indent)) => {
                reindent_replacement(&raw_replacement, old_indent, file_indent)
            }
            None => raw_replacement,
        };
        let ending = if region.contains("\r\n") || (!region.contains('\n') && source.contains("\r\n")) {
            "\r\n"
        } else {
            "\n"
        };
        updated.push_str(&replacement.replace('\n', ending));
        cursor = end;
    }
    updated.push_str(&source[cursor..]);
    if updated == content {
        return Err("No changes to apply: the replacements leave the file unchanged".into());
    }
    Ok((view, updated))
}

struct MatchPlan {
    start: usize,
    end: usize,
    reindent: Option<(String, String)>,
}

struct LineSpan<'a> {
    text: &'a str,
    start: usize,
    end: usize,
    has_nl: bool,
}

fn ambiguous_error(index: usize, path: &str, mode_label: Option<&str>) -> String {
    let mode = mode_label.map_or(String::new(), |label| format!(" ({label})"));
    format!(
        "edits[{index}].old_text matches more than once in {path}{mode}. Include more surrounding context."
    )
}

struct NormalizedView {
    norm: String,
    byte_start: Vec<usize>,
    byte_end: Vec<usize>,
}

#[derive(Default)]
struct MatchCache<'a> {
    lines: Option<Vec<LineSpan<'a>>>,
    norm_ws: Option<NormalizedView>,
    norm_unicode: Option<NormalizedView>,
}

impl MatchCache<'_> {
    fn normalized(&mut self, view: &str, unicode: bool) -> &NormalizedView {
        let slot = if unicode {
            &mut self.norm_unicode
        } else {
            &mut self.norm_ws
        };
        slot.get_or_insert_with(|| {
            let (norm, byte_start, byte_end) = normalize_with_map(view, unicode);
            NormalizedView { norm, byte_start, byte_end }
        })
    }
}

fn find_unique_match<'a>(
    view: &'a str,
    old: &str,
    index: usize,
    path: &str,
    cache: &mut MatchCache<'a>,
) -> Result<MatchPlan, String> {
    if let Some(start) = view.find(old) {
        let next = start + old.chars().next().expect("old text is nonempty").len_utf8();
        if view[next..].contains(old) {
            return Err(ambiguous_error(index, path, None));
        }
        return Ok(MatchPlan {
            start,
            end: start + old.len(),
            reindent: None,
        });
    }
    if !old.chars().any(|ch| !ch.is_whitespace()) {
        return Err(not_found_error(view, old, index, path, &mut cache.lines));
    }
    for (unicode, label) in [
        (false, "ignoring trailing whitespace"),
        (true, "normalizing Unicode punctuation/spaces"),
    ] {
        let normalized = cache.normalized(view, unicode);
        if let Some(plan) = find_normalized_unique(view, normalized, old, unicode, label, index, path)? {
            return Ok(plan);
        }
    }
    let lines = cache.lines.get_or_insert_with(|| split_line_spans(view));
    if let Some(plan) = find_line_block_unique(lines, old, "ignoring outer indentation", index, path)? {
        return Ok(plan);
    }
    Err(not_found_error(view, old, index, path, &mut cache.lines))
}

fn find_normalized_unique(
    view: &str,
    normalized: &NormalizedView,
    old: &str,
    unicode: bool,
    mode_label: &str,
    index: usize,
    path: &str,
) -> Result<Option<MatchPlan>, String> {
    let (norm_old, _, _) = normalize_with_map(old, unicode);
    if !norm_old.chars().any(|ch| !ch.is_whitespace()) {
        return Ok(None);
    }
    let last_line_trimmed = !old.ends_with('\n')
        && old
            .rsplit('\n')
            .next()
            .and_then(|segment| segment.chars().next_back())
            .is_some_and(|ch| matches!(normalize_char(ch, unicode), ' ' | '\t'));
    let (norm_view, byte_start, byte_end) =
        (normalized.norm.as_str(), &normalized.byte_start, &normalized.byte_end);
    let mut matched: Option<MatchPlan> = None;
    let mut offset = 0usize;
    while let Some(rel) = norm_view[offset..].find(&norm_old) {
        let ns = offset + rel;
        let ne = ns + norm_old.len();
        offset = ns + norm_view[ns..].chars().next().expect("nonempty match").len_utf8();
        let at_eol = ne == norm_view.len() || norm_view.as_bytes()[ne] == b'\n';
        if last_line_trimmed && !at_eol {
            continue;
        }
        if matched.is_some() {
            return Err(ambiguous_error(index, path, Some(mode_label)));
        }
        let orig_end = if ne == norm_view.len() {
            view.len()
        } else if norm_view.as_bytes()[ne] == b'\n' {
            byte_start[ne]
        } else {
            byte_end[ne]
        };
        matched = Some(MatchPlan {
            start: byte_start[ns],
            end: orig_end,
            reindent: None,
        });
    }
    Ok(matched)
}

fn normalize_char(ch: char, unicode: bool) -> char {
    if !unicode {
        return ch;
    }
    match ch {
        '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' | '\u{2032}' => '\'',
        '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' | '\u{2033}' => '"',
        '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2015}' | '\u{2212}' => '-',
        '\u{00A0}' | '\u{2002}'..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}' => ' ',
        _ => ch,
    }
}

fn normalize_with_map(text: &str, unicode: bool) -> (String, Vec<usize>, Vec<usize>) {
    let mut norm = String::with_capacity(text.len());
    let mut byte_start = Vec::with_capacity(text.len() + 1);
    let mut byte_end = Vec::with_capacity(text.len() + 1);
    byte_end.push(0);
    let mut line_start = 0usize;
    for segment in text.split('\n') {
        let mut trimmed_end = segment.len();
        for (rel, ch) in segment.char_indices().rev() {
            if matches!(normalize_char(ch, unicode), ' ' | '\t') {
                trimmed_end = rel;
            } else {
                break;
            }
        }
        for (rel, ch) in segment[..trimmed_end].char_indices() {
            let (orig_s, orig_e, prev) = (line_start + rel, line_start + rel + ch.len_utf8(), norm.len());
            norm.push(normalize_char(ch, unicode));
            for _ in prev..norm.len() {
                byte_start.push(orig_s);
                byte_end.push(orig_e);
            }
        }
        let nl_pos = line_start + segment.len();
        if nl_pos < text.len() {
            norm.push('\n');
            byte_start.push(nl_pos);
            byte_end.push(nl_pos + 1);
            line_start = nl_pos + 1;
        }
    }
    byte_start.push(text.len());
    (norm, byte_start, byte_end)
}

fn split_line_spans(view: &str) -> Vec<LineSpan<'_>> {
    let (mut spans, mut start) = (Vec::new(), 0usize);
    for (idx, _) in view.match_indices('\n') {
        spans.push(LineSpan {
            text: &view[start..idx],
            start,
            end: idx,
            has_nl: true,
        });
        start = idx + 1;
    }
    if start < view.len() || spans.is_empty() {
        spans.push(LineSpan {
            text: &view[start..],
            start,
            end: view.len(),
            has_nl: false,
        });
    }
    spans
}

fn normalize_line(line: &str, base_indent: &str) -> String {
    if line.trim().is_empty() {
        return String::new();
    }
    let slice = line.strip_prefix(base_indent).unwrap_or(line);
    let mapped: String = slice.chars().map(|ch| normalize_char(ch, true)).collect();
    mapped.trim_end_matches([' ', '\t']).to_string()
}

fn leading_indent(line: &str) -> &str {
    &line[..line.bytes().take_while(|b| matches!(b, b' ' | b'\t')).count()]
}

fn common_indent<'a>(lines: impl Iterator<Item = &'a str>) -> &'a str {
    let mut common: Option<&str> = None;
    for line in lines.filter(|line| !line.trim().is_empty()) {
        let indent = leading_indent(line);
        common = Some(match common {
            None => indent,
            Some(prev) => &prev[..prev.bytes().zip(indent.bytes()).take_while(|(a, b)| a == b).count()],
        });
    }
    common.unwrap_or("")
}

fn find_line_block_unique(
    lines: &[LineSpan<'_>],
    old: &str,
    mode_label: &str,
    index: usize,
    path: &str,
) -> Result<Option<MatchPlan>, String> {
    let (old_body, old_has_nl) = old.strip_suffix('\n').map_or((old, false), |body| (body, true));
    let old_lines: Vec<&str> = old_body.split('\n').collect();
    if old_lines.is_empty() || old_lines.len() > lines.len() {
        return Ok(None);
    }
    let old_indent = common_indent(old_lines.iter().copied());
    let old_norm: Vec<String> = old_lines.iter().map(|line| normalize_line(line, old_indent)).collect();
    let mut matched: Option<MatchPlan> = None;
    for window in lines.windows(old_lines.len()) {
        let last = window.last().expect("nonempty window");
        if old_has_nl && !last.has_nl {
            continue;
        }
        let file_indent = common_indent(window.iter().map(|span| span.text));
        if !window
            .iter()
            .zip(&old_norm)
            .all(|(span, expected)| normalize_line(span.text, file_indent) == *expected)
        {
            continue;
        }
        if matched.is_some() {
            return Err(ambiguous_error(index, path, Some(mode_label)));
        }
        let start = window.first().expect("nonempty window").start;
        let end = last.end + usize::from(old_has_nl);
        let reindent = (file_indent != old_indent).then(|| (old_indent.to_string(), file_indent.to_string()));
        matched = Some(MatchPlan { start, end, reindent });
    }
    Ok(matched)
}

fn reindent_replacement(replacement: &str, old_indent: &str, file_indent: &str) -> String {
    if replacement.is_empty()
        || old_indent == file_indent
        || !replacement
            .split('\n')
            .filter(|line| !line.trim().is_empty())
            .all(|line| line.starts_with(old_indent))
    {
        return replacement.to_string();
    }
    replacement
        .split('\n')
        .map(|line| {
            if line.trim().is_empty() {
                line.to_string()
            } else {
                format!("{file_indent}{}", line.strip_prefix(old_indent).unwrap_or(line))
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn not_found_error<'a>(
    view: &'a str,
    old: &str,
    index: usize,
    path: &str,
    cached_lines: &mut Option<Vec<LineSpan<'a>>>,
) -> String {
    if let Some(anchor) = old.split('\n').map(str::trim).find(|line| !line.is_empty()) {
        let lines = cached_lines.get_or_insert_with(|| split_line_spans(view));
        let hits: Vec<String> = lines
            .iter()
            .enumerate()
            .filter(|(_, span)| span.text.trim() == anchor)
            .take(3)
            .map(|(idx, _)| (idx + 1).to_string())
            .collect();
        if !hits.is_empty() {
            return format!(
                "edits[{index}].old_text was not found in {path} (anchor line found near line {}). Read the current text and retry.",
                hits.join(", ")
            );
        }
    }
    format!("edits[{index}].old_text was not found in {path}. Read the current text and retry.")
}
