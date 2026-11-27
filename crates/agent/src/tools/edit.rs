use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, Weak};

const MAX_EDIT_FILE_BYTES: u64 = 20 * 1024 * 1024;

use serde::Deserialize;
use serde_json::json;
use similar::{ChangeTag, TextDiff};
use tokio_util::sync::CancellationToken;

use super::registry::{
    ABORTED_MESSAGE, COMMAND_MAX_BYTES, COMMAND_MAX_LINES, ToolDisplay, ToolExecution, error_result,
};

static FILE_EDITS: LazyLock<Mutex<HashMap<String, Weak<tokio::sync::Mutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub(super) fn file_lock_key(path: &Path) -> std::io::Result<String> {
    let mut missing = Vec::new();
    let mut skip_parents = 0usize;
    let mut cursor = path;
    loop {
        if skip_parents == 0
            && let Ok(mut full) = cursor.canonicalize()
        {
            for component in missing.into_iter().rev() {
                full.push(component);
            }
            return Ok(app_core::workspace_key(&full));
        }
        match cursor.components().next_back() {
            Some(Component::CurDir) => {}
            Some(Component::ParentDir) => skip_parents += 1,
            Some(Component::Normal(name)) => {
                if let Some(remaining) = skip_parents.checked_sub(1) {
                    skip_parents = remaining;
                } else {
                    missing.push(name.to_os_string());
                }
            }
            _ => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("cannot resolve parent directory for {}", path.display()),
                ));
            }
        }
        let parent = cursor.parent().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("cannot resolve parent directory for {}", path.display()),
            )
        })?;
        cursor = if parent.as_os_str().is_empty() {
            Path::new(".")
        } else {
            parent
        };
    }
}

pub(super) async fn with_file_lock<F>(key: String, signal: CancellationToken, work: F) -> ToolExecution
where
    F: FnOnce(&CancellationToken) -> Result<ToolExecution, String> + Send + 'static,
{
    let lock = {
        let mut files = FILE_EDITS.lock().expect("file edit registry lock poisoned");
        let lock = files.get(&key).and_then(Weak::upgrade).unwrap_or_default();
        files.insert(key, Arc::downgrade(&lock));
        lock
    };
    let result = tokio::select! {
        biased;
        () = signal.cancelled() => error_result(ABORTED_MESSAGE),
        guard = Arc::clone(&lock).lock_owned() => {
            tokio::task::spawn_blocking(move || {
                let _guard = guard;
                work(&signal).unwrap_or_else(error_result)
            })
            .await
            .expect("tool worker completes while the runtime is running")
        }
    };
    drop(lock);
    FILE_EDITS
        .lock()
        .expect("file edit registry lock poisoned")
        .retain(|_, lock| lock.strong_count() > 0);
    result
}

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

pub(crate) fn spec() -> app_core::ToolDefinition {
    app_core::ToolDefinition {
        name: "edit".into(),
        description: "Edit an existing UTF-8 text file using one or more replacements. Exact match runs first; if it misses, matching falls back to ignoring trailing whitespace, common Unicode quote/dash/space variants, or line indentation differences. Each old_text must be nonempty and match uniquely in the original file; edits must not overlap. All replacements are checked before writing once. Read the relevant content first. LF and CRLF match equivalently. Use an empty new_text to delete text. Keep enough context to identify each location. Combine independent changes to one file in a single call; do not issue dependent edits concurrently.".into(),
        parameters_schema: json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Existing text file, relative to the working directory or absolute" },
                "edits": {
                    "type": "array", "minItems": 1,
                    "items": {
                        "type": "object",
                        "properties": {
                            "old_text": { "type": "string", "minLength": 1 },
                            "new_text": { "type": "string" }
                        },
                        "required": ["old_text", "new_text"],
                        "additionalProperties": false
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
    signal: CancellationToken,
) -> ToolExecution {
    if signal.is_cancelled() {
        return error_result(ABORTED_MESSAGE);
    }
    if args.edits.is_empty() {
        return error_result("edits must contain at least one replacement");
    }
    let target = cwd.join(&args.path);
    let target = match tokio::task::spawn_blocking(move || target.canonicalize())
        .await
        .expect("tool worker completes while the runtime is running")
    {
        Ok(target) => target,
        Err(error) => return error_result(format!("Could not edit {}: {error}", args.path)),
    };
    let key = app_core::workspace_key(&target);
    with_file_lock(key, signal, move |signal| execute_locked(&args, &target, &output_directory, signal)).await
}

pub(super) fn read_opened_text_file(file: File, display_path: &str) -> Result<String, String> {
    let metadata = file.metadata().map_err(|error| format!("Could not inspect {display_path}: {error}"))?;
    if !metadata.is_file() {
        return Err(format!("{display_path} is not a regular file"));
    }
    if metadata.len() > MAX_EDIT_FILE_BYTES {
        return Err(format!("{display_path} exceeds the 20 MB text file limit"));
    }
    let mut reader = BufReader::with_capacity(64 * 1024, file);
    let head = reader.fill_buf().map_err(|error| format!("Could not read {display_path}: {error}"))?;
    if head.contains(&0) {
        return Err(format!("{display_path} is a binary file, not UTF-8 text"));
    }
    let mut bytes = Vec::with_capacity(usize::try_from(metadata.len()).expect("file size is at most 20 MiB"));
    reader
        .take(MAX_EDIT_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("Could not read {display_path}: {error}"))?;
    if bytes.len() as u64 > MAX_EDIT_FILE_BYTES {
        return Err(format!("{display_path} exceeds the 20 MB text file limit"));
    }
    if bytes.contains(&0) {
        return Err(format!("{display_path} is a binary file, not UTF-8 text"));
    }
    String::from_utf8(bytes).map_err(|error| format!("Could not read {display_path} as UTF-8 text: {error}"))
}

fn execute_locked(
    args: &EditArgs,
    target: &Path,
    output_directory: &Path,
    signal: &CancellationToken,
) -> Result<ToolExecution, String> {
    if signal.is_cancelled() {
        return Err(ABORTED_MESSAGE.into());
    }
    let file = File::open(target).map_err(|error| format!("Could not read {}: {error}", args.path))?;
    let content = read_opened_text_file(file, &args.path)?;
    let (view, updated) = apply_edits(&content, &args.path, &args.edits)?;
    if signal.is_cancelled() {
        return Err(ABORTED_MESSAGE.into());
    }
    app_core::write_bytes_atomic(target, updated.as_bytes())
        .map_err(|error| format!("Could not write {}: {error}", args.path))?;
    Ok(format_diff_result("Edited", &args.path, &view, &updated, output_directory))
}

#[cfg(test)]
pub(crate) fn apply_edits_for_test(
    content: &str,
    path: &str,
    edits: &[(&str, &str)],
) -> Result<String, String> {
    let items: Vec<_> = edits
        .iter()
        .map(|(old_text, new_text)| Replacement {
            old_text: (*old_text).into(),
            new_text: (*new_text).into(),
        })
        .collect();
    apply_edits(content, path, &items).map(|(_, updated)| updated)
}

fn apply_edits(content: &str, display_path: &str, edits: &[Replacement]) -> Result<(String, String), String> {
    let source = content.strip_prefix('\u{feff}').unwrap_or(content);
    let view = source.replace("\r\n", "\n");
    let removed: Vec<_> = source
        .match_indices("\r\n")
        .enumerate()
        .map(|(index, (offset, _))| offset - index)
        .collect();
    let source_offset = |offset| offset + removed.partition_point(|removed| *removed < offset);
    let mut matches = Vec::with_capacity(edits.len());
    let mut cached_lines: Option<Vec<LineSpan<'_>>> = None;
    for (index, edit) in edits.iter().enumerate() {
        if edit.new_text.contains('\0') {
            return Err(format!("edits[{index}].new_text must not contain NUL bytes"));
        }
        let old = edit.old_text.replace("\r\n", "\n");
        if old.is_empty() {
            return Err(format!("edits[{index}].old_text must not be empty"));
        }
        let plan = find_unique_match(&view, &old, index, display_path, &mut cached_lines)?;
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
        let replacement = apply_reindent(&raw_replacement, &reindent, index, display_path)?;
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

enum ReindentPlan {
    None,
    Uniform { old_indent: String, file_indent: String },
    Mapped(Option<HashMap<String, String>>),
}

struct MatchPlan {
    start: usize,
    end: usize,
    reindent: ReindentPlan,
}

struct LineSpan<'a> {
    text: &'a str,
    start: usize,
    end: usize,
    has_nl: bool,
}

#[derive(Clone, Copy)]
enum LineMatchMode {
    IndentFlexible,
    Trimmed,
}

fn ambiguous_error(index: usize, path: &str, mode_label: Option<&str>) -> String {
    let mode = mode_label.map_or(String::new(), |label| format!(" ({label})"));
    format!(
        "edits[{index}].old_text matches more than once in {path}{mode}. Include more surrounding context."
    )
}

fn find_unique_match<'a>(
    view: &'a str,
    old: &str,
    index: usize,
    path: &str,
    cached_lines: &mut Option<Vec<LineSpan<'a>>>,
) -> Result<MatchPlan, String> {
    if let Some(start) = view.find(old) {
        let next = start + old.chars().next().expect("old text is nonempty").len_utf8();
        if view[next..].contains(old) {
            return Err(ambiguous_error(index, path, None));
        }
        return Ok(MatchPlan {
            start,
            end: start + old.len(),
            reindent: ReindentPlan::None,
        });
    }
    if !old.chars().any(|ch| !ch.is_whitespace()) {
        return Err(not_found_error(view, old, index, path, cached_lines));
    }
    for (unicode, label) in [
        (false, "ignoring trailing whitespace"),
        (true, "normalizing Unicode punctuation/spaces"),
    ] {
        if let Some(plan) = find_normalized_unique(view, old, unicode, label, index, path)? {
            return Ok(plan);
        }
    }
    let lines = cached_lines.get_or_insert_with(|| split_line_spans(view));
    for (mode, label) in [
        (LineMatchMode::IndentFlexible, "ignoring outer indentation"),
        (LineMatchMode::Trimmed, "ignoring line indentation"),
    ] {
        if let Some(plan) = find_line_block_unique(lines, old, mode, label, index, path)? {
            return Ok(plan);
        }
    }
    Err(not_found_error(view, old, index, path, cached_lines))
}

fn find_normalized_unique(
    view: &str,
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
    let (norm_view, byte_start, byte_end) = normalize_with_map(view, unicode);
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
            reindent: ReindentPlan::None,
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

fn normalize_line(line: &str, mode: LineMatchMode, base_indent: &str) -> String {
    if matches!(mode, LineMatchMode::IndentFlexible) && line.trim().is_empty() {
        return String::new();
    }
    let slice = match mode {
        LineMatchMode::IndentFlexible => line.strip_prefix(base_indent).unwrap_or(line),
        LineMatchMode::Trimmed => line,
    };
    let mapped: String = slice.chars().map(|ch| normalize_char(ch, true)).collect();
    let trimmed = mapped.trim_end_matches([' ', '\t']);
    match mode {
        LineMatchMode::IndentFlexible => trimmed.to_string(),
        LineMatchMode::Trimmed => trimmed.trim_start_matches([' ', '\t']).to_string(),
    }
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

fn has_mixed_indent<'a>(indents: impl Iterator<Item = &'a str>) -> bool {
    let (mut has_space, mut has_tab) = (false, false);
    for indent in indents {
        has_space |= indent.contains(' ');
        has_tab |= indent.contains('\t');
        if has_space && has_tab {
            return true;
        }
    }
    false
}

fn build_indent_map(old_lines: &[&str], window: &[LineSpan<'_>]) -> Option<HashMap<String, String>> {
    let mut map = HashMap::new();
    for (old_line, span) in old_lines.iter().zip(window) {
        if old_line.trim().is_empty() {
            continue;
        }
        let (old_ind, file_ind) = (leading_indent(old_line), leading_indent(span.text));
        match map.get(old_ind) {
            Some(existing) if existing != file_ind => return None,
            Some(_) => {}
            None => {
                map.insert(old_ind.to_string(), file_ind.to_string());
            }
        }
    }
    if has_mixed_indent(map.keys().map(String::as_str)) || has_mixed_indent(map.values().map(String::as_str))
    {
        return None;
    }
    let mut ordered: Vec<(&str, &str)> = map.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    ordered.sort_by_key(|(old_ind, _)| old_ind.len());
    for pair in ordered.windows(2) {
        let ((o1, f1), (o2, f2)) = (pair[0], pair[1]);
        if o1.len() == o2.len() || f1.len() >= f2.len() {
            return None;
        }
    }
    Some(map)
}

fn find_line_block_unique(
    lines: &[LineSpan<'_>],
    old: &str,
    mode: LineMatchMode,
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
    let old_norm: Vec<String> = old_lines.iter().map(|line| normalize_line(line, mode, old_indent)).collect();
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
            .all(|(span, expected)| normalize_line(span.text, mode, file_indent) == *expected)
        {
            continue;
        }
        if matched.is_some() {
            return Err(ambiguous_error(index, path, Some(mode_label)));
        }
        let start = window.first().expect("nonempty window").start;
        let end = last.end + usize::from(old_has_nl);
        let reindent = match mode {
            LineMatchMode::IndentFlexible if file_indent != old_indent => ReindentPlan::Uniform {
                old_indent: old_indent.to_string(),
                file_indent: file_indent.to_string(),
            },
            LineMatchMode::IndentFlexible => ReindentPlan::None,
            LineMatchMode::Trimmed => ReindentPlan::Mapped(build_indent_map(&old_lines, window)),
        };
        matched = Some(MatchPlan { start, end, reindent });
    }
    Ok(matched)
}

fn apply_reindent(
    replacement: &str,
    plan: &ReindentPlan,
    index: usize,
    path: &str,
) -> Result<String, String> {
    match plan {
        ReindentPlan::None => Ok(replacement.to_string()),
        ReindentPlan::Uniform { old_indent, file_indent } => {
            Ok(reindent_replacement(replacement, old_indent, file_indent))
        }
        ReindentPlan::Mapped(map) => {
            if replacement.is_empty() {
                return Ok(String::new());
            }
            let err = || {
                format!(
                    "edits[{index}] matched ignoring indentation in {path}, but replacement indentation cannot be uniquely derived; read the exact indentation and retry."
                )
            };
            let Some(map) = map else {
                return Err(err());
            };
            let mut lines = Vec::new();
            for line in replacement.split('\n') {
                if line.trim().is_empty() {
                    lines.push(line.to_string());
                    continue;
                }
                let ind = leading_indent(line);
                let Some(mapped) = map.get(ind) else {
                    return Err(err());
                };
                lines.push(format!("{mapped}{}", &line[ind.len()..]));
            }
            Ok(lines.join("\n"))
        }
    }
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
    let anchor = old
        .split('\n')
        .map(|line| normalize_line(line, LineMatchMode::Trimmed, ""))
        .find(|line| !line.is_empty());
    if let Some(anchor) = anchor {
        let lines = cached_lines.get_or_insert_with(|| split_line_spans(view));
        let hits: Vec<String> = lines
            .iter()
            .enumerate()
            .filter(|(_, span)| normalize_line(span.text, LineMatchMode::Trimmed, "") == anchor)
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

pub(crate) fn format_diff_result(
    verb: &str,
    path: &str,
    before: &str,
    after: &str,
    output_directory: &Path,
) -> ToolExecution {
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
    let summary = format!("+{added} -{deleted}\n");
    let mut display_output = summary.clone();
    for (index, line) in patch.lines().enumerate() {
        if index >= COMMAND_MAX_LINES || display_output.len() + line.len() + 1 > COMMAND_MAX_BYTES {
            display_output.push_str("[Diff truncated]\n");
            break;
        }
        display_output.push_str(line);
        display_output.push('\n');
    }
    let llm_diff = super::output::format_diff_head_tail(&summary, &patch, output_directory);
    let mut execution = ToolExecution::text(format!("{verb} {path}.\n{llm_diff}"));
    execution.display = Some(Box::new(ToolDisplay {
        output: display_output,
        background_task_id: None,
    }));
    execution
}
