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
    ABORTED_MESSAGE, DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, ToolDisplay, ToolExecution, error_result,
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
        description: "Edit an existing UTF-8 text file using one or more exact replacements. Each old_text must be nonempty and unique in the original file; edits must not overlap. All replacements are checked before writing once. Read the relevant content first. LF and CRLF match equivalently; other whitespace and characters must match exactly. Use an empty new_text to delete text. Keep enough context to identify each location. Combine independent changes to one file in a single call; do not issue dependent edits concurrently.".into(),
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

pub(crate) async fn execute(args: EditArgs, cwd: PathBuf, signal: CancellationToken) -> ToolExecution {
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
    with_file_lock(key, signal, move |signal| execute_locked(&args, &target, signal)).await
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
    signal: &CancellationToken,
) -> Result<ToolExecution, String> {
    if signal.is_cancelled() {
        return Err(ABORTED_MESSAGE.into());
    }
    let file = File::open(target).map_err(|error| format!("Could not read {}: {error}", args.path))?;
    let content = read_opened_text_file(file, &args.path)?;
    let source = content.strip_prefix('\u{feff}').unwrap_or(&content);
    let view = source.replace("\r\n", "\n");
    let removed: Vec<_> = source
        .match_indices("\r\n")
        .enumerate()
        .map(|(index, (offset, _))| offset - index)
        .collect();
    let source_offset = |offset| offset + removed.partition_point(|removed| *removed < offset);
    let mut matches = Vec::with_capacity(args.edits.len());
    for (index, edit) in args.edits.iter().enumerate() {
        if edit.new_text.contains('\0') {
            return Err(format!("edits[{index}].new_text must not contain NUL bytes"));
        }
        let old = edit.old_text.replace("\r\n", "\n");
        if old.is_empty() {
            return Err(format!("edits[{index}].old_text must not be empty"));
        }
        let start = view.find(&old).ok_or_else(|| {
            format!(
                "edits[{index}].old_text was not found in {}. Read the current text and retry.",
                args.path
            )
        })?;
        let next = start + old.chars().next().expect("old text is nonempty").len_utf8();
        if view[next..].contains(&old) {
            return Err(format!(
                "edits[{index}].old_text matches more than once in {}. Include more surrounding context.",
                args.path
            ));
        }
        matches.push((source_offset(start), source_offset(start + old.len()), index));
    }
    matches.sort_unstable();
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
    for (start, end, index) in matches {
        updated.push_str(&source[cursor..start]);
        let region = &source[start..end];
        let replacement = args.edits[index].new_text.replace("\r\n", "\n");
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
    if signal.is_cancelled() {
        return Err(ABORTED_MESSAGE.into());
    }
    app_core::write_bytes_atomic(target, updated.as_bytes())
        .map_err(|error| format!("Could not write {}: {error}", args.path))?;
    Ok(format_diff_result("Edited", &args.path, &view, &updated))
}

pub(super) fn format_diff_result(verb: &str, path: &str, before: &str, after: &str) -> ToolExecution {
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
    let mut execution = ToolExecution::text(format!("{verb} {path}.\n{output}"));
    execution.display = Some(Box::new(ToolDisplay { output, background_task_id: None }));
    execution
}
