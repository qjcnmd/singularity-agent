//! bash 输出捕获：尾部缓冲、行/字节计数与完整输出 spill。

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use uuid::Uuid;

use crate::tools::truncate::{
    DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, TruncatedBy, format_size, truncate_tail,
};

/// 内存中保留的尾部缓冲区字节上限（100KB），防止超大单行输出耗尽内存。
pub(super) const INTERNAL_TAIL_MAX_BYTES: usize = DEFAULT_MAX_BYTES * 2;

/// 截断发生时保存完整输出的临时文件写入器。位于
/// <TEMP>/singularity-tool-output/<uuid>/<命令slug>.log，不随调用结束清理。
/// 创建新 spill 时惰性删除同根目录下超过七天的旧文件。
pub(super) struct SpillWriter {
    pub(super) path: PathBuf,
    file: std::fs::File,
}

impl SpillWriter {
    /// 以 initial 为完整初始内容创建 spill 文件。
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

/// 把命令文本投影为文件名安全的 slug（ASCII 字母数字与 -_.，其余折叠为
/// -，去除首尾 -，最长 40 字符）。
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

/// 累计输出状态：尾部缓冲（上限 2×50KB）、行/字节计数。超出展示上限的输出
/// 只保留尾部缓冲；首次丢弃字节前创建 spill 文件保存完整输出，其后每个
/// chunk 同步追加；保存失败时保留原因，不再声称有完整输出。
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

    /// 累计行数：已闭合行加上末尾是否还有一个未闭合行。
    ///
    /// 尾部缓冲只裁前缀、保留末尾，因此「是否以换行结束」就能判定开行，不必
    /// 另存一个只能表达开行的计数字段；未闭合行的完整长度无法从裁剪后的尾部
    /// 恢复，截断说明也不得声称它。
    fn total_lines(&self) -> usize {
        let has_open_line = !self.tail.is_empty() && !self.tail.ends_with('\n');
        self.completed_lines + usize::from(has_open_line)
    }

    fn is_truncated(&self) -> bool {
        self.total_lines() > DEFAULT_MAX_LINES || self.total_bytes > DEFAULT_MAX_BYTES
    }

    /// 确保完整输出已在落盘通道中：成功一次后为 no-op，失败一次后不再重试。
    /// 必须在尾部缓冲丢弃任何字节之前调用，写入的才是完整输出。
    fn ensure_spill(&mut self) {
        if self.spill.is_none() {
            let root = std::env::temp_dir().join("singularity-tool-output");
            let created = SpillWriter::create(&root, &self.command_slug, &self.tail);
            self.spill = Some(created);
        }
    }

    /// 首份进度立即返回，后续有新输出时最多每 100ms 构造一次累计尾部快照。
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

    /// 吸收一个清洗后的 chunk：更新计数与尾部缓冲。空 chunk 不改变任何状态。
    ///
    /// 这里还承担 spill 的 I/O：spill 已启用时每个 chunk 同步追加写入；尾部缓冲
    /// 首次超出内部上限前创建 spill，写入的才是丢弃前的完整输出；追加失败时关闭
    /// 并放弃该 spill（不再声称有完整输出），文件本身留在临时目录按保留期清理。
    pub(super) fn ingest(&mut self, text: &str) {
        self.total_bytes += text.len();
        self.completed_lines += text.bytes().filter(|byte| *byte == b'\n').count();
        if let Some(Ok(spill)) = &mut self.spill
            && let Err(error) = spill.append(text)
        {
            // 追加失败后完整输出不再可恢复：放弃 spill，后续不再输出假路径。
            self.spill = Some(Err(error));
        }
        self.tail.push_str(text);
        if self.tail.len() > INTERNAL_TAIL_MAX_BYTES {
            // 首次丢弃前保存完整窗口；spill 已就绪或已放弃后不再重复克隆尾部。
            self.ensure_spill();
            self.tail = crate::tools::truncate::truncate_string_to_bytes_from_end(
                &self.tail,
                INTERNAL_TAIL_MAX_BYTES,
            );
        }
    }

    /// 截断已发生且 spill 尚未启用（最终裁剪型截断，尾部缓冲从未丢弃字节）
    /// 时，把完整输出一次性写入 spill。
    pub(super) fn ensure_spill_for_final_truncation(&mut self) {
        if self.is_truncated() {
            self.ensure_spill();
        }
    }

    /// 最终的展示文本：未截断时就是完整输出，截断时已带上说明。
    ///
    /// 说明只陈述可证实的事实：展示的尾部字节量、所在行位置与触发的限制。
    /// 单行超限时尾部缓冲里只剩该行的末尾，完整行长已经丢失，因此不得报告
    /// 「该行有多少字节」。说明的拼接就在这里完成，调用方拿到的即最终文本。
    pub(super) fn final_output(&self) -> String {
        if !self.is_truncated() {
            return self.tail.clone();
        }
        let tail_result = truncate_tail(&self.tail);
        let total_lines = self.total_lines();
        // 尾部缓冲自身未被裁剪时，截断只来自累计输出：按累计行/字节区分原因。
        let truncated_by = match tail_result.truncated_by {
            Some(reason) => reason,
            None if self.total_bytes > DEFAULT_MAX_BYTES => TruncatedBy::Bytes,
            None => TruncatedBy::Lines,
        };
        let start_line = total_lines.saturating_sub(tail_result.output_lines) + 1;
        let end_line = total_lines;
        let note = if tail_result.last_line_partial {
            // 只展示超限末行的尾部字节；该行的完整长度已不在捕获窗口内。
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
