use std::collections::BTreeMap;

use protocol::{ChangedFile, ChangedFileStatus};

const MAX_DIFF_BYTES: usize = 200 * 1024;
const MAX_DIFF_LINES: usize = 2_000;

pub(super) fn parse_raw_numstat(out: &[u8], files: &mut BTreeMap<String, ChangedFile>) {
    let parse_count = |s: &str| s.parse::<u64>().ok().map_or(0, |n| u32::try_from(n).unwrap_or(u32::MAX));
    let mut parts = out.split(|&b| b == 0);
    while let Some(chunk) = parts.next() {
        if chunk.is_empty() {
            continue;
        }
        if chunk.starts_with(b":") {
            let header = String::from_utf8_lossy(chunk);
            let Some(status_token) = header.split_ascii_whitespace().last() else {
                continue;
            };
            let first_char = status_token.as_bytes().first().copied().unwrap_or(b'M');
            let raw_path = if matches!(first_char, b'R' | b'C') {
                let _old = parts.next();
                let Some(new_bytes) = parts.next() else { break };
                String::from_utf8_lossy(new_bytes).into_owned()
            } else {
                let Some(path_bytes) = parts.next() else { break };
                String::from_utf8_lossy(path_bytes).into_owned()
            };
            let rel = raw_path.replace('\\', "/");
            if rel.is_empty() {
                continue;
            }
            let status = match first_char {
                b'A' => ChangedFileStatus::Added,
                b'D' => ChangedFileStatus::Deleted,
                _ => ChangedFileStatus::Modified,
            };
            files.insert(rel.clone(), ChangedFile { path: rel, added: 0, deleted: 0, status });
            continue;
        }
        let text = String::from_utf8_lossy(chunk);
        let mut fields = text.splitn(3, '\t');
        let (Some(added), Some(deleted), Some(path_field)) = (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let raw_path = if path_field.is_empty() {
            let _old = parts.next();
            let Some(new_bytes) = parts.next() else { break };
            String::from_utf8_lossy(new_bytes).into_owned()
        } else {
            path_field.to_owned()
        };
        let rel = raw_path.replace('\\', "/");
        if !rel.is_empty()
            && let Some(file) = files.get_mut(&rel)
        {
            file.added = parse_count(added);
            file.deleted = parse_count(deleted);
        }
    }
}

pub(super) fn count_lines_in_bytes(bytes: &[u8]) -> u32 {
    if bytes.is_empty() || bytes.contains(&0) {
        return 0;
    }
    let newlines = bytes.iter().filter(|&&b| b == b'\n').count();
    let total = if bytes.last() == Some(&b'\n') {
        newlines
    } else {
        newlines + 1
    };
    u32::try_from(total).unwrap_or(u32::MAX)
}

pub(super) fn format_untracked_bytes(bytes: &[u8], truncated_file: bool) -> String {
    if bytes.contains(&0) {
        return "二进制文件已变更，暂不支持文本差异对比。".to_owned();
    }
    if bytes.is_empty() {
        return "无文本内容差异（空文件或文件模式变更）。".to_owned();
    }
    let text = String::from_utf8_lossy(bytes);
    let lines: Vec<&str> = text.split_terminator('\n').map(|l| l.strip_suffix('\r').unwrap_or(l)).collect();
    let added = u32::try_from(lines.len()).unwrap_or(u32::MAX);
    let mut output = format!("+{added} -0\n@@ -0,0 +1,{added} @@\n");
    let mut truncated = truncated_file;
    for (index, line) in lines.iter().enumerate() {
        if index >= MAX_DIFF_LINES || output.len() + line.len() + 2 > MAX_DIFF_BYTES {
            truncated = true;
            break;
        }
        output.push('+');
        output.push_str(line);
        output.push('\n');
    }
    if truncated {
        output.push_str("[Diff truncated]\n");
    }
    output
}

pub(super) fn format_git_diff(raw: &str) -> String {
    if raw.trim().is_empty() {
        return "该文件当前无改动。".to_owned();
    }
    let (mut in_hunks, mut is_binary, mut truncated) = (false, false, false);
    let (mut added, mut deleted, mut hunk_lines) = (0u32, 0u32, 0usize);
    let mut hunk_buf = String::new();
    for line in raw.lines() {
        if !in_hunks {
            if line.starts_with("@@ ") {
                in_hunks = true;
            } else {
                if (line.starts_with("Binary files ") && line.ends_with(" differ"))
                    || line == "GIT binary patch"
                {
                    is_binary = true;
                }
                continue;
            }
        }
        if line.starts_with('+') {
            added = added.saturating_add(1);
        } else if line.starts_with('-') {
            deleted = deleted.saturating_add(1);
        }
        if !truncated {
            if hunk_lines >= MAX_DIFF_LINES || hunk_buf.len() + line.len() + 1 > MAX_DIFF_BYTES {
                truncated = true;
            } else {
                hunk_buf.push_str(line);
                hunk_buf.push('\n');
                hunk_lines += 1;
            }
        }
    }
    if is_binary {
        "二进制文件已变更，暂不支持文本差异对比。".to_owned()
    } else if !in_hunks {
        "无文本内容差异（空文件或文件模式变更）。".to_owned()
    } else {
        let mut out = format!("+{added} -{deleted}\n{hunk_buf}");
        if truncated {
            out.push_str("[Diff truncated]\n");
        }
        out
    }
}
