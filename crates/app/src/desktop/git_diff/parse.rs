use std::collections::BTreeMap;

use protocol::{ChangedFile, ChangedFileStatus};

const MAX_DIFF_BYTES: usize = 200 * 1024;
const MAX_DIFF_LINES: usize = 2_000;

pub(super) struct DiffSection {
    git_header: String,
    rename_to: Option<String>,
    minus_path: Option<String>,
    plus_path: Option<String>,
    status: ChangedFileStatus,
    is_binary: bool,
    in_hunks: bool,
    added: u32,
    deleted: u32,
    hunk_lines: usize,
    hunk_buf: String,
    hunk_truncated: bool,
}

impl Default for DiffSection {
    fn default() -> Self {
        Self {
            git_header: String::new(),
            rename_to: None,
            minus_path: None,
            plus_path: None,
            status: ChangedFileStatus::Modified,
            is_binary: false,
            in_hunks: false,
            added: 0,
            deleted: 0,
            hunk_lines: 0,
            hunk_buf: String::new(),
            hunk_truncated: false,
        }
    }
}

impl DiffSection {
    pub(super) fn new(git_header: String) -> Self {
        Self { git_header, ..Self::default() }
    }

    pub(super) fn push_line(&mut self, line: &str, capture_text: bool) {
        if !self.in_hunks {
            if line.starts_with("@@ ") {
                self.in_hunks = true;
            } else {
                if line.starts_with("new file mode ") {
                    self.status = ChangedFileStatus::Added;
                } else if line.starts_with("deleted file mode ") {
                    self.status = ChangedFileStatus::Deleted;
                } else if let Some(rest) =
                    line.strip_prefix("rename to ").or_else(|| line.strip_prefix("copy to "))
                {
                    self.rename_to = Some(unquote_git_path(rest));
                } else if let Some(rest) = line.strip_prefix("--- ") {
                    self.minus_path = parse_patch_path(rest, "a/");
                } else if let Some(rest) = line.strip_prefix("+++ ") {
                    self.plus_path = parse_patch_path(rest, "b/");
                } else if (line.starts_with("Binary files ") && line.ends_with(" differ"))
                    || line == "GIT binary patch"
                {
                    self.is_binary = true;
                }
                return;
            }
        }
        if line.starts_with('+') {
            self.added = self.added.saturating_add(1);
        } else if line.starts_with('-') {
            self.deleted = self.deleted.saturating_add(1);
        }
        if capture_text && !self.hunk_truncated {
            if self.hunk_lines >= MAX_DIFF_LINES || self.hunk_buf.len() + line.len() + 1 > MAX_DIFF_BYTES {
                self.hunk_truncated = true;
            } else {
                self.hunk_buf.push_str(line);
                self.hunk_buf.push('\n');
                self.hunk_lines += 1;
            }
        }
    }

    fn target_path(&self) -> Option<String> {
        self.rename_to
            .clone()
            .or_else(|| self.plus_path.clone())
            .or_else(|| self.minus_path.clone())
            .or_else(|| parse_diff_git_header(&self.git_header))
    }

    fn into_formatted(self, allow_diff: bool) -> (ChangedFileStatus, u32, u32, Option<String>) {
        if !allow_diff {
            return (self.status, self.added, self.deleted, None);
        }
        let text = if self.is_binary {
            "二进制文件已变更，暂不支持文本差异对比。".to_owned()
        } else if !self.in_hunks {
            "无文本内容差异（空文件或文件模式变更）。".to_owned()
        } else {
            let mut out = format!("+{} -{}\n{}", self.added, self.deleted, self.hunk_buf);
            if self.hunk_truncated {
                out.push_str("[Diff truncated]\n");
            }
            out
        };
        (self.status, self.added, self.deleted, Some(text))
    }
}

pub(super) fn commit_section(
    section: DiffSection,
    files: &mut BTreeMap<String, ChangedFile>,
    remaining_budget: &mut usize,
) {
    let Some(raw_path) = section.target_path() else {
        return;
    };
    let rel = raw_path.replace('\\', "/");
    if rel.is_empty() {
        return;
    }
    let (status, added, deleted, formatted) = section.into_formatted(*remaining_budget > 0);
    let diff = match formatted {
        Some(text) if text.len() <= *remaining_budget => {
            *remaining_budget -= text.len();
            Some(text)
        }
        _ => None,
    };
    files.insert(rel.clone(), ChangedFile { path: rel, added, deleted, status, diff });
}

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
            files.insert(
                rel.clone(),
                ChangedFile {
                    path: rel,
                    added: 0,
                    deleted: 0,
                    status,
                    diff: None,
                },
            );
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
    let mut section = DiffSection::default();
    for line in raw.lines() {
        section.push_line(line, true);
    }
    section.into_formatted(true).3.unwrap_or_default()
}

fn parse_patch_path(raw: &str, strip: &str) -> Option<String> {
    let trimmed = raw.strip_suffix('\t').unwrap_or(raw);
    if trimmed == "/dev/null" {
        return None;
    }
    unquote_git_path(trimmed).strip_prefix(strip).map(str::to_owned)
}

fn parse_diff_git_header(line: &str) -> Option<String> {
    let rest = line.strip_prefix("diff --git ")?;
    if rest.starts_with('"') {
        let mut escaped = false;
        let mut first_end = None;
        for (idx, ch) in rest.char_indices().skip(1) {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                first_end = Some(idx + 1);
                break;
            }
        }
        let tail = rest[first_end?..].trim_start();
        return unquote_git_path(tail).strip_prefix("b/").map(str::to_owned);
    }
    let inner = rest.strip_prefix("a/")?;
    if inner.len() >= 3 && (inner.len() - 3).is_multiple_of(2) {
        let half = (inner.len() - 3) / 2;
        if inner.is_char_boundary(half)
            && inner.is_char_boundary(half + 3)
            && &inner[half..half + 3] == " b/"
            && inner[..half] == inner[half + 3..]
        {
            return Some(inner[..half].to_owned());
        }
    }
    inner.rsplit_once(" b/").map(|(_, right)| unquote_git_path(right))
}

fn unquote_git_path(raw: &str) -> String {
    let Some(inner) = raw.strip_prefix('"').and_then(|s| s.strip_suffix('"')) else {
        return raw.to_owned();
    };
    let bytes = inner.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() {
            match bytes[i + 1] {
                b'n' => {
                    out.push(b'\n');
                    i += 2;
                }
                b'r' => {
                    out.push(b'\r');
                    i += 2;
                }
                b't' => {
                    out.push(b'\t');
                    i += 2;
                }
                b'0'..=b'7' if i + 3 < bytes.len() => {
                    if let Ok(val) = u8::from_str_radix(&inner[i + 1..i + 4], 8) {
                        out.push(val);
                        i += 4;
                    } else {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
                other => {
                    out.push(other);
                    i += 2;
                }
            }
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}
