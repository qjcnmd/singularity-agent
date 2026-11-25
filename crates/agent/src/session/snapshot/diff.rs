use std::collections::BTreeMap;
use std::io;
use std::path::Path;

use protocol::{ChangedFile, ChangedFileStatus};

use super::{InitPolicy, Repo, split_nul, validate_relative_path, validate_tree_hash};

const MAX_DIFF_BYTES: usize = 200 * 1024;
const MAX_DIFF_LINES: usize = 2_000;

pub fn diff_summary(snapshot_dir: &Path, cwd: &Path, base_tree: &str) -> io::Result<Vec<ChangedFile>> {
    let base = validate_tree_hash(base_tree)?;
    let repo = Repo { dir: snapshot_dir, cwd };
    repo.with_live_tree(InitPolicy::RequireExisting, |live, _| {
        let status = repo.diff(base, &live, &["--name-status", "-z"], &[])?;
        let numstat = repo.diff(base, &live, &["--numstat", "-z"], &[])?;
        Ok(parse_diff_summary(&status, &numstat))
    })
}

pub fn file_diff(snapshot_dir: &Path, cwd: &Path, base_tree: &str, rel_path: &str) -> io::Result<String> {
    let base = validate_tree_hash(base_tree)?;
    let rel = validate_relative_path(rel_path)?;
    let repo = Repo { dir: snapshot_dir, cwd };
    repo.with_live_tree(InitPolicy::RequireExisting, |live, _| {
        let flags = &["--no-color", "-U3", "--no-textconv"];
        let mut out = repo.diff(base, &live, flags, &["--", &rel])?;
        #[cfg(windows)]
        if out.is_empty() {
            let names = repo.diff(base, &live, &["--name-only", "-z"], &[])?;
            let target = rel.to_lowercase();
            if let Some(matched) =
                split_nul(&names).map(|p| String::from_utf8_lossy(p)).find(|p| p.to_lowercase() == target)
            {
                out = repo.diff(base, &live, flags, &["--", &matched])?;
            }
        }
        Ok(format_git_diff(&String::from_utf8_lossy(&out)))
    })
}

fn parse_diff_summary(status_out: &[u8], numstat_out: &[u8]) -> Vec<ChangedFile> {
    let mut files = BTreeMap::new();
    let mut fields = split_nul(status_out);
    while let (Some(status_raw), Some(path_raw)) = (fields.next(), fields.next()) {
        let status = match status_raw.first().copied() {
            Some(b'A') => ChangedFileStatus::Added,
            Some(b'D') => ChangedFileStatus::Deleted,
            _ => ChangedFileStatus::Modified,
        };
        let path = String::from_utf8_lossy(path_raw).into_owned();
        files.insert(path.clone(), ChangedFile { path, added: 0, deleted: 0, status });
    }
    let parse_count = |s: &str| s.parse::<u64>().ok().map_or(0, |n| u32::try_from(n).unwrap_or(u32::MAX));
    for entry in split_nul(numstat_out) {
        let text = String::from_utf8_lossy(entry);
        let mut parts = text.splitn(3, '\t');
        if let (Some(added), Some(deleted), Some(path)) = (parts.next(), parts.next(), parts.next())
            && let Some(file) = files.get_mut(path)
        {
            file.added = parse_count(added);
            file.deleted = parse_count(deleted);
        }
    }
    files.into_values().collect()
}

fn format_git_diff(raw: &str) -> String {
    if raw.trim().is_empty() {
        return "该文件当前无改动。".to_owned();
    }
    let lines: Vec<&str> = raw.lines().collect();
    let first_hunk = lines.iter().position(|line| line.starts_with("@@ "));
    let header = &lines[..first_hunk.unwrap_or(lines.len())];
    if header
        .iter()
        .any(|l| (l.starts_with("Binary files ") && l.ends_with(" differ")) || *l == "GIT binary patch")
    {
        return "二进制文件已变更，暂不支持文本差异对比。".to_owned();
    }
    let Some(start) = first_hunk else {
        return "无文本内容差异（空文件或文件模式变更）。".to_owned();
    };
    let hunks = &lines[start..];
    let added = u32::try_from(hunks.iter().filter(|l| l.starts_with('+')).count()).unwrap_or(u32::MAX);
    let deleted = u32::try_from(hunks.iter().filter(|l| l.starts_with('-')).count()).unwrap_or(u32::MAX);
    let mut output = format!("+{added} -{deleted}\n");
    for (index, line) in hunks.iter().enumerate() {
        if index >= MAX_DIFF_LINES || output.len() + line.len() + 1 > MAX_DIFF_BYTES {
            output.push_str("[Diff truncated]\n");
            break;
        }
        output.push_str(line);
        output.push('\n');
    }
    output
}
