//! bash 的输出捕获：尾部缓冲、行数与字节数统计，以及完整输出的 spill。

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use uuid::Uuid;

use crate::tools::truncate::{
    DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, TruncatedBy, format_size, truncate_tail,
};

/// 内存里保留的尾部缓冲字节上限（100KB），防止超大的单行输出耗尽内存。
pub(super) const INTERNAL_TAIL_MAX_BYTES: usize = DEFAULT_MAX_BYTES * 2;

/// 截断发生时，用来保存完整输出的临时文件写入器。位置在
/// <TEMP>/singularity-tool-output/<uuid>/<命令slug>.log，调用结束后不会清理；
/// 每次创建新的 spill 时，顺手删掉同一根目录下超过七天的旧文件。
pub(super) struct SpillWriter {
    pub(super) path: PathBuf,
    file: std::fs::File,
}

impl SpillWriter {
    /// 创建 spill 文件，initial 是它的完整初始内容。
    fn create(root: &Path, slug: &str, initial: &str) -> io::Result<Self> {
        std::fs::create_dir_all(root)?;
        cleanup_old_spills(root, std::time::SystemTime::now());
        let dir = root.join(Uuid::new_v4().to_string());
        singularity_core::create_data_dir(&dir).map_err(io::Error::other)?;
        let path = dir.join(format!("{slug}.log"));
        let mut file = singularity_core::create_new_file(&path)?;
        file.write_all(initial.as_bytes())?;
        Ok(Self { path, file })
    }

    fn append(&mut self, text: &str) -> io::Result<()> {
        self.file.write_all(text.as_bytes())
    }
}

const SPILL_RETENTION: std::time::Duration = std::time::Duration::from_secs(7 * 24 * 60 * 60);

/// 把命令文本转成对文件名安全的 slug（只保留 ASCII 字母数字与 -_.，最长 40 个字符）。
fn command_slug(command: &str) -> String {
    let mut slug: String = command
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric()
                || character == '-'
                || character == '_'
                || character == '.'
            {
                character
            } else {
                '-'
            }
        })
        .collect();
    slug = slug.trim_matches('-').to_string();
    if slug.is_empty() {
        slug = "command".to_string();
    }
    slug.truncate(40);
    slug
}

fn cleanup_old_spills(root: &std::path::Path, now: std::time::SystemTime) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        let Ok(age) = now.duration_since(modified) else {
            continue;
        };
        if age <= SPILL_RETENTION {
            continue;
        }
        if metadata.is_file() {
            let _ = std::fs::remove_file(path);
        } else if metadata.is_dir() {
            let _ = std::fs::remove_dir_all(path);
        }
    }
}

/// 累计输出的状态：尾部缓冲（上限 2×50KB）以及行数和字节数统计。超出展示上限的
/// 输出只保留尾部缓冲；第一次丢弃字节之前先建 spill 文件保存完整输出，之后每个
/// chunk 都同步追加；保存失败时把原因留下来，不再声称有完整输出。
#[derive(Default)]
pub(super) struct CaptureState {
    tail: String,
    total_bytes: usize,
    completed_lines: usize,
    pub(super) spill: Option<io::Result<SpillWriter>>,
    command_slug: String,
    last_progress: Option<(Instant, usize)>,
}

impl CaptureState {
    pub(super) fn new(command: &str) -> Self {
        Self {
            command_slug: command_slug(command),
            ..Default::default()
        }
    }

    /// 累计行数：尾部缓冲只裁掉前缀、保留末尾，所以看「是否以换行结尾」就能判断有没有
    /// 开行。未闭合行的完整长度无法从裁剪后的尾部还原，截断说明里也不得声称它。
    fn total_lines(&self) -> usize {
        let has_open_line = !self.tail.is_empty() && !self.tail.ends_with('\n');
        self.completed_lines + usize::from(has_open_line)
    }

    fn is_truncated(&self) -> bool {
        self.total_lines() > DEFAULT_MAX_LINES || self.total_bytes > DEFAULT_MAX_BYTES
    }

    /// 确保完整输出已经进入落盘通道：成功一次之后就是空操作，失败一次之后不再重试。
    /// 必须在尾部缓冲丢弃任何字节之前调用，写进去的才是完整输出。
    fn ensure_spill(&mut self) {
        if self.spill.is_none() {
            let root = std::env::temp_dir().join("singularity-tool-output");
            let created = SpillWriter::create(&root, &self.command_slug, &self.tail);
            self.spill = Some(created);
        }
    }

    /// 首次进度立刻返回；之后每有新输出，最多每 100ms 构造一次累计尾部的快照。
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
        Some(if self.is_truncated() {
            truncate_tail(&self.tail).content
        } else {
            self.tail.clone()
        })
    }

    /// 吸收一个清洗过的 chunk：更新计数和尾部缓冲。空 chunk 不改变任何状态。
    ///
    /// spill 的 I/O 也在这里：spill 启用后每个 chunk 都同步追加写入；尾部缓冲第一次
    /// 超出内部上限之前先建 spill，这样写进去的才是丢弃前的完整输出；追加失败就放弃
    /// 这个 spill（不再声称有完整输出），文件本身留在临时目录按保留期清理。
    pub(super) fn ingest(&mut self, text: &str) {
        self.total_bytes += text.len();
        self.completed_lines += text.bytes().filter(|byte| *byte == b'\n').count();
        if let Some(Ok(spill)) = &mut self.spill
            && let Err(error) = spill.append(text)
        {
            // 追加失败之后完整输出已经无法恢复：放弃这个 spill，后续不再输出假路径。
            self.spill = Some(Err(error));
        }
        self.tail.push_str(text);
        if self.tail.len() > INTERNAL_TAIL_MAX_BYTES {
            self.ensure_spill();
            self.tail = crate::tools::truncate::truncate_string_to_bytes_from_end(
                &self.tail,
                INTERNAL_TAIL_MAX_BYTES,
            );
        }
    }

    /// 在「已经发生截断、但 spill 还没启用」时（属于最终裁剪型截断，尾部缓冲从未
    /// 丢过字节），把完整输出一次性写进 spill。
    pub(super) fn ensure_spill_for_final_truncation(&mut self) {
        if self.is_truncated() {
            self.ensure_spill();
        }
    }

    /// 最终的展示文本：没截断时就是完整输出，截断时已经带上说明。
    ///
    /// 说明只讲能证实的事实：展示了多少尾部字节、处在哪些行、触发了哪个限制。单行
    /// 超限时尾部缓冲里只剩该行的末尾，完整行长已经丢了，所以不能报告「这一行有多少
    /// 字节」；说明在这里拼接完成，调用方拿到的就是最终文本。
    pub(super) fn final_output(&self) -> String {
        if !self.is_truncated() {
            return self.tail.clone();
        }
        let tail_result = truncate_tail(&self.tail);
        let total_lines = self.total_lines();
        // 尾部缓冲本身没被裁剪时，截断只可能来自累计输出：按累计的行数或字节数区分原因。
        let truncated_by = match tail_result.truncated_by {
            Some(reason) => reason,
            None if self.total_bytes > DEFAULT_MAX_BYTES => TruncatedBy::Bytes,
            None => TruncatedBy::Lines,
        };
        let start_line = total_lines.saturating_sub(tail_result.output_lines) + 1;
        let end_line = total_lines;
        let note = if tail_result.last_line_partial {
            format!(
                "[Showing the last {} of output, ending at line {end_line}.]",
                format_size(tail_result.content.len()),
            )
        } else if truncated_by == TruncatedBy::Lines {
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
