use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::sync::{Arc, LazyLock, Mutex, Weak};

use similar::{ChangeTag, TextDiff};
use tokio_util::sync::CancellationToken;

use super::registry::{
    ABORTED_MESSAGE, COMMAND_MAX_BYTES, COMMAND_MAX_LINES, ToolDisplay, ToolExecution, error_result,
};

const MAX_EDIT_FILE_BYTES: u64 = 20 * 1024 * 1024;

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
            Some(std::path::Component::CurDir) => {}
            Some(std::path::Component::ParentDir) => skip_parents += 1,
            Some(std::path::Component::Normal(name)) => {
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

pub(super) fn format_diff_result(
    verb: &str,
    path: &str,
    before: &str,
    after: &str,
    output_directory: &Path,
) -> ToolExecution {
    let before = app_core::strip_utf8_bom(before).replace("\r\n", "\n");
    let after = app_core::strip_utf8_bom(after).replace("\r\n", "\n");
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
