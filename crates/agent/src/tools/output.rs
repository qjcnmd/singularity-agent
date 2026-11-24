use std::fmt::Write as _;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use uuid::Uuid;

use crate::tools::registry::{DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES};

const INTERNAL_TAIL_MAX_BYTES: usize = DEFAULT_MAX_BYTES * 2;

struct SpillWriter {
    path: PathBuf,
    file: std::fs::File,
}

impl SpillWriter {
    fn create(root: &Path, initial: &str) -> io::Result<Self> {
        std::fs::create_dir_all(root)?;
        let path = root.join(format!("{}.log", Uuid::new_v4()));
        let mut file = std::fs::File::create_new(&path)?;
        file.write_all(initial.as_bytes())?;
        Ok(Self { path, file })
    }
}

pub(super) struct CaptureState {
    directory: Option<PathBuf>,
    tail: String,
    total_bytes: usize,
    completed_lines: usize,
    spill: Option<io::Result<SpillWriter>>,
    last_progress: Option<(Instant, usize)>,
}

impl CaptureState {
    pub(super) fn persist(&mut self) {
        if self.spill.is_none()
            && let Some(directory) = &self.directory
        {
            self.spill = Some(SpillWriter::create(directory, &self.tail));
        }
    }

    pub(super) fn new(directory: PathBuf) -> Self {
        Self::with_directory(Some(directory))
    }

    pub(super) fn new_in_memory() -> Self {
        Self::with_directory(None)
    }

    fn with_directory(directory: Option<PathBuf>) -> Self {
        Self {
            directory,
            tail: String::new(),
            total_bytes: 0,
            completed_lines: 0,
            spill: None,
            last_progress: None,
        }
    }

    fn total_lines(&self) -> usize {
        let has_open_line = !self.tail.is_empty() && !self.tail.ends_with('\n');
        self.completed_lines + usize::from(has_open_line)
    }

    pub(super) fn is_truncated(&self) -> bool {
        self.total_lines() > DEFAULT_MAX_LINES || self.total_bytes > DEFAULT_MAX_BYTES
    }

    pub(super) fn has_spill(&self) -> bool {
        self.spill.is_some()
    }

    pub(super) fn will_write_disk(&self, text: &str) -> bool {
        if self.directory.is_none() {
            return false;
        }
        match &self.spill {
            Some(Ok(_)) => return true,
            Some(Err(_)) => return false,
            None => {}
        }
        if self.total_bytes.saturating_add(text.len()) > DEFAULT_MAX_BYTES {
            return true;
        }
        let incoming_lines = text.bytes().filter(|byte| *byte == b'\n').count();
        self.completed_lines + incoming_lines + 1 > DEFAULT_MAX_LINES
    }

    pub(super) fn has_pending_output(&self) -> bool {
        self.total_bytes > 0 && self.last_progress.is_none_or(|(_, bytes)| bytes < self.total_bytes)
    }

    pub(super) fn current_output(&mut self, spill_note: Option<&str>) -> Option<String> {
        let now = Instant::now();
        if self.total_bytes == 0
            || self.last_progress.is_some_and(|(last, bytes)| {
                bytes == self.total_bytes || now.duration_since(last) < Duration::from_millis(100)
            })
        {
            return None;
        }
        self.last_progress = Some((now, self.total_bytes));
        Some(self.display_output(spill_note))
    }

    pub(super) fn display_output(&self, spill_note: Option<&str>) -> String {
        if self.total_bytes == 0 {
            return String::new();
        }
        let mut content = self.final_output();
        if let Some(note) = spill_note {
            let _ = write!(content, "\n\n{note}");
        }
        content
    }

    pub(super) fn ingest(&mut self, text: &str) {
        self.total_bytes += text.len();
        self.completed_lines += text.bytes().filter(|byte| *byte == b'\n').count();
        if let Some(Ok(spill)) = &mut self.spill
            && let Err(error) = spill.file.write_all(text.as_bytes())
        {
            self.spill = Some(Err(error));
        }
        self.tail.push_str(text);
        if self.spill.is_none()
            && self.is_truncated()
            && let Some(directory) = &self.directory
        {
            self.spill = Some(SpillWriter::create(directory, &self.tail));
        }
        if self.tail.len() > INTERNAL_TAIL_MAX_BYTES {
            self.tail = truncate_string_to_bytes_from_end(&self.tail, INTERNAL_TAIL_MAX_BYTES);
        }
    }

    pub(super) fn snapshot(&self) -> protocol::CommandOutputSnapshot {
        protocol::CommandOutputSnapshot {
            text: self.tail.clone(),
            total_bytes: self.total_bytes as u64,
            full_output: self
                .spill
                .as_ref()
                .and_then(|spill| spill.as_ref().ok().map(|spill| spill.path.to_string_lossy().into_owned())),
            file_error: self.spill.as_ref().and_then(|spill| spill.as_ref().err().map(ToString::to_string)),
            incomplete: false,
        }
    }

    pub(super) fn finish(self) -> (String, Option<String>) {
        let note = self.spill_note();
        let content = if self.is_truncated() {
            self.final_output()
        } else {
            self.tail
        };
        (content, note)
    }

    pub(super) fn spill_note(&self) -> Option<String> {
        self.spill.as_ref().map(|spill| match spill {
            Ok(spill) => format!("Full output: {}", spill.path.display()),
            Err(error) => format!("Full output could not be saved: {error}"),
        })
    }

    fn final_output(&self) -> String {
        if !self.is_truncated() {
            return self.tail.clone();
        }
        let tail_result = truncate_tail(&self.tail);
        let total_lines = self.total_lines();
        let start_line = total_lines.saturating_sub(tail_result.output_lines) + 1;
        let end_line = total_lines;
        let note = if tail_result.partial_line {
            format!(
                "[Showing the last {} of output, ending at line {end_line}.]",
                format_size(tail_result.content.len()),
            )
        } else if tail_result.truncated_by == TruncatedBy::Lines {
            format!("[Showing lines {start_line}-{end_line} of {total_lines}.]")
        } else {
            format!(
                "[Showing lines {start_line}-{end_line} of {total_lines} ({} limit).]",
                format_size(DEFAULT_MAX_BYTES),
            )
        };
        format!("{}\n\n{note}", tail_result.content)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TruncatedBy {
    Lines,
    Bytes,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Truncation {
    content: String,
    truncated_by: TruncatedBy,
    output_lines: usize,
    partial_line: bool,
}

fn format_size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes}B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1}KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1}MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

fn truncate_tail(content: &str) -> Truncation {
    let lines = content.split_terminator('\n');
    let mut output: Vec<&str> = Vec::new();
    let mut output_bytes = 0usize;
    let mut truncated_by = TruncatedBy::Bytes;
    for line in lines.rev() {
        if output.len() >= DEFAULT_MAX_LINES {
            truncated_by = TruncatedBy::Lines;
            break;
        }
        let line_bytes = line.len() + usize::from(!output.is_empty());
        if output_bytes + line_bytes > DEFAULT_MAX_BYTES {
            if output.is_empty() {
                return Truncation {
                    content: truncate_string_to_bytes_from_end(line, DEFAULT_MAX_BYTES),
                    truncated_by: TruncatedBy::Bytes,
                    output_lines: 1,
                    partial_line: true,
                };
            }
            break;
        }
        output.push(line);
        output_bytes += line_bytes;
    }
    let content = {
        output.reverse();
        output.join("\n")
    };
    Truncation {
        content,
        truncated_by,
        output_lines: output.len(),
        partial_line: false,
    }
}

fn truncate_string_to_bytes_from_end(line: &str, max_bytes: usize) -> String {
    if line.len() <= max_bytes {
        return line.to_string();
    }
    let start = line.ceil_char_boundary(line.len() - max_bytes);
    line[start..].to_string()
}

pub(super) fn format_head_bounded(content: &str, output_directory: &Path) -> String {
    let total_lines = content.lines().count();
    if total_lines <= DEFAULT_MAX_LINES && content.len() <= DEFAULT_MAX_BYTES {
        return content.to_string();
    }
    let head = truncate_head(content);
    let spill_note = match SpillWriter::create(output_directory, content) {
        Ok(spill) => format!("Full output: {}", spill.path.display()),
        Err(error) => format!("Full output could not be saved: {error}"),
    };
    let summary = if head.partial_line {
        format!(
            "[Showing the first {} of output (line 1 of {total_lines}). Use read to inspect the full output.]",
            format_size(head.content.len()),
        )
    } else if head.truncated_by == TruncatedBy::Lines {
        format!(
            "[Showing lines 1-{} of {total_lines}. Use read with offset={} to inspect the rest.]",
            head.output_lines,
            head.output_lines + 1,
        )
    } else {
        format!(
            "[Showing lines 1-{} of {total_lines} ({} limit). Use read with offset={} to inspect the rest.]",
            head.output_lines,
            format_size(DEFAULT_MAX_BYTES),
            head.output_lines + 1,
        )
    };
    format!("{}\n\n{summary}\n\n{spill_note}", head.content)
}

fn truncate_head(content: &str) -> Truncation {
    let mut output: Vec<&str> = Vec::new();
    let mut output_bytes = 0usize;
    let mut truncated_by = TruncatedBy::Bytes;
    for line in content.split_terminator('\n') {
        if output.len() >= DEFAULT_MAX_LINES {
            truncated_by = TruncatedBy::Lines;
            break;
        }
        let line_bytes = line.len() + usize::from(!output.is_empty());
        if output_bytes + line_bytes > DEFAULT_MAX_BYTES {
            if output.is_empty() {
                let end = line.floor_char_boundary(DEFAULT_MAX_BYTES);
                return Truncation {
                    content: line[..end].to_string(),
                    truncated_by: TruncatedBy::Bytes,
                    output_lines: 1,
                    partial_line: true,
                };
            }
            break;
        }
        output.push(line);
        output_bytes += line_bytes;
    }
    let output_lines = output.len();
    Truncation {
        content: output.join("\n"),
        truncated_by,
        output_lines,
        partial_line: false,
    }
}
