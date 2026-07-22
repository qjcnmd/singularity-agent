//! Trusted, bounded workspace snapshots used to bind mutating commands to verification.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::Path;
use std::time::UNIX_EPOCH;

#[cfg(windows)]
use cap_fs_ext::DirEntryExt as _;
use cap_fs_ext::{DirExt as _, FollowSymlinks, MetadataExt as _, OpenOptionsFollowExt as _};
use cap_std::ambient_authority;
use cap_std::fs::{Dir, Metadata, OpenOptions};
use serde::Serialize;
use sha2::{Digest, Sha256};
use singularity_core::is_protected_path;

use super::WorkspaceChangeSummary;

const MAX_SNAPSHOT_ENTRIES: usize = 100_000;
const MAX_SNAPSHOT_FILE_BYTES: u64 = 512 * 1024 * 1024;
const MAX_CHANGED_FILES: usize = 64;
const MAX_CHANGED_PATH_CHARS: usize = 512;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
enum SnapshotEntry {
    Directory,
    File {
        metadata: FileMetadata,
        content_digest: String,
    },
    Symlink {
        target_digest: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct FileMetadata {
    length: u64,
    readonly: bool,
    modified_seconds: u64,
    modified_nanos: u32,
    device: u64,
    inode: u64,
    links: u64,
}

/// A capability-relative snapshot that contains only bounded metadata and content digests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct WorkspaceSnapshot {
    entries: BTreeMap<String, SnapshotEntry>,
}

impl WorkspaceSnapshot {
    /// Compare two trusted observations and produce the final changed paths and diff digest.
    pub(super) fn change_summary(
        &self,
        after: &Self,
    ) -> Result<Option<WorkspaceChangeSummary>, String> {
        let changed_files = self
            .entries
            .keys()
            .chain(after.entries.keys())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .filter(|path| self.entries.get(*path) != after.entries.get(*path))
            .cloned()
            .collect::<Vec<_>>();
        if changed_files.is_empty() {
            return Ok(None);
        }
        if changed_files.len() > MAX_CHANGED_FILES
            || changed_files
                .iter()
                .any(|path| path.chars().count() > MAX_CHANGED_PATH_CHARS)
        {
            return Err("workspace change exceeds the bounded verification scope".to_string());
        }
        let changed_entries = changed_files
            .iter()
            .map(|path| (path, self.entries.get(path), after.entries.get(path)))
            .collect::<Vec<_>>();
        let encoded = serde_json::to_vec(&changed_entries)
            .map_err(|error| format!("workspace change summary encoding failed: {error}"))?;
        Ok(Some(WorkspaceChangeSummary::new(
            changed_files,
            format!("sha256:{:x}", Sha256::digest(encoded)),
        )))
    }
}

/// Snapshot the workspace through a directory capability without following links.
pub(super) fn snapshot_workspace(workspace: &Path) -> Result<WorkspaceSnapshot, String> {
    let root = Dir::open_ambient_dir(workspace, ambient_authority())
        .map_err(|error| format!("workspace change snapshot is unavailable: {error}"))?;
    let mut state = SnapshotState {
        entries: BTreeMap::new(),
        total_file_bytes: 0,
    };
    visit_directory(&root, Path::new(""), &mut state)?;
    Ok(WorkspaceSnapshot {
        entries: state.entries,
    })
}

struct SnapshotState {
    entries: BTreeMap<String, SnapshotEntry>,
    total_file_bytes: u64,
}

fn visit_directory(
    directory: &Dir,
    relative_parent: &Path,
    state: &mut SnapshotState,
) -> Result<(), String> {
    let mut entries = directory
        .entries()
        .map_err(|error| format!("workspace change snapshot enumeration failed: {error}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("workspace change snapshot enumeration failed: {error}"))?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        if state.entries.len() >= MAX_SNAPSHOT_ENTRIES {
            return Err("workspace change snapshot exceeds the entry bound".to_string());
        }
        let name = entry.file_name();
        let name_text = name
            .to_str()
            .ok_or_else(|| "workspace change snapshot contains a non-Unicode path".to_string())?;
        let relative = relative_parent.join(&name);
        let relative_text = workspace_relative_path(&relative)?;
        if is_protected_path(name_text) || is_protected_path(&relative_text) {
            continue;
        }
        let file_type = entry
            .file_type()
            .map_err(|error| format!("workspace change snapshot type failed: {error}"))?;
        #[cfg(windows)]
        let is_link = file_type.is_symlink()
            || entry
                .full_metadata()
                .map_err(|error| format!("workspace change snapshot metadata failed: {error}"))?
                .is_symlink();
        #[cfg(not(windows))]
        let is_link = file_type.is_symlink();

        let snapshot_entry = if is_link {
            snapshot_symlink(directory, Path::new(&name))?
        } else if file_type.is_dir() {
            let child = directory
                .open_dir_nofollow(&name)
                .map_err(|error| format!("workspace change directory open failed: {error}"))?;
            state
                .entries
                .insert(relative_text.clone(), SnapshotEntry::Directory);
            visit_directory(&child, &relative, state)?;
            continue;
        } else if file_type.is_file() {
            snapshot_file(directory, Path::new(&name), state)?
        } else {
            return Err(format!(
                "workspace change snapshot found an unsupported object: {relative_text}"
            ));
        };
        state.entries.insert(relative_text, snapshot_entry);
    }
    Ok(())
}

fn snapshot_symlink(directory: &Dir, name: &Path) -> Result<SnapshotEntry, String> {
    let first = directory
        .read_link(name)
        .map_err(|error| format!("workspace change link read failed: {error}"))?;
    let second = directory
        .read_link(name)
        .map_err(|error| format!("workspace change link revalidation failed: {error}"))?;
    if first != second {
        return Err("workspace changed while its link snapshot was being captured".to_string());
    }
    Ok(SnapshotEntry::Symlink {
        target_digest: format!(
            "sha256:{:x}",
            Sha256::digest(first.as_os_str().to_string_lossy().as_bytes())
        ),
    })
}

fn snapshot_file(
    directory: &Dir,
    name: &Path,
    state: &mut SnapshotState,
) -> Result<SnapshotEntry, String> {
    let mut options = OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    let mut file = directory
        .open_with(name, &options)
        .map_err(|error| format!("workspace change file open failed: {error}"))?;
    let before = file
        .metadata()
        .and_then(|metadata| file_metadata(&metadata))
        .map_err(|error| format!("workspace change file metadata failed: {error}"))?;
    state.total_file_bytes = state
        .total_file_bytes
        .checked_add(before.length)
        .ok_or_else(|| "workspace change snapshot size overflowed".to_string())?;
    if state.total_file_bytes > MAX_SNAPSHOT_FILE_BYTES {
        return Err("workspace change snapshot exceeds the content bound".to_string());
    }
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| format!("workspace change file read failed: {error}"))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let after = file
        .metadata()
        .and_then(|metadata| file_metadata(&metadata))
        .map_err(|error| format!("workspace change file revalidation failed: {error}"))?;
    let path_after = directory
        .symlink_metadata(name)
        .and_then(|metadata| file_metadata(&metadata))
        .map_err(|error| format!("workspace change path revalidation failed: {error}"))?;
    if before != after || before != path_after {
        return Err("workspace changed while its file snapshot was being captured".to_string());
    }
    Ok(SnapshotEntry::File {
        metadata: before,
        content_digest: format!("sha256:{:x}", hasher.finalize()),
    })
}

fn file_metadata(metadata: &Metadata) -> std::io::Result<FileMetadata> {
    if !metadata.is_file() || metadata.is_symlink() {
        return Err(std::io::Error::other(
            "workspace snapshot object changed type",
        ));
    }
    let modified = metadata.modified()?.into_std();
    let modified = modified
        .duration_since(UNIX_EPOCH)
        .map_err(|_| std::io::Error::other("workspace file timestamp predates the epoch"))?;
    Ok(FileMetadata {
        length: metadata.len(),
        readonly: metadata.permissions().readonly(),
        modified_seconds: modified.as_secs(),
        modified_nanos: modified.subsec_nanos(),
        device: metadata.dev(),
        inode: metadata.ino(),
        links: metadata.nlink(),
    })
}

fn workspace_relative_path(path: &Path) -> Result<String, String> {
    let text = path
        .to_str()
        .ok_or_else(|| "workspace change snapshot contains a non-Unicode path".to_string())?;
    Ok(text.replace('\\', "/"))
}

#[cfg(test)]
mod tests {
    use super::snapshot_workspace;

    #[test]
    fn summary_binds_changed_path_and_before_after_content() {
        let workspace = tempfile::tempdir().expect("workspace");
        let path = workspace.path().join("value.txt");
        std::fs::write(&path, b"before").expect("write before");
        let before = snapshot_workspace(workspace.path()).expect("before snapshot");
        std::fs::write(&path, b"after").expect("write after");
        let after = snapshot_workspace(workspace.path()).expect("after snapshot");

        let summary = before
            .change_summary(&after)
            .expect("change summary")
            .expect("changed");

        assert_eq!(summary.changed_files, ["value.txt"]);
        assert!(summary.diff_digest.starts_with("sha256:"));
        assert_eq!(summary.diff_digest.len(), "sha256:".len() + 64);

        std::fs::write(&path, b"different after").expect("write alternate after");
        let alternate = snapshot_workspace(workspace.path()).expect("alternate snapshot");
        let alternate = before
            .change_summary(&alternate)
            .expect("alternate summary")
            .expect("alternate changed");
        assert_ne!(summary.diff_digest, alternate.diff_digest);
    }

    #[test]
    fn unchanged_snapshot_has_no_summary() {
        let workspace = tempfile::tempdir().expect("workspace");
        std::fs::write(workspace.path().join("value.txt"), b"stable").expect("write file");
        let before = snapshot_workspace(workspace.path()).expect("before snapshot");
        let after = snapshot_workspace(workspace.path()).expect("after snapshot");

        assert_eq!(before.change_summary(&after).expect("summary"), None);
    }
}
