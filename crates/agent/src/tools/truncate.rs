//! 工具输出截断（对齐 Pi `truncateHead`/`truncateTail` 语义）。

pub const DEFAULT_MAX_LINES: usize = 2000;
pub const DEFAULT_MAX_BYTES: usize = 50 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TruncatedBy {
    Lines,
    Bytes,
}

/// 截断结果。`content` 为截断后的文本，其余字段描述截断发生的上下文。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Truncation {
    pub content: String,
    pub truncated: bool,
    pub truncated_by: Option<TruncatedBy>,
    /// 原始内容的总行数（结尾换行不产生额外空行）。
    pub total_lines: usize,
    /// 截断后保留的行数。
    pub output_lines: usize,
    /// 仅 bash 尾部截断：末尾单行本身超限时返回该行的尾部（半行）。
    pub last_line_partial: bool,
    /// 仅 read 头部截断：第一行单独超过字节上限。
    pub first_line_exceeds_limit: bool,
}

/// 人类可读字节大小（对齐 Pi `formatSize`）。
pub fn format_size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes}B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1}KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1}MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

/// 行列表；空内容返回空，结尾换行不产生额外空行（对齐 Pi `splitLinesForCounting`）。
fn split_lines(content: &str) -> Vec<&str> {
    if content.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<&str> = content.split('\n').collect();
    if content.ends_with('\n') {
        lines.pop();
    }
    lines
}

/// 保留头部（read 用）：前 `DEFAULT_MAX_LINES` 行且不超过 `DEFAULT_MAX_BYTES` 字节，
/// 绝不输出半行。
pub fn truncate_head(content: &str) -> Truncation {
    let max_lines = DEFAULT_MAX_LINES;
    let max_bytes = DEFAULT_MAX_BYTES;
    let total_bytes = content.len();
    let lines = split_lines(content);
    let total_lines = lines.len();
    if total_lines <= max_lines && total_bytes <= max_bytes {
        return Truncation {
            content: content.to_string(),
            truncated: false,
            truncated_by: None,
            total_lines,
            output_lines: total_lines,
            last_line_partial: false,
            first_line_exceeds_limit: false,
        };
    }
    if lines.first().is_some_and(|first| first.len() > max_bytes) {
        return Truncation {
            content: String::new(),
            truncated: true,
            truncated_by: Some(TruncatedBy::Bytes),
            total_lines,
            output_lines: 0,
            last_line_partial: false,
            first_line_exceeds_limit: true,
        };
    }
    let mut output: Vec<&str> = Vec::new();
    let mut output_bytes = 0usize;
    let mut truncated_by = TruncatedBy::Lines;
    for (index, line) in lines.iter().take(max_lines).enumerate() {
        let line_bytes = line.len() + usize::from(index > 0); // 行间的换行 +1
        if output_bytes + line_bytes > max_bytes {
            truncated_by = TruncatedBy::Bytes;
            break;
        }
        output.push(*line);
        output_bytes += line_bytes;
    }
    if output.len() >= max_lines && output_bytes <= max_bytes {
        truncated_by = TruncatedBy::Lines;
    }
    Truncation {
        content: output.join("\n"),
        truncated: true,
        truncated_by: Some(truncated_by),
        total_lines,
        output_lines: output.len(),
        last_line_partial: false,
        first_line_exceeds_limit: false,
    }
}

/// 保留尾部（bash 用）：最后 `DEFAULT_MAX_LINES` 行且不超过 `DEFAULT_MAX_BYTES` 字节。
/// 末尾单行本身超限时保留其尾部（截断到 UTF-8 字符边界，`last_line_partial = true`）。
pub fn truncate_tail(content: &str) -> Truncation {
    let max_lines = DEFAULT_MAX_LINES;
    let max_bytes = DEFAULT_MAX_BYTES;
    let total_bytes = content.len();
    let lines = split_lines(content);
    let total_lines = lines.len();
    if total_lines <= max_lines && total_bytes <= max_bytes {
        return Truncation {
            content: content.to_string(),
            truncated: false,
            truncated_by: None,
            total_lines,
            output_lines: total_lines,
            last_line_partial: false,
            first_line_exceeds_limit: false,
        };
    }
    let mut output: Vec<&str> = Vec::new();
    let mut output_bytes = 0usize;
    let mut truncated_by = TruncatedBy::Lines;
    for line in lines.iter().rev() {
        if output.len() >= max_lines {
            break;
        }
        let line_bytes = line.len() + usize::from(!output.is_empty()); // 行间的换行 +1
        if output_bytes + line_bytes > max_bytes {
            truncated_by = TruncatedBy::Bytes;
            if output.is_empty() {
                return Truncation {
                    content: truncate_string_to_bytes_from_end(line, max_bytes),
                    truncated: true,
                    truncated_by: Some(TruncatedBy::Bytes),
                    total_lines,
                    output_lines: 1,
                    last_line_partial: true,
                    first_line_exceeds_limit: false,
                };
            }
            break;
        }
        output.push(line);
        output_bytes += line_bytes;
    }
    if output.len() >= max_lines && output_bytes <= max_bytes {
        truncated_by = TruncatedBy::Lines;
    }
    let content = {
        output.reverse();
        output.join("\n")
    };
    Truncation {
        content,
        truncated: true,
        truncated_by: Some(truncated_by),
        total_lines,
        output_lines: output.len(),
        last_line_partial: false,
        first_line_exceeds_limit: false,
    }
}

/// 从行尾截断到 `max_bytes` 字节内（保持 UTF-8 字符完整）。
fn truncate_string_to_bytes_from_end(line: &str, max_bytes: usize) -> String {
    if line.len() <= max_bytes {
        return line.to_string();
    }
    let mut start = line.len() - max_bytes;
    while start < line.len() && (line.as_bytes()[start] & 0b1100_0000) == 0b1000_0000 {
        start += 1;
    }
    line[start..].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_truncation_keeps_last_lines() {
        let content = (1..=2500)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let truncation = truncate_tail(&content);
        assert!(truncation.truncated);
        assert_eq!(truncation.truncated_by, Some(TruncatedBy::Lines));
        assert_eq!(truncation.total_lines, 2500);
        assert_eq!(truncation.output_lines, 2000);
        assert!(truncation.content.starts_with("line 501"));
        assert!(truncation.content.ends_with("line 2500"));
    }

    #[test]
    fn head_truncation_keeps_first_lines() {
        let content = (1..=2500)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let truncation = truncate_head(&content);
        assert!(truncation.truncated);
        assert!(truncation.content.starts_with("line 1"));
        assert!(truncation.content.ends_with("line 2000"));
        assert!(!truncation.content.contains("line 2001"));
    }
}
