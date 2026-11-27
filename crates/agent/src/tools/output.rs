use std::fmt::Write as _;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use uuid::Uuid;

use crate::tools::registry::{COMMAND_MAX_BYTES, COMMAND_MAX_LINES, WEB_MAX_BYTES, WEB_MAX_LINES};

pub(super) const COMMAND_HEAD_LINES: usize = COMMAND_MAX_LINES / 2;
pub(super) const COMMAND_TAIL_LINES: usize = COMMAND_MAX_LINES - COMMAND_HEAD_LINES;
pub(super) const COMMAND_HEAD_BYTES: usize = COMMAND_MAX_BYTES / 2;
pub(super) const COMMAND_TAIL_BYTES: usize = COMMAND_MAX_BYTES - COMMAND_HEAD_BYTES;
const INTERNAL_TAIL_MAX_BYTES: usize = COMMAND_MAX_BYTES * 2;

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

pub(crate) struct CaptureState {
    directory: PathBuf,
    head: String,
    head_newlines: usize,
    head_sealed: bool,
    head_hit_byte_cap: bool,
    tail: String,
    total_bytes: usize,
    completed_lines: usize,
    spill: Option<io::Result<SpillWriter>>,
    last_progress: Option<(Instant, usize)>,
}

impl CaptureState {
    pub(super) fn persist(&mut self) {
        if self.spill.is_none() {
            self.spill = Some(SpillWriter::create(&self.directory, &self.tail));
        }
    }

    pub(crate) fn new(directory: PathBuf) -> Self {
        Self {
            directory,
            head: String::new(),
            head_newlines: 0,
            head_sealed: false,
            head_hit_byte_cap: false,
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

    fn is_truncated(&self) -> bool {
        self.total_lines() > COMMAND_MAX_LINES || self.total_bytes > COMMAND_MAX_BYTES
    }

    pub(super) fn will_write_disk(&self, text: &str) -> bool {
        match &self.spill {
            Some(Ok(_)) => return true,
            Some(Err(_)) => return false,
            None => {}
        }
        if self.total_bytes.saturating_add(text.len()) > COMMAND_MAX_BYTES {
            return true;
        }
        let incoming_lines = text.bytes().filter(|byte| *byte == b'\n').count();
        self.completed_lines + incoming_lines + 1 > COMMAND_MAX_LINES
    }

    pub(super) fn has_pending_output(&self) -> bool {
        self.total_bytes > 0 && self.last_progress.is_none_or(|(_, bytes)| bytes < self.total_bytes)
    }

    pub(super) fn current_output(&mut self) -> Option<String> {
        let now = Instant::now();
        if self.total_bytes == 0
            || self.last_progress.is_some_and(|(last, bytes)| {
                bytes == self.total_bytes || now.duration_since(last) < Duration::from_millis(100)
            })
        {
            return None;
        }
        self.last_progress = Some((now, self.total_bytes));
        Some(self.display_output())
    }

    pub(super) fn display_output(&self) -> String {
        if self.total_bytes == 0 {
            return String::new();
        }
        let mut content = self.final_output();
        if let Some(note) = self.spill_note() {
            let _ = write!(content, "\n\n{note}");
        }
        content
    }

    pub(crate) fn ingest(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.total_bytes += text.len();
        self.completed_lines += text.bytes().filter(|byte| *byte == b'\n').count();
        if !self.head_sealed {
            let remaining = COMMAND_HEAD_BYTES.saturating_sub(self.head.len());
            let mut end = text.floor_char_boundary(remaining);
            let mut hit_byte_cap = end < text.len();
            let mut hit_line_cap = false;
            for (idx, byte) in text.as_bytes()[..end].iter().enumerate() {
                if *byte == b'\n' {
                    self.head_newlines += 1;
                    if self.head_newlines >= COMMAND_HEAD_LINES {
                        end = idx + 1;
                        hit_line_cap = true;
                        hit_byte_cap = false;
                        break;
                    }
                }
            }
            self.head.push_str(&text[..end]);
            if hit_line_cap || hit_byte_cap {
                self.head_sealed = true;
                self.head_hit_byte_cap = hit_byte_cap;
            }
        }
        if let Some(Ok(spill)) = &mut self.spill
            && let Err(error) = spill.file.write_all(text.as_bytes())
        {
            self.spill = Some(Err(error));
        }
        self.tail.push_str(text);
        if self.spill.is_none() && self.is_truncated() {
            self.spill = Some(SpillWriter::create(&self.directory, &self.tail));
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

    pub(crate) fn finish(self) -> (String, Option<String>) {
        let note = self.spill_note();
        let content = if self.is_truncated() {
            self.final_output()
        } else {
            self.tail
        };
        (content, note)
    }

    fn spill_note(&self) -> Option<String> {
        self.spill.as_ref().map(|spill| match spill {
            Ok(spill) => format!("Full output: {}", spill.path.display()),
            Err(error) => format!("Full output could not be saved: {error}"),
        })
    }

    fn head_truncation(&self) -> Truncation {
        if self.head_hit_byte_cap {
            if let Some(last_nl) = self.head.rfind('\n') {
                let complete = &self.head[..last_nl];
                let output_lines = complete.bytes().filter(|byte| *byte == b'\n').count() + 1;
                return Truncation {
                    content: complete.to_string(),
                    truncated_by: TruncatedBy::Bytes,
                    output_lines,
                    partial_line: false,
                };
            }
            return Truncation {
                content: self.head.clone(),
                truncated_by: TruncatedBy::Bytes,
                output_lines: usize::from(!self.head.is_empty()),
                partial_line: true,
            };
        }
        truncate_lines(&self.head, false, COMMAND_HEAD_LINES, COMMAND_HEAD_BYTES)
    }

    fn final_output(&self) -> String {
        if !self.is_truncated() {
            return self.tail.clone();
        }
        let head = self.head_truncation();
        let tail = truncate_lines(&self.tail, true, COMMAND_TAIL_LINES, COMMAND_TAIL_BYTES);
        format_head_tail_parts(&head, &tail, self.total_bytes as u64, Some(self.total_lines()))
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

pub(super) fn format_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes}B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1}KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1}MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

fn head_tail_marker(
    head: &Truncation,
    tail: &Truncation,
    total_bytes: u64,
    total_lines: Option<usize>,
) -> String {
    let omitted_bytes = total_bytes
        .saturating_sub(head.content.len() as u64)
        .saturating_sub(tail.content.len() as u64);
    match total_lines {
        Some(total_lines) => {
            let omitted_lines = total_lines.saturating_sub(head.output_lines + tail.output_lines);
            if !head.partial_line && !tail.partial_line && omitted_lines > 0 {
                let tail_start = total_lines.saturating_sub(tail.output_lines) + 1;
                format!(
                    "[... {omitted_lines} lines ({}) omitted; showing lines 1-{} and {tail_start}-{total_lines} of {total_lines} ...]",
                    format_size(omitted_bytes),
                    head.output_lines,
                )
            } else {
                format!(
                    "[... {} omitted; showing first {} and last {} of {} ({total_lines} lines) ...]",
                    format_size(omitted_bytes),
                    format_size(head.content.len() as u64),
                    format_size(tail.content.len() as u64),
                    format_size(total_bytes),
                )
            }
        }
        None => format!(
            "[... {} omitted; showing first {} and last {} of {} ...]",
            format_size(omitted_bytes),
            format_size(head.content.len() as u64),
            format_size(tail.content.len() as u64),
            format_size(total_bytes),
        ),
    }
}

fn format_head_tail_parts(
    head: &Truncation,
    tail: &Truncation,
    total_bytes: u64,
    total_lines: Option<usize>,
) -> String {
    let marker = head_tail_marker(head, tail, total_bytes, total_lines);
    format!("{}\n\n{marker}\n\n{}", head.content, tail.content)
}

pub(super) fn format_diff_head_tail(summary: &str, patch: &str, output_directory: &Path) -> String {
    let full_diff = format!("{summary}{patch}");
    let total_lines = count_lines(patch);
    if total_lines <= COMMAND_MAX_LINES && full_diff.len() <= COMMAND_MAX_BYTES {
        return full_diff;
    }
    let spill_note = match SpillWriter::create(output_directory, &full_diff) {
        Ok(spill) => format!("Full output: {}", spill.path.display()),
        Err(error) => format!("Full output could not be saved: {error}"),
    };
    let body = format_head_tail_slice(patch);
    format!("{summary}{body}\n\n{spill_note}")
}

pub(super) fn format_head_tail_slice(content: &str) -> String {
    let total_lines = count_lines(content);
    if content.len() <= COMMAND_MAX_BYTES && total_lines <= COMMAND_MAX_LINES {
        return content.to_string();
    }
    let head = truncate_lines(content, false, COMMAND_HEAD_LINES, COMMAND_HEAD_BYTES);
    let tail = truncate_lines(content, true, COMMAND_TAIL_LINES, COMMAND_TAIL_BYTES);
    format_head_tail_parts(&head, &tail, content.len() as u64, Some(total_lines))
}

pub(super) fn format_spill_head_tail(head_raw: &str, tail_raw: &str, total_unread_bytes: u64) -> String {
    let head = truncate_lines(head_raw, false, COMMAND_HEAD_LINES, COMMAND_HEAD_BYTES);
    let tail = truncate_lines(tail_raw, true, COMMAND_TAIL_LINES, COMMAND_TAIL_BYTES);
    format_head_tail_parts(&head, &tail, total_unread_bytes, None)
}

pub(super) fn count_lines(content: &str) -> usize {
    if content.is_empty() {
        return 0;
    }
    let newlines = content.bytes().filter(|byte| *byte == b'\n').count();
    newlines + usize::from(!content.ends_with('\n'))
}

fn truncate_lines(content: &str, from_end: bool, max_lines: usize, max_bytes: usize) -> Truncation {
    let mut lines = content.split_terminator('\n');
    let mut output: Vec<&str> = Vec::new();
    let mut output_bytes = 0usize;
    let mut truncated_by = TruncatedBy::Bytes;
    while let Some(line) = if from_end { lines.next_back() } else { lines.next() } {
        if output.len() >= max_lines {
            truncated_by = TruncatedBy::Lines;
            break;
        }
        let line_bytes = line.len() + usize::from(!output.is_empty());
        if output_bytes + line_bytes > max_bytes {
            if output.is_empty() {
                let partial = if from_end {
                    truncate_string_to_bytes_from_end(line, max_bytes)
                } else {
                    line[..line.floor_char_boundary(max_bytes)].to_string()
                };
                return Truncation {
                    content: partial,
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
    if from_end {
        output.reverse();
    }
    Truncation {
        content: output.join("\n"),
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

pub(crate) fn format_head_bounded(content: &str, output_directory: &Path) -> String {
    let total_lines = content.lines().count();
    if total_lines <= WEB_MAX_LINES && content.len() <= WEB_MAX_BYTES {
        return content.to_string();
    }
    let head = truncate_lines(content, false, WEB_MAX_LINES, WEB_MAX_BYTES);
    if !head.partial_line && head.output_lines >= total_lines {
        return head.content;
    }
    let spill_note = match SpillWriter::create(output_directory, content) {
        Ok(spill) => format!("Full output: {}", spill.path.display()),
        Err(error) => format!("Full output could not be saved: {error}"),
    };
    let summary = if head.partial_line {
        format!(
            "[Showing the first {} of output (line 1 of {total_lines}). Use read to inspect the full output.]",
            format_size(head.content.len() as u64),
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
            format_size(WEB_MAX_BYTES as u64),
            head.output_lines + 1,
        )
    };
    format!("{}\n\n{summary}\n\n{spill_note}", head.content)
}
