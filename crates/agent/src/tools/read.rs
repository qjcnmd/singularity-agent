//! read 工具：有界流式读取指定文件内容，支持基于 `offset` 与 `limit` 的行范围读取。

use std::fs::File;
use std::io::{self, BufRead, BufReader};

use serde_json::{Value, json};
use singularity_core::CancellationToken;

use super::registry::{ExecuteContext, ToolError, ToolExecution, error_result, resolve_path};
use super::truncate::{DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, format_size};

/// 单行硬上限：一行超过 4 MiB 视为不可安全读取的输入。
const MAX_READ_LINE_BYTES: usize = 4 * 1024 * 1024;
pub(crate) const DESCRIPTION: &str = "Read the contents of a text file. Output is truncated to 2000 lines or 50KB (whichever is hit first). Use offset/limit for large files. When you need the full file, continue with offset until complete.";

pub(crate) fn parameters() -> Value {
    json!({
        "type": "object",
        "properties": {
            "path": { "type": "string", "description": "Path to the file to read (relative or absolute)" },
            "offset": { "type": "integer", "description": "Line number to start reading from (1-indexed)" },
            "limit": { "type": "integer", "description": "Maximum number of lines to read" },
        },
        "required": ["path"],
        "additionalProperties": false,
    })
}

pub(crate) fn spec() -> super::registry::ToolSpec {
    super::registry::ToolSpec {
        name: "read",
        description: DESCRIPTION,
        parameters: parameters(),
        supports_parallel: true,
        execute,
    }
}

pub(crate) fn execute(ctx: ExecuteContext<'_>) -> Result<ToolExecution, ToolError> {
    let Some(path) = ctx.args.get("path").and_then(Value::as_str) else {
        return error_result("missing required parameter \"path\"");
    };
    let offset = ctx.args.get("offset").and_then(Value::as_u64);
    let limit = ctx.args.get("limit").and_then(Value::as_u64);
    if ctx.signal.is_some_and(|signal| signal.is_cancelled()) {
        return error_result("Operation aborted");
    }
    let full_path = resolve_path(ctx.cwd, path);
    let file = match File::open(&full_path) {
        Ok(file) => file,
        Err(error) => {
            return error_result(format!("Could not read file: {path}. {error}"));
        }
    };
    let mut reader = BufReader::with_capacity(64 * 1024, file);
    execute_reader(path, offset, limit, &mut reader, ctx.signal)
}

fn execute_reader(
    path: &str,
    offset: Option<u64>,
    limit: Option<u64>,
    reader: &mut impl BufRead,
    signal: Option<&CancellationToken>,
) -> Result<ToolExecution, ToolError> {
    if signal.is_some_and(CancellationToken::is_cancelled) {
        return error_result("Operation aborted");
    }
    let start_line = offset.map_or(0, |offset| (offset as usize).saturating_sub(1));
    let start_line_display = start_line + 1;
    let user_line_limit = limit.map_or(DEFAULT_MAX_LINES, |limit| {
        usize::try_from(limit)
            .unwrap_or(DEFAULT_MAX_LINES)
            .min(DEFAULT_MAX_LINES)
    });
    let mut state = ReadState {
        selected: Vec::new(),
        selected_bytes: 0,
        selected_truncated: false,
        first_line_exceeds_limit: false,
        first_line_len: None,
    };
    let mut line_number = 0usize;
    loop {
        if signal.is_some_and(CancellationToken::is_cancelled) {
            return error_result("Operation aborted");
        }
        let Some(line) = (match read_bounded_line(reader, signal) {
            Ok(line) => line,
            Err(ReadFailure::Cancelled) => return error_result("Operation aborted"),
            Err(error) => {
                return error_result(format!("Could not read file: {path}. {error}"));
            }
        }) else {
            break;
        };
        if signal.is_some_and(CancellationToken::is_cancelled) {
            return error_result("Operation aborted");
        }
        line_number += 1;
        let selected_position = line_number.saturating_sub(start_line);
        if selected_position == 0 {
            continue;
        }
        if selected_position == 1 && line.len() > DEFAULT_MAX_BYTES {
            state.first_line_exceeds_limit = true;
            state.first_line_len = Some(line.len());
            break;
        }
        // 选中窗口已满（例如 limit 为 0）时无需再读取或换算后续行。
        if state.selected.len() >= user_line_limit {
            break;
        }
        let next_bytes = state
            .selected_bytes
            .saturating_add(line.len())
            .saturating_add(usize::from(!state.selected.is_empty()));
        if next_bytes > DEFAULT_MAX_BYTES {
            state.selected_truncated = true;
            break;
        }
        state
            .selected
            .push(String::from_utf8_lossy(&line).into_owned());
        state.selected_bytes = next_bytes;
        if state.selected.len() >= user_line_limit {
            // 收集满 limit 即停：只需确认文件是否还有后续，不再扫到 EOF 统计行数。
            state.selected_truncated = match read_bounded_line(reader, signal) {
                Ok(Some(_)) => true,
                Ok(None) => false,
                Err(ReadFailure::Cancelled) => return error_result("Operation aborted"),
                Err(error) => {
                    return error_result(format!("Could not read file: {path}. {error}"));
                }
            };
            break;
        }
    }

    if line_number == 0 {
        if offset.is_some_and(|offset| offset > 1) {
            return error_result(format!(
                "Offset {} is beyond end of file (0 lines total)",
                offset.unwrap_or(0)
            ));
        }
        return Ok(ToolExecution {
            content: String::new(),
            is_error: false,
        });
    }

    if start_line >= line_number {
        return error_result(format!(
            "Offset {} is beyond end of file ({line_number} lines total)",
            offset.unwrap_or(0)
        ));
    }
    let output_text = render_read_output(path, start_line_display, &state);
    Ok(ToolExecution {
        content: output_text,
        is_error: false,
    })
}

struct ReadState {
    selected: Vec<String>,
    selected_bytes: usize,
    selected_truncated: bool,
    first_line_exceeds_limit: bool,
    first_line_len: Option<usize>,
}

fn render_read_output(path: &str, start_line_display: usize, state: &ReadState) -> String {
    let selected_content = state.selected.join("\n");
    if state.first_line_exceeds_limit {
        let line_len = state
            .first_line_len
            .unwrap_or(DEFAULT_MAX_BYTES.saturating_add(1));
        return format!(
            "[Line {start_line_display} is {}, exceeds {} limit. Use bash: sed -n '{start_line_display}p' {path} | head -c {}]",
            format_size(line_len),
            format_size(DEFAULT_MAX_BYTES),
            DEFAULT_MAX_BYTES
        );
    }
    if state.selected_truncated && !state.selected.is_empty() {
        let end_line_display =
            start_line_display.saturating_add(state.selected.len().saturating_sub(1));
        let next_offset = end_line_display.saturating_add(1);
        return format!(
            "{selected_content}\n\n[Showing lines {start_line_display}-{end_line_display}. File continues; use offset={next_offset} to continue.]"
        );
    }
    selected_content
}

#[derive(Debug)]
enum ReadFailure {
    Cancelled,
    Io(io::Error),
}

impl std::fmt::Display for ReadFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled => formatter.write_str("Operation aborted"),
            Self::Io(error) => error.fmt(formatter),
        }
    }
}

/// 有界读取一行：单行超过 MAX_READ_LINE_BYTES 时 fail closed，不分配无界内存。
fn read_bounded_line(
    reader: &mut impl BufRead,
    signal: Option<&CancellationToken>,
) -> Result<Option<Vec<u8>>, ReadFailure> {
    let mut bytes = Vec::new();
    let newline_terminated = loop {
        if signal.is_some_and(CancellationToken::is_cancelled) {
            return Err(ReadFailure::Cancelled);
        }
        let (take_len, newline_terminated) = {
            let buffer = reader.fill_buf().map_err(ReadFailure::Io)?;
            if buffer.is_empty() {
                break false;
            }
            let newline = buffer.iter().position(|byte| *byte == b'\n');
            let take_len = newline.map_or(buffer.len(), |index| index.saturating_add(1));
            if bytes.len().saturating_add(take_len) > MAX_READ_LINE_BYTES.saturating_add(1) {
                return Err(ReadFailure::Io(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("line exceeds {MAX_READ_LINE_BYTES} bytes"),
                )));
            }
            bytes.extend_from_slice(&buffer[..take_len]);
            (take_len, newline.is_some())
        };
        reader.consume(take_len);
        if newline_terminated {
            break true;
        }
    };
    if bytes.is_empty() {
        return Ok(None);
    }
    if !newline_terminated && bytes.len() > MAX_READ_LINE_BYTES {
        return Err(ReadFailure::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("line exceeds {MAX_READ_LINE_BYTES} bytes"),
        )));
    }
    if newline_terminated {
        bytes.pop();
        if bytes.last() == Some(&b'\r') {
            bytes.pop();
        }
    }
    Ok(Some(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::registry::ToolRegistry;
    use crate::tools::test_support::context;
    use serde_json::json;
    use std::fs;
    use std::io::{self, Read};
    use std::sync::mpsc::{self, Receiver, Sender};
    use std::thread;
    use std::time::Duration;
    use tempfile::tempdir;

    fn write_lines(dir: &std::path::Path, name: &str, lines: usize) {
        let content = (1..=lines)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(dir.join(name), content).expect("write fixture");
    }

    #[test]
    fn reads_file_content() {
        let dir = tempdir().expect("temp dir");
        fs::write(dir.path().join("sample.txt"), "line1\nline2\nline3").expect("fixture");
        let result = ToolRegistry::new()
            .execute("read", context(json!({ "path": "sample.txt" }), dir.path()))
            .expect("execute");
        assert!(!result.is_error, "content: {}", result.content);
        assert!(result.content.contains("line1\nline2\nline3"));
    }

    #[test]
    fn offset_and_limit_select_lines() {
        let dir = tempdir().expect("temp dir");
        write_lines(dir.path(), "sample.txt", 10);
        let result = ToolRegistry::new()
            .execute(
                "read",
                context(
                    json!({ "path": "sample.txt", "offset": 3, "limit": 2 }),
                    dir.path(),
                ),
            )
            .expect("execute");
        assert!(!result.is_error, "content: {}", result.content);
        assert_eq!(
            result.content,
            "line3\nline4\n\n[Showing lines 3-4. File continues; use offset=5 to continue.]"
        );
    }

    #[test]
    fn reading_stops_at_limit_on_large_file_with_continue_hint() {
        let dir = tempdir().expect("temp dir");
        write_lines(dir.path(), "large.txt", 5000);
        let result = ToolRegistry::new()
            .execute("read", context(json!({ "path": "large.txt" }), dir.path()))
            .expect("execute");
        assert!(!result.is_error, "content: {}", result.content);
        assert!(
            result
                .content
                .contains("[Showing lines 1-2000. File continues; use offset=2001 to continue.]"),
            "content: {}",
            result.content
        );
        assert_eq!(result.content.lines().next(), Some("line1"));
    }

    #[test]
    fn reading_exactly_limit_lines_does_not_claim_file_continues() {
        let dir = tempdir().expect("temp dir");
        write_lines(dir.path(), "exact.txt", 5);
        let result = ToolRegistry::new()
            .execute(
                "read",
                context(json!({ "path": "exact.txt", "limit": 5 }), dir.path()),
            )
            .expect("execute");
        assert!(!result.is_error, "content: {}", result.content);
        assert!(!result.content.contains("File continues"));
        assert!(result.content.contains("line5"));
    }

    /// 有界读取：收集满 limit 后不再向后读取更多字节。
    ///
    /// 该 reader 一旦被读取/消费到超过 `fail_at` 的偏移就返回 I/O 错误；读取整份
    /// 文件会撞上该边界，而按 limit 提前停止则不会。以此可观测地证明"达限即停"。
    struct FailPastBoundReader {
        cursor: io::Cursor<Vec<u8>>,
        fail_at: usize,
    }

    impl FailPastBoundReader {
        fn new(content: Vec<u8>, fail_at: usize) -> Self {
            Self {
                cursor: io::Cursor::new(content),
                fail_at,
            }
        }
    }

    impl Read for FailPastBoundReader {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            let position = self.cursor.position() as usize;
            if position >= self.fail_at {
                return Err(io::Error::other("read past bound"));
            }
            let allowed = (self.fail_at - position).min(buffer.len());
            let limited = &mut buffer[..allowed];
            self.cursor.read(limited)
        }
    }

    impl BufRead for FailPastBoundReader {
        fn fill_buf(&mut self) -> io::Result<&[u8]> {
            let position = self.cursor.position() as usize;
            if position >= self.fail_at {
                return Err(io::Error::other("read past bound"));
            }
            Ok(&self.cursor.get_ref()[position..])
        }

        fn consume(&mut self, amount: usize) {
            self.cursor
                .set_position(self.cursor.position() + amount as u64);
        }
    }

    #[test]
    fn read_breaks_at_limit_without_scanning_to_eof() {
        // "a\nb\nc\nd\ne"：读取 3 行（含确认文件继续的一行）即到 6 字节。
        let content = b"a\nb\nc\nd\ne".to_vec();
        // fail_at = 6 意味着第 4 行起报错；按 limit 提前停止则不会触达。
        let mut reader = FailPastBoundReader::new(content, 6);
        let result = execute_reader("probe.txt", None, Some(2), &mut reader, None)
            .expect("read must not trip the bound");
        assert!(!result.is_error, "content: {}", result.content);
        assert!(
            result
                .content
                .contains("File continues; use offset=3 to continue."),
            "content: {}",
            result.content
        );
    }

    #[test]
    fn offset_beyond_end_is_error() {
        let dir = tempdir().expect("temp dir");
        write_lines(dir.path(), "sample.txt", 3);
        let result = ToolRegistry::new()
            .execute(
                "read",
                context(json!({ "path": "sample.txt", "offset": 10 }), dir.path()),
            )
            .expect("execute");
        assert!(result.is_error);
        assert!(
            result
                .content
                .contains("Offset 10 is beyond end of file (3 lines total)")
        );
    }

    #[test]
    fn offset_scan_reaches_true_eof_beyond_legacy_scan_cap() {
        let dir = tempdir().expect("temp dir");
        let path = dir.path().join("over-cap.txt");
        let line = "x".repeat(1024);
        let content = (0..66_000)
            .map(|index| format!("{line}-{index}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(content.len() > 64 * 1024 * 1024);
        std::fs::write(&path, content).expect("fixture");

        let result = ToolRegistry::new()
            .execute(
                "read",
                context(
                    json!({ "path": "over-cap.txt", "offset": 66_000, "limit": 1 }),
                    dir.path(),
                ),
            )
            .expect("execute");
        assert!(!result.is_error, "content: {}", result.content);
        assert!(
            result.content.ends_with("-65999"),
            "content: {}",
            result.content
        );
        assert!(
            !result.content.contains("scan limit"),
            "content: {}",
            result.content
        );
    }

    struct BlockingReader {
        bytes: Vec<u8>,
        position: usize,
        started: Option<Sender<(usize, usize)>>,
        release: Option<Receiver<()>>,
    }

    impl BlockingReader {
        fn new(bytes: Vec<u8>, started: Sender<(usize, usize)>, release: Receiver<()>) -> Self {
            Self {
                bytes,
                position: 0,
                started: Some(started),
                release: Some(release),
            }
        }
    }

    impl Read for BlockingReader {
        fn read(&mut self, target: &mut [u8]) -> io::Result<usize> {
            let amount;
            {
                let available = self.fill_buf()?;
                amount = available.len().min(target.len());
                target[..amount].copy_from_slice(&available[..amount]);
            }
            self.consume(amount);
            Ok(amount)
        }
    }

    impl BufRead for BlockingReader {
        fn fill_buf(&mut self) -> io::Result<&[u8]> {
            Ok(&self.bytes[self.position..])
        }

        fn consume(&mut self, amount: usize) {
            let next_position = self.position.saturating_add(amount).min(self.bytes.len());
            let consumed = next_position.saturating_sub(self.position);
            self.position = next_position;
            if consumed == 0 {
                return;
            }
            if let (Some(started), Some(release)) = (self.started.take(), self.release.take()) {
                let consumed_lines = self.bytes[..self.position]
                    .iter()
                    .filter(|byte| **byte == b'\n')
                    .count();
                started
                    .send((self.position, consumed_lines))
                    .expect("read scan started receiver");
                release.recv().expect("release blocked reader");
            }
        }
    }

    #[test]
    fn read_cancelled_during_scan_returns_aborted_without_partial_output() {
        let data = (1..=100)
            .map(|line| format!("line{line}"))
            .collect::<Vec<_>>()
            .join("\n")
            .into_bytes();
        let target_offset = 90_u64;
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let cancellation = singularity_core::CancellationToken::new();
        let worker_cancellation = cancellation.clone();
        let worker = thread::spawn(move || {
            let mut reader = BlockingReader::new(data, started_tx, release_rx);
            execute_reader(
                "cancellation.txt",
                Some(target_offset),
                Some(10),
                &mut reader,
                Some(&worker_cancellation),
            )
        });

        let (consumed_bytes, consumed_lines) = started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("read scan must consume input before cancellation");
        assert!(consumed_bytes > 0);
        assert_eq!(consumed_lines, 1);
        assert!(consumed_lines < target_offset as usize);

        cancellation.cancel();
        release_tx.send(()).expect("release read scan");
        let result = worker
            .join()
            .expect("read scan worker")
            .expect("read scan result");
        assert!(result.is_error);
        assert_eq!(result.content, "Operation aborted");
    }

    #[test]
    fn missing_file_is_error() {
        let dir = tempdir().expect("temp dir");
        let result = ToolRegistry::new()
            .execute(
                "read",
                context(json!({ "path": "missing.txt" }), dir.path()),
            )
            .expect("execute");
        assert!(result.is_error);
        assert!(result.content.contains("Could not read file"));
    }

    #[test]
    fn oversized_line_fails_closed_without_allocating_the_file() {
        let dir = tempdir().expect("temp dir");
        let path = dir.path().join("large.txt");
        std::fs::write(&path, "x".repeat(MAX_READ_LINE_BYTES + 1)).expect("fixture");
        let result = ToolRegistry::new()
            .execute("read", context(json!({ "path": "large.txt" }), dir.path()))
            .expect("execute");
        assert!(result.is_error, "content: {}", result.content);
        assert!(result.content.contains("line exceeds"));
    }

    #[test]
    fn max_sized_line_with_newline_is_accepted() {
        let mut input = vec![b'x'; MAX_READ_LINE_BYTES];
        input.push(b'\n');
        let mut reader = std::io::Cursor::new(input);

        let line = read_bounded_line(&mut reader, None)
            .expect("read boundary line")
            .expect("line must be present");
        assert_eq!(line.len(), MAX_READ_LINE_BYTES);
        assert!(
            read_bounded_line(&mut reader, None)
                .expect("read EOF")
                .is_none()
        );
    }

    #[test]
    fn reads_empty_file_successfully() {
        let dir = tempdir().expect("temp dir");
        let path = dir.path().join("empty.txt");
        std::fs::write(&path, "").expect("fixture");
        let result = ToolRegistry::new()
            .execute("read", context(json!({ "path": "empty.txt" }), dir.path()))
            .expect("execute");
        assert!(!result.is_error, "content: {}", result.content);
        assert_eq!(result.content, "");

        let with_offset_one = ToolRegistry::new()
            .execute(
                "read",
                context(json!({ "path": "empty.txt", "offset": 1 }), dir.path()),
            )
            .expect("execute");
        assert!(
            !with_offset_one.is_error,
            "content: {}",
            with_offset_one.content
        );
        assert_eq!(with_offset_one.content, "");
    }

    #[test]
    fn reads_empty_file_with_offset_beyond_end_is_error() {
        let dir = tempdir().expect("temp dir");
        let path = dir.path().join("empty.txt");
        std::fs::write(&path, "").expect("fixture");
        let result = ToolRegistry::new()
            .execute(
                "read",
                context(json!({ "path": "empty.txt", "offset": 2 }), dir.path()),
            )
            .expect("execute");
        assert!(result.is_error);
        assert!(
            result
                .content
                .contains("Offset 2 is beyond end of file (0 lines total)")
        );
    }
}
