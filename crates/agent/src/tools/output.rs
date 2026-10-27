//! 工具输出的捕获：只在内存保留末尾部分，统计行数和字节数，超出显示上限时把完整输出写到临时文件。

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use uuid::Uuid;

use crate::tools::truncate::{DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, TruncatedBy, format_size, truncate_tail};

/// 末尾缓冲的上限（100KB），输出再长也不会占满内存。
const INTERNAL_TAIL_MAX_BYTES: usize = DEFAULT_MAX_BYTES * 2;

/// 输出超过显示上限时保存完整输出的临时文件，位于
/// <TEMP>/singularity-tool-output/<uuid>.log，调用结束后保留，供模型和用户查看。
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

/// 累计输出的状态：内存里保留末尾部分（最多 100KB），并统计总行数和总字节数。
/// 输出首次超过显示上限（2000 行或 50KB）时创建临时文件写入已有缓冲，之后每段输出都追加进去；
/// 写入失败时记下原因，结果里改为说明完整输出没能保存。
#[derive(Default)]
pub(super) struct CaptureState {
    tail: String,
    total_bytes: usize,
    completed_lines: usize,
    spill: Option<io::Result<SpillWriter>>,
    last_progress: Option<(Instant, usize)>,
}

impl CaptureState {
    /// 总行数：已结束的行数，加上末尾还没换行的一行（如果有）。尾部缓冲裁掉的是开头，
    /// 所以看缓冲是否以换行结尾就能判断；这一行被裁过的完整长度无法得知，截断说明里不报。
    fn total_lines(&self) -> usize {
        let has_open_line = !self.tail.is_empty() && !self.tail.ends_with('\n');
        self.completed_lines + usize::from(has_open_line)
    }

    fn is_truncated(&self) -> bool {
        self.total_lines() > DEFAULT_MAX_LINES || self.total_bytes > DEFAULT_MAX_BYTES
    }

    /// 运行中显示的输出：第一次有输出立即返回，之后有新输出时最多每 100ms 返回一次；
    /// 超出上限时附上完整输出文件的位置。
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
        let mut content = self.final_output();
        if let Some(note) = self.spill_note() {
            content.push_str(&format!("\n\n{note}"));
        }
        Some(content)
    }

    /// 接收一段已解码的输出；首次超出显示上限时把已有输出写入临时文件，再把内存缓冲限制在 100KB 内。
    /// 之后每段输出都追加到文件；保存失败后记下错误。
    pub(super) fn ingest(&mut self, text: &str) {
        self.total_bytes += text.len();
        self.completed_lines += text.bytes().filter(|byte| *byte == b'\n').count();
        if let Some(Ok(spill)) = &mut self.spill
            && let Err(error) = spill.file.write_all(text.as_bytes())
        {
            // 追加失败后临时文件内容已不完整，结果里改报保存失败原因。
            self.spill = Some(Err(error));
        }
        self.tail.push_str(text);
        if self.spill.is_none() && self.is_truncated() {
            let root = std::env::temp_dir().join("singularity-tool-output");
            self.spill = Some(SpillWriter::create(&root, &self.tail));
        }
        if self.tail.len() > INTERNAL_TAIL_MAX_BYTES {
            self.tail = crate::tools::truncate::truncate_string_to_bytes_from_end(
                &self.tail,
                INTERNAL_TAIL_MAX_BYTES,
            );
        }
    }

    /// 结束捕获，返回显示用的正文和完整输出文件的说明。
    /// 调用方把文件说明放在命令结束原因之后。
    pub(super) fn finish(self) -> (String, Option<String>) {
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

    /// 显示用的文本：没超出上限时是完整输出，超出时是末尾部分加截断说明。
    ///
    /// 说明只写确定知道的信息：显示了多少字节、哪些行、超出了哪个上限。单行超长时
    /// 缓冲里只剩这一行的末尾，无法给出这一行的总长度。
    fn final_output(&self) -> String {
        if !self.is_truncated() {
            return self.tail.clone();
        }
        let tail_result = truncate_tail(&self.tail);
        let total_lines = self.total_lines();
        let start_line = total_lines.saturating_sub(tail_result.output_lines) + 1;
        let end_line = total_lines;
        let note = if tail_result.last_line_partial {
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
