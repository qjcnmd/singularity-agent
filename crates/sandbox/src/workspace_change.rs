//! Trusted, bounded workspace snapshots used to bind mutating commands to verification.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::io::Read;
use std::path::Path;
use std::time::UNIX_EPOCH;

#[cfg(unix)]
use std::os::unix::ffi::OsStrExt as _;
#[cfg(windows)]
use std::os::windows::ffi::OsStrExt as _;

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
const MAX_SNAPSHOT_DEPTH: usize = 256;
const MAX_SNAPSHOT_FILE_BYTES: u64 = 512 * 1024 * 1024;
const MAX_CHANGED_FILES: usize = 64;
const MAX_CHANGED_PATH_CHARS: usize = 512;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
enum SnapshotEntry {
    Directory {
        metadata: EntryMetadata,
    },
    File {
        metadata: EntryMetadata,
        content_digest: String,
    },
    Symlink {
        metadata: EntryMetadata,
        target_digest: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct EntryMetadata {
    object_kind: u8,
    length: u64,
    readonly: bool,
    platform_permissions: u64,
    modified_seconds: u64,
    modified_nanos: u32,
    device: u64,
    inode: u64,
    links: u64,
}

/// A capability-relative snapshot that contains only bounded metadata and content digests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct WorkspaceSnapshot {
    root: EntryMetadata,
    entries: BTreeMap<String, SnapshotEntry>,
    protected_entries: BTreeMap<String, EntryMetadata>,
}

impl WorkspaceSnapshot {
    /// Compare two trusted observations and produce the final changed paths and diff digest.
    pub(super) fn change_summary(
        &self,
        after: &Self,
    ) -> Result<Option<WorkspaceChangeSummary>, String> {
        if (self.root.device, self.root.inode) != (after.root.device, after.root.inode) {
            return Err("workspace root identity changed between trusted observations".to_string());
        }
        if self.protected_entries != after.protected_entries {
            return Err(
                "protected workspace state changed between trusted observations".to_string(),
            );
        }
        let mut changed_files = self
            .entries
            .keys()
            .chain(after.entries.keys())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .filter(|path| self.entries.get(*path) != after.entries.get(*path))
            .cloned()
            .collect::<Vec<_>>();
        if self.root != after.root {
            changed_files.push(".".to_string());
        }
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
        let encoded = serde_json::to_vec(&(self.root.clone(), after.root.clone(), changed_entries))
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
    let root_before = root
        .dir_metadata()
        .and_then(|metadata| entry_metadata(&metadata))
        .map_err(|error| format!("workspace root metadata failed: {error}"))?;
    let mut state = SnapshotState {
        entries: BTreeMap::new(),
        protected_entries: BTreeMap::new(),
        visited_entries: 0,
        total_file_bytes: 0,
    };
    visit_directory(&root, Path::new(""), 0, &mut state)?;
    let root_after = root
        .dir_metadata()
        .and_then(|metadata| entry_metadata(&metadata))
        .map_err(|error| format!("workspace root revalidation failed: {error}"))?;
    if root_before != root_after {
        return Err("workspace changed while its root snapshot was being captured".to_string());
    }
    Ok(WorkspaceSnapshot {
        root: root_before,
        entries: state.entries,
        protected_entries: state.protected_entries,
    })
}

struct SnapshotState {
    entries: BTreeMap<String, SnapshotEntry>,
    protected_entries: BTreeMap<String, EntryMetadata>,
    visited_entries: usize,
    total_file_bytes: u64,
}

fn visit_directory(
    directory: &Dir,
    relative_parent: &Path,
    depth: usize,
    state: &mut SnapshotState,
) -> Result<(), String> {
    if depth > MAX_SNAPSHOT_DEPTH {
        return Err("workspace change snapshot exceeds the depth bound".to_string());
    }
    let mut entries = directory
        .entries()
        .map_err(|error| format!("workspace change snapshot enumeration failed: {error}"))?
        .map(|entry| {
            state.visited_entries = state.visited_entries.saturating_add(1);
            if state.visited_entries > MAX_SNAPSHOT_ENTRIES {
                return Err(std::io::Error::other(
                    "workspace change snapshot exceeds the entry bound",
                ));
            }
            entry
        })
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("workspace change snapshot enumeration failed: {error}"))?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let name = entry.file_name();
        let name_text = name
            .to_str()
            .ok_or_else(|| "workspace change snapshot contains a non-Unicode path".to_string())?;
        let relative = relative_parent.join(&name);
        let relative_text = workspace_relative_path(&relative)?;
        if is_protected_path(name_text) || is_protected_path(&relative_text) {
            let metadata = directory
                .symlink_metadata(&name)
                .and_then(|metadata| entry_metadata(&metadata))
                .map_err(|error| format!("workspace protected-path metadata failed: {error}"))?;
            state.protected_entries.insert(relative_text, metadata);
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
            let before = child
                .dir_metadata()
                .and_then(|metadata| entry_metadata(&metadata))
                .map_err(|error| format!("workspace change directory metadata failed: {error}"))?;
            visit_directory(&child, &relative, depth + 1, state)?;
            let after = child
                .dir_metadata()
                .and_then(|metadata| entry_metadata(&metadata))
                .map_err(|error| {
                    format!("workspace change directory revalidation failed: {error}")
                })?;
            let path_after = directory
                .symlink_metadata(&name)
                .and_then(|metadata| entry_metadata(&metadata))
                .map_err(|error| format!("workspace change path revalidation failed: {error}"))?;
            if before != after || before != path_after {
                return Err(
                    "workspace changed while its directory snapshot was being captured".to_string(),
                );
            }
            state
                .entries
                .insert(relative_text, SnapshotEntry::Directory { metadata: before });
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
    let metadata_before = directory
        .symlink_metadata(name)
        .and_then(|metadata| entry_metadata(&metadata))
        .map_err(|error| format!("workspace change link metadata failed: {error}"))?;
    let first = directory
        .read_link(name)
        .map_err(|error| format!("workspace change link read failed: {error}"))?;
    let second = directory
        .read_link(name)
        .map_err(|error| format!("workspace change link revalidation failed: {error}"))?;
    if first != second {
        return Err("workspace changed while its link snapshot was being captured".to_string());
    }
    let metadata_after = directory
        .symlink_metadata(name)
        .and_then(|metadata| entry_metadata(&metadata))
        .map_err(|error| format!("workspace change link revalidation failed: {error}"))?;
    if metadata_before != metadata_after {
        return Err("workspace changed while its link snapshot was being captured".to_string());
    }
    Ok(SnapshotEntry::Symlink {
        metadata: metadata_before,
        target_digest: hash_os_str(first.as_os_str()),
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
        .and_then(|metadata| entry_metadata(&metadata))
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
        .and_then(|metadata| entry_metadata(&metadata))
        .map_err(|error| format!("workspace change file revalidation failed: {error}"))?;
    let path_after = directory
        .symlink_metadata(name)
        .and_then(|metadata| entry_metadata(&metadata))
        .map_err(|error| format!("workspace change path revalidation failed: {error}"))?;
    if before != after || before != path_after {
        return Err("workspace changed while its file snapshot was being captured".to_string());
    }
    Ok(SnapshotEntry::File {
        metadata: before,
        content_digest: format!("sha256:{:x}", hasher.finalize()),
    })
}

fn entry_metadata(metadata: &Metadata) -> std::io::Result<EntryMetadata> {
    let modified = metadata.modified()?.into_std();
    let modified = modified
        .duration_since(UNIX_EPOCH)
        .map_err(|_| std::io::Error::other("workspace file timestamp predates the epoch"))?;
    Ok(EntryMetadata {
        object_kind: if metadata.is_symlink() {
            3
        } else if metadata.is_dir() {
            2
        } else if metadata.is_file() {
            1
        } else {
            return Err(std::io::Error::other(
                "workspace snapshot found an unsupported object type",
            ));
        },
        length: metadata.len(),
        readonly: metadata.permissions().readonly(),
        platform_permissions: platform_permissions(metadata),
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
    #[cfg(windows)]
    return Ok(text.replace('\\', "/"));
    #[cfg(not(windows))]
    Ok(text.to_string())
}

#[cfg(unix)]
fn platform_permissions(metadata: &Metadata) -> u64 {
    u64::from(cap_std::fs::PermissionsExt::mode(&metadata.permissions()))
}

#[cfg(windows)]
fn platform_permissions(metadata: &Metadata) -> u64 {
    u64::from(cap_std::fs::MetadataExt::file_attributes(metadata))
}

#[cfg(unix)]
fn hash_os_str(value: &OsStr) -> String {
    format!("sha256:{:x}", Sha256::digest(value.as_bytes()))
}

#[cfg(windows)]
fn hash_os_str(value: &OsStr) -> String {
    let mut hasher = Sha256::new();
    for unit in value.encode_wide() {
        hasher.update(unit.to_le_bytes());
    }
    format!("sha256:{:x}", hasher.finalize())
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

    #[test]
    fn protected_path_creation_fails_closed_without_exposing_its_name() {
        let workspace = tempfile::tempdir().expect("workspace");
        let before = snapshot_workspace(workspace.path()).expect("before snapshot");
        std::fs::write(workspace.path().join(".env"), b"opaque").expect("protected file");
        let after = snapshot_workspace(workspace.path()).expect("after snapshot");

        assert_eq!(
            before.change_summary(&after).expect_err("must fail closed"),
            "protected workspace state changed between trusted observations"
        );
    }

    #[cfg(unix)]
    #[test]
    fn unix_backslash_component_does_not_collide_with_path_separator() {
        let workspace = tempfile::tempdir().expect("workspace");
        std::fs::write(workspace.path().join("a\\b"), b"flat").expect("flat file");
        std::fs::create_dir(workspace.path().join("a")).expect("nested directory");
        std::fs::write(workspace.path().join("a").join("b"), b"nested").expect("nested file");
        let before = snapshot_workspace(workspace.path()).expect("before snapshot");

        std::fs::write(workspace.path().join("a\\b"), b"changed").expect("change flat file");
        let after_flat = snapshot_workspace(workspace.path()).expect("flat snapshot");
        assert_eq!(
            before
                .change_summary(&after_flat)
                .expect("flat summary")
                .expect("flat changed")
                .changed_files,
            ["a\\b"]
        );

        std::fs::write(workspace.path().join("a\\b"), b"flat").expect("restore flat file");
        std::fs::write(workspace.path().join("a").join("b"), b"changed")
            .expect("change nested file");
        let after_nested = snapshot_workspace(workspace.path()).expect("nested snapshot");
        assert_eq!(
            before
                .change_summary(&after_nested)
                .expect("nested summary")
                .expect("nested changed")
                .changed_files,
            ["a/b"]
        );
    }

    #[cfg(unix)]
    #[test]
    fn unix_symlink_digest_preserves_non_unicode_target_bytes() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt as _;
        use std::os::unix::fs::symlink;

        let workspace = tempfile::tempdir().expect("workspace");
        let link = workspace.path().join("link");
        symlink(OsString::from_vec(vec![0xff]), &link).expect("first link");
        let before = snapshot_workspace(workspace.path()).expect("before snapshot");
        std::fs::remove_file(&link).expect("remove first link");
        symlink(OsString::from_vec(vec![0xfe]), &link).expect("second link");
        let after = snapshot_workspace(workspace.path()).expect("after snapshot");

        assert!(
            before
                .change_summary(&after)
                .expect("summary")
                .expect("target changed")
                .changed_files
                .contains(&"link".to_string())
        );
    }

    #[cfg(unix)]
    #[test]
    fn unix_directory_identity_and_permissions_are_observed() {
        use std::os::unix::fs::PermissionsExt as _;

        let workspace = tempfile::tempdir().expect("workspace");
        let first = workspace.path().join("first");
        let second = workspace.path().join("second");
        std::fs::create_dir(&first).expect("first directory");
        std::fs::create_dir(&second).expect("second directory");
        let before = snapshot_workspace(workspace.path()).expect("before snapshot");
        let temporary = workspace.path().join("temporary");
        std::fs::rename(&first, &temporary).expect("move first");
        std::fs::rename(&second, &first).expect("move second");
        std::fs::rename(&temporary, &second).expect("move temporary");
        let swapped = snapshot_workspace(workspace.path()).expect("swapped snapshot");
        let swapped_paths = before
            .change_summary(&swapped)
            .expect("swapped summary")
            .expect("identity changed")
            .changed_files;
        assert!(swapped_paths.contains(&"first".to_string()));
        assert!(swapped_paths.contains(&"second".to_string()));

        let mut permissions = std::fs::metadata(&first)
            .expect("directory metadata")
            .permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&first, permissions).expect("chmod directory");
        let chmod = snapshot_workspace(workspace.path()).expect("chmod snapshot");
        assert!(
            swapped
                .change_summary(&chmod)
                .expect("chmod summary")
                .expect("permissions changed")
                .changed_files
                .contains(&"first".to_string())
        );
    }
}
