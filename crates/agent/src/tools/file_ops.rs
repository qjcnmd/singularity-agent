use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::sync::Mutex;

use similar::{ChangeTag, TextDiff};

use super::registry::{COMMAND_MAX_BYTES, COMMAND_MAX_LINES, ToolDisplay, ToolExecution};

const MAX_EDIT_FILE_BYTES: u64 = 20 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileStamp {
    modified: Option<std::time::SystemTime>,
    len: u64,
}

impl FileStamp {
    fn from_metadata(metadata: &std::fs::Metadata) -> Self {
        Self {
            modified: metadata.modified().ok(),
            len: metadata.len(),
        }
    }
}

#[derive(Debug, Default)]
pub struct FileReadTracker {
    stamps: Mutex<HashMap<String, FileStamp>>,
}

impl FileReadTracker {
    pub(crate) fn record_metadata(&self, key: String, metadata: &std::fs::Metadata) {
        self.stamps
            .lock()
            .expect("file read tracker lock poisoned")
            .insert(key, FileStamp::from_metadata(metadata));
    }

    pub(crate) fn verify_unchanged(
        &self,
        key: &str,
        metadata: &std::fs::Metadata,
        display_path: &str,
        verb: &str,
    ) -> Result<(), String> {
        let recorded = self.stamps.lock().expect("file read tracker lock poisoned").get(key).copied();
        match recorded {
            None => Err(format!(
                "Cannot {verb} {display_path}: file has not been read in this session yet. Call read first."
            )),
            Some(previous) if previous != FileStamp::from_metadata(metadata) => Err(format!(
                "Cannot {verb} {display_path}: file was modified on disk since it was last read. Call read again before continuing."
            )),
            Some(_) => Ok(()),
        }
    }

    pub fn clear(&self) {
        self.stamps.lock().expect("file read tracker lock poisoned").clear();
    }
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
