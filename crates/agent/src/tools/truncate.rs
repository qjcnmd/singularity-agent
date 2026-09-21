//! 工具输出文本的安全截断算法。
//!
//! 提供按行数或按字节数对工具返回内容进行有界截断的能力，防止超大单次命令输出
//! 或大文件读取撑爆模型上下文；上限数值只在下方常量一处声明，工具描述与运行时
//! 提示都由这里生成。

pub const DEFAULT_MAX_LINES: usize = 2000;
pub const DEFAULT_MAX_BYTES: usize = 50 * 1024;

/// 展示上限的 KB 数值：工具描述与运行时提示共用，改上限不必同步文案。
pub const fn default_max_kb() -> usize {
    DEFAULT_MAX_BYTES / 1024
}

/// read 与 bash 共用的展示上限文案。
pub fn default_cap_summary() -> String {
    format!("{DEFAULT_MAX_LINES} lines or {}KB", default_max_kb())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TruncatedBy {
    Lines,
    Bytes,
}

/// 截断结果结构体。content 为截断后的安全文本，其余字段记录截断元数据。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Truncation {
    pub content: String,
    pub truncated_by: Option<TruncatedBy>,
    /// 截断后实际保留的行数。
    pub output_lines: usize,
    /// 尾部截断场景：末尾单行本身超限时是否只保留了该行的尾部。
    pub last_line_partial: bool,
}

/// 将字节数格式化为人类可读的容量大小字符串（如 45.2KB、1.5MB）。
pub fn format_size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes}B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1}KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1}MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

/// 保留尾部（bash 用）：最后 DEFAULT_MAX_LINES 行且不超过 DEFAULT_MAX_BYTES 字节。
/// 末尾单行本身超限时保留其尾部（截断到 UTF-8 字符边界，last_line_partial = true）。
pub fn truncate_tail(content: &str) -> Truncation {
    let max_lines = DEFAULT_MAX_LINES;
    let max_bytes = DEFAULT_MAX_BYTES;
    let total_bytes = content.len();
    let lines = content.split_terminator('\n');
    let total_lines = lines.clone().count();
    if total_lines <= max_lines && total_bytes <= max_bytes {
        return Truncation {
            content: content.to_string(),
            truncated_by: None,
            output_lines: total_lines,
            last_line_partial: false,
        };
    }
    let mut output: Vec<&str> = Vec::new();
    let mut output_bytes = 0usize;
    // 截断类别由实际触发的边界决定：行数上限命中时按行报告，否则按字节报告。
    // 去掉末尾换行后正文仍可能恰好放下，此时超限原因仍是字节。
    let mut truncated_by = TruncatedBy::Bytes;
    for line in lines.rev() {
        if output.len() >= max_lines {
            truncated_by = TruncatedBy::Lines;
            break;
        }
        let line_bytes = line.len() + usize::from(!output.is_empty()); // 行间的换行 +1
        if output_bytes + line_bytes > max_bytes {
            if output.is_empty() {
                return Truncation {
                    content: truncate_string_to_bytes_from_end(line, max_bytes),
                    truncated_by: Some(TruncatedBy::Bytes),
                    output_lines: 1,
                    last_line_partial: true,
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
        truncated_by: Some(truncated_by),
        output_lines: output.len(),
        last_line_partial: false,
    }
}

/// 从行尾截断到 max_bytes 字节内（保持 UTF-8 字符完整）。
pub(crate) fn truncate_string_to_bytes_from_end(line: &str, max_bytes: usize) -> String {
    if line.len() <= max_bytes {
        return line.to_string();
    }
    let start = line.ceil_char_boundary(line.len() - max_bytes);
    line[start..].to_string()
}
