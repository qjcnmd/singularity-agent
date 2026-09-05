//! 有界本机目录选择与已登记 Workspace 内文件候选。

use std::path::Path;

use serde::Serialize;
use singularity_protocol::{DirectoryEntry, DirectoryEntryKind, Workspace};

const MAX_SCANNED_DIRECTORIES: usize = 2_000;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileCandidate {
    pub path: String,
    pub kind: DirectoryEntryKind,
}

pub fn list_directory(path: Option<&str>) -> Result<Vec<DirectoryEntry>, String> {
    let Some(path) = path else {
        return Ok(system_roots());
    };
    let directory =
        singularity_core::canonicalize_workspace(path).map_err(|error| error.to_string())?;
    let mut entries = Vec::new();
    if let Some(parent) = directory.as_path().parent() {
        entries.push(DirectoryEntry {
            name: "..".to_string(),
            path: display_existing_path(parent)?,
            kind: DirectoryEntryKind::Parent,
        });
    }
    for entry in std::fs::read_dir(directory.as_path())
        .map_err(|error| format!("directory could not be read: {error}"))?
    {
        let Ok(entry) = entry else {
            continue;
        };
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() && !file_type.is_symlink() {
            let path = entry.path();
            let Ok(path) = display_existing_path(&path) else {
                continue;
            };
            entries.push(DirectoryEntry {
                name: entry.file_name().to_string_lossy().into_owned(),
                path,
                kind: DirectoryEntryKind::Directory,
            });
        }
    }
    entries.sort_by(|left, right| {
        (left.kind != DirectoryEntryKind::Parent)
            .cmp(&(right.kind != DirectoryEntryKind::Parent))
            .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
            .then_with(|| left.path.cmp(&right.path))
    });
    Ok(entries)
}

pub fn search_files(
    workspace: &Workspace,
    query: &str,
    limit: usize,
) -> Result<Vec<FileCandidate>, String> {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return Ok(Vec::new());
    }
    let limit = limit.clamp(1, 100);
    let root = singularity_core::canonicalize_workspace(&workspace.root)
        .map_err(|error| error.to_string())?;
    let mut pending = vec![root.as_path().to_path_buf()];
    let mut scanned = 0;
    let mut candidates = Vec::new();
    while let Some(directory) = pending.pop() {
        if scanned >= MAX_SCANNED_DIRECTORIES || candidates.len() >= limit {
            break;
        }
        scanned += 1;
        let mut entries = std::fs::read_dir(&directory)
            .map_err(|error| format!("workspace directory could not be read: {error}"))?
            .filter_map(Result::ok)
            .collect::<Vec<_>>();
        entries.sort_by_key(|entry| entry.file_name().to_string_lossy().to_lowercase());
        for entry in entries {
            if candidates.len() >= limit {
                break;
            }
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(_) => continue,
            };
            if file_type.is_symlink() {
                continue;
            }
            let path = entry.path();
            if file_type.is_dir() {
                if !is_ignored_directory(&entry.file_name().to_string_lossy()) {
                    pending.push(path);
                }
                continue;
            }
            if !file_type.is_file() {
                continue;
            }
            let Ok(relative) = path.strip_prefix(root.as_path()) else {
                continue;
            };
            let relative = relative.to_string_lossy().replace('\\', "/");
            if relative.to_lowercase().contains(&query) {
                candidates.push(FileCandidate {
                    path: relative,
                    kind: DirectoryEntryKind::File,
                });
            }
        }
    }
    candidates.sort_by(|left, right| {
        left.path
            .to_lowercase()
            .cmp(&right.path.to_lowercase())
            .then_with(|| left.path.cmp(&right.path))
    });
    candidates.truncate(limit);
    Ok(candidates)
}

fn is_ignored_directory(name: &str) -> bool {
    matches!(name, ".git" | "node_modules" | "target")
}

fn display_existing_path(path: &Path) -> Result<String, String> {
    singularity_core::canonicalize_workspace(path)
        .map(|path| path.display().to_string())
        .map_err(|error| error.to_string())
}

fn system_roots() -> Vec<DirectoryEntry> {
    #[cfg(windows)]
    {
        (b'A'..=b'Z')
            .map(|letter| format!("{}:/", letter as char))
            .filter(|path| Path::new(path).is_dir())
            .map(|path| DirectoryEntry {
                name: path.clone(),
                path,
                kind: DirectoryEntryKind::Root,
            })
            .collect()
    }
    #[cfg(not(windows))]
    {
        vec![DirectoryEntry {
            name: "/".to_string(),
            path: "/".to_string(),
            kind: DirectoryEntryKind::Root,
        }]
    }
}
