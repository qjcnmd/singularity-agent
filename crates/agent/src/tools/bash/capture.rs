//! bash 输出捕获：尾部缓冲、行/字节计数与完整输出 spill。

use std::io::{self, Write};

use uuid::Uuid;

use crate::tools::truncate::{
    DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, TruncatedBy, format_size, truncate_tail,
};

/// 内存中保留的尾部缓冲区字节上限（100KB），防止超大单行输出耗尽内存。
pub(super) const INTERNAL_TAIL_MAX_BYTES: usize = DEFAULT_MAX_BYTES * 2;

/// 截断发生时保存完整输出的临时文件写入器。位于
/// `<TEMP>/singularity-tool-output/<uuid>/<命令slug>.log`，不主动清理
/// 创建新 spill 时惰性删除同根目录下超过七天的旧文件。
pub(super) struct SpillWriter {
    pub(super) path: std::path::PathBuf,
    pub(super) file: std::fs::File,
}

impl SpillWriter {
    /// 以 `initial` 为完整初始内容创建 spill 文件。
    fn create(slug: &str, initial: &str) -> io::Result<Self> {
        let root = std::env::temp_dir().join("singularity-tool-output");
        std::fs::create_dir_all(&root)?;
        cleanup_old_spills(&root, std::time::SystemTime::now());
        let dir = root.join(Uuid::new_v4().to_string());
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{slug}.log"));
        let mut file = std::fs::File::create(&path)?;
        file.write_all(initial.as_bytes())?;
        Ok(Self { path, file })
    }

    fn append(&mut self, text: &str) -> io::Result<()> {
        self.file.write_all(text.as_bytes())
    }
}

const SPILL_RETENTION: std::time::Duration = std::time::Duration::from_secs(7 * 24 * 60 * 60);

/// 把命令文本投影为文件名安全的 slug（ASCII 字母数字与 `-_.`，其余折叠为
/// `-`，去除首尾 `-`，最长 40 字符）。
pub(super) fn command_slug(command: &str) -> String {
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
/// chunk 同步追加，保证截断时完整输出可从 spill 恢复。
#[derive(Default)]
pub(super) struct CaptureState {
    pub(super) tail: String,
    pub(super) total_bytes: usize,
    pub(super) completed_lines: usize,
    pub(super) has_open_line: bool,
    pub(super) current_line_bytes: usize,
    pub(super) spill: Option<SpillWriter>,
    pub(super) spill_failed: bool,
    pub(super) command_slug: String,
}

impl CaptureState {
    fn total_lines(&self) -> usize {
        self.completed_lines + usize::from(self.has_open_line)
    }

    fn is_truncated(&self) -> bool {
        self.total_lines() > DEFAULT_MAX_LINES || self.total_bytes > DEFAULT_MAX_BYTES
    }

    /// spill 文件路径（已成功创建时）。
    pub(super) fn spill_path(&self) -> Option<&std::path::Path> {
        self.spill.as_ref().map(|spill| spill.path.as_path())
    }

    /// 确保完整输出已在落盘通道中：成功一次后为 no-op，失败一次后不再重试。
    /// 必须在尾部缓冲丢弃任何字节之前调用，写入的才是完整输出。
    fn ensure_spill(&mut self, initial: &str) {
        if self.spill.is_some() || self.spill_failed {
            return;
        }
        match SpillWriter::create(&self.command_slug, initial) {
            Ok(writer) => self.spill = Some(writer),
            Err(_) => self.spill_failed = true,
        }
    }

    /// 当前应展示给流式回调的输出（超限时为截断尾部）。
    pub(super) fn current_output(&self) -> String {
        if self.is_truncated() {
            truncate_tail(&self.tail).content
        } else {
            self.tail.clone()
        }
    }

    /// 吸收一个清洗后的 chunk：更新计数与尾部缓冲。
    pub(super) fn ingest(&mut self, text: &str) {
        self.total_bytes += text.len();
        self.completed_lines += text.bytes().filter(|byte| *byte == b'\n').count();
        match text.rfind('\n') {
            Some(last_newline) => {
                let trailing = &text[last_newline + 1..];
                self.current_line_bytes = trailing.len();
                self.has_open_line = !trailing.is_empty();
            }
            None => {
                self.current_line_bytes += text.len();
                self.has_open_line = true;
            }
        }
        let spill_append_failed = self
            .spill
            .as_mut()
            .is_some_and(|spill| spill.append(text).is_err());
        if spill_append_failed {
            // 追加失败后完整输出不再可恢复：放弃 spill，后续不再输出假路径。
            self.spill = None;
            self.spill_failed = true;
        }
        self.tail.push_str(text);
        if self.tail.len() > INTERNAL_TAIL_MAX_BYTES {
            // 首次丢弃前保存完整窗口；spill 已就绪或已放弃后不再重复克隆尾部。
            if self.spill.is_none() && !self.spill_failed {
                self.ensure_spill(&self.tail.clone());
            }
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
            self.ensure_spill(&self.tail.clone());
        }
    }

    /// 生成最终的展示文本与截断说明信息。
    pub(super) fn final_progress(&self) -> BashProgress {
        let tail_result = truncate_tail(&self.tail);
        let total_lines = self.total_lines();
        if !self.is_truncated() {
            return BashProgress {
                output_text: self.tail.clone(),
                note: None,
            };
        }
        let truncated_by = if tail_result.truncated {
            tail_result.truncated_by.unwrap_or(TruncatedBy::Lines)
        } else if self.total_bytes > DEFAULT_MAX_BYTES {
            TruncatedBy::Bytes
        } else {
            TruncatedBy::Lines
        };
        let start_line = total_lines.saturating_sub(tail_result.output_lines) + 1;
        let end_line = total_lines;
        let note = if tail_result.last_line_partial {
            format!(
                "[Showing last {} of line {end_line} (line is {}).]",
                format_size(tail_result.content.len()),
                format_size(self.current_line_bytes)
            )
        } else if truncated_by == TruncatedBy::Lines {
            format!("[Showing lines {start_line}-{end_line} of {total_lines}.]")
        } else {
            format!(
                "[Showing lines {start_line}-{end_line} of {total_lines} ({} limit).]",
                format_size(DEFAULT_MAX_BYTES),
            )
        };
        BashProgress {
            output_text: tail_result.content,
            note: Some(note),
        }
    }
}

pub(super) struct BashProgress {
    pub(super) output_text: String,
    pub(super) note: Option<String>,
}
