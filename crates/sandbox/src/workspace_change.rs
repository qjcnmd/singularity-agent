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

/// Return whether a relative workspace path belongs to the closed toolchain artifact set.
///
/// This classifier is intentionally conservative: paths outside the known set return `false`
/// and therefore remain verification-relevant. Callers must only treat a diff as artifact-only
/// when every path is classified here and was absent from the before snapshot.
pub fn is_toolchain_artifact_path(path: &str) -> bool {
    if path.is_empty()
        || path.contains('\\')
        || path
            .split('/')
            .any(|segment| segment.is_empty() || matches!(segment, "." | ".."))
        || path.split('/').any(|segment| segment.contains(':'))
    {
        return false;
    }
    let segments = path.split('/').collect::<Vec<_>>();
    if segments.iter().any(|segment| {
        matches!(
            *segment,
            "target"
                | "__pycache__"
                | ".pytest_cache"
                | ".mypy_cache"
                | ".ruff_cache"
                | ".hypothesis"
                | ".tox"
                | ".nox"
                | ".cache"
                | ".next"
                | ".nuxt"
                | ".svelte-kit"
                | ".parcel-cache"
                | ".turbo"
                | ".vite"
                | "coverage"
                | "htmlcov"
        )
    }) {
        return true;
    }

    segments
        .iter()
        .any(|segment| segment.ends_with(".egg-info"))
        || segments.last().is_some_and(|file| {
            file.ends_with(".pyc")
                || file.ends_with(".pyo")
                || *file == ".coverage"
                || file.starts_with(".coverage.")
                || *file == ".eslintcache"
                || *file == ".stylelintcache"
        })
}

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
        if !protected_entries_match(&self.protected_entries, &after.protected_entries) {
            return Err(
                "protected workspace state changed between trusted observations".to_string(),
            );
        }
        let protected_paths = self
            .protected_entries
            .keys()
            .chain(after.protected_entries.keys())
            .cloned()
            .collect::<BTreeSet<_>>();
        let mut changed_files = self
            .entries
            .keys()
            .chain(after.entries.keys())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .filter(|path| {
                !snapshot_entry_matches(
                    self.entries.get(*path),
                    after.entries.get(*path),
                    path,
                    &protected_paths,
                )
            })
            .cloned()
            .collect::<Vec<_>>();
        if root_behavior_changed(&self.root, &after.root) {
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
        let verification_relevant =
            workspace_change_is_verification_relevant(self, after, &changed_files);
        Ok(Some(
            WorkspaceChangeSummary::new(
                changed_files,
                format!("sha256:{:x}", Sha256::digest(encoded)),
            )
            .with_verification_relevant(verification_relevant),
        ))
    }

    /// Compare the workspace baseline while ignoring trusted metadata churn on protected
    /// directories. Protected object existence, type, identity and permissions remain
    /// authoritative; volatile directory contents, length and timestamps stay outside the
    /// agent-visible revision.
    #[cfg(unix)]
    pub(super) fn transaction_baseline_matches(&self, after: &Self) -> bool {
        (self.root.device, self.root.inode) == (after.root.device, after.root.inode)
            && !root_behavior_changed(&self.root, &after.root)
            && workspace_entries_match(
                &self.entries,
                &after.entries,
                &self.protected_entries,
                &after.protected_entries,
            )
            && protected_entries_match(&self.protected_entries, &after.protected_entries)
    }
}

fn workspace_change_is_verification_relevant(
    before: &WorkspaceSnapshot,
    after: &WorkspaceSnapshot,
    changed_files: &[String],
) -> bool {
    changed_files.iter().any(|path| {
        if is_toolchain_artifact_path(path) {
            return before.entries.contains_key(path);
        }
        !is_incidental_artifact_ancestor_change(before, after, changed_files, path)
    })
}

fn is_incidental_artifact_ancestor_change(
    before: &WorkspaceSnapshot,
    after: &WorkspaceSnapshot,
    changed_files: &[String],
    path: &str,
) -> bool {
    let (
        Some(SnapshotEntry::Directory {
            metadata: before_metadata,
        }),
        Some(SnapshotEntry::Directory {
            metadata: after_metadata,
        }),
    ) = (before.entries.get(path), after.entries.get(path))
    else {
        return false;
    };
    if !directory_behavior_matches(before_metadata, after_metadata) {
        return false;
    }
    let prefix = format!("{path}/");
    let mut descendants = changed_files
        .iter()
        .filter(|changed| changed.starts_with(&prefix));
    descendants.next().is_some()
        && descendants.all(|changed| {
            is_toolchain_artifact_path(changed) && !before.entries.contains_key(changed)
        })
}

/// Compare directory identity and security behavior while allowing filesystem bookkeeping churn
/// caused by the newly-created artifact descendants (length, timestamps and link count).
fn directory_behavior_matches(before: &EntryMetadata, after: &EntryMetadata) -> bool {
    before.object_kind == after.object_kind
        && before.readonly == after.readonly
        && before.platform_permissions == after.platform_permissions
        && before.device == after.device
        && before.inode == after.inode
}

#[cfg(unix)]
fn workspace_entries_match(
    before: &BTreeMap<String, SnapshotEntry>,
    after: &BTreeMap<String, SnapshotEntry>,
    before_protected: &BTreeMap<String, EntryMetadata>,
    after_protected: &BTreeMap<String, EntryMetadata>,
) -> bool {
    if before.keys().ne(after.keys()) {
        return false;
    }
    let protected_paths = before_protected
        .keys()
        .chain(after_protected.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    before.iter().all(|(path, entry)| {
        snapshot_entry_matches(Some(entry), after.get(path), path, &protected_paths)
    })
}

fn snapshot_entry_matches(
    before: Option<&SnapshotEntry>,
    after: Option<&SnapshotEntry>,
    path: &str,
    protected_paths: &BTreeSet<String>,
) -> bool {
    match (before, after) {
        (
            Some(SnapshotEntry::Directory { metadata: before }),
            Some(SnapshotEntry::Directory { metadata: after }),
        ) if protected_paths
            .iter()
            .any(|protected| protected.starts_with(&format!("{path}/"))) =>
        {
            protected_metadata_matches(before, after)
        }
        _ => before == after,
    }
}

fn protected_entries_match(
    before: &BTreeMap<String, EntryMetadata>,
    after: &BTreeMap<String, EntryMetadata>,
) -> bool {
    before.len() == after.len()
        && before.iter().all(|(path, metadata)| {
            after
                .get(path)
                .is_some_and(|other| protected_metadata_matches(metadata, other))
        })
}

fn protected_metadata_matches(before: &EntryMetadata, after: &EntryMetadata) -> bool {
    if before.object_kind == 2 && after.object_kind == 2 {
        before.object_kind == after.object_kind
            && before.readonly == after.readonly
            && before.platform_permissions == after.platform_permissions
            && before.device == after.device
            && before.inode == after.inode
    } else {
        before == after
    }
}

/// Snapshot the workspace through a directory capability without following links.
pub(super) fn snapshot_workspace(workspace: &Path) -> Result<WorkspaceSnapshot, String> {
    snapshot_workspace_with_protected_paths(workspace, true)
}

/// Snapshot controller-owned workspace metadata as ordinary transactional content.
pub(super) fn snapshot_trusted_workspace(workspace: &Path) -> Result<WorkspaceSnapshot, String> {
    snapshot_workspace_with_protected_paths(workspace, false)
}

fn snapshot_workspace_with_protected_paths(
    workspace: &Path,
    protect_paths: bool,
) -> Result<WorkspaceSnapshot, String> {
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
    visit_directory(&root, Path::new(""), 0, protect_paths, &mut state)?;
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
    protect_paths: bool,
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
        if protect_paths && (is_protected_path(name_text) || is_protected_path(&relative_text)) {
            #[cfg(not(windows))]
            {
                let metadata = directory
                    .symlink_metadata(&name)
                    .and_then(|metadata| entry_metadata(&metadata))
                    .map_err(|error| {
                        format!("workspace protected-path metadata failed: {error}")
                    })?;
                state.protected_entries.insert(relative_text, metadata);
            }
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
            visit_directory(&child, &relative, depth + 1, protect_paths, state)?;
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

fn root_behavior_changed(before: &EntryMetadata, after: &EntryMetadata) -> bool {
    before.object_kind != after.object_kind
        || before.readonly != after.readonly
        || before.platform_permissions != after.platform_permissions
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
    use super::{is_toolchain_artifact_path, snapshot_trusted_workspace, snapshot_workspace};

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
        assert!(summary.verification_relevant);
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
    fn new_toolchain_artifacts_are_not_verification_relevant() {
        let workspace = tempfile::tempdir().expect("workspace");
        let before = snapshot_workspace(workspace.path()).expect("before snapshot");
        std::fs::create_dir_all(workspace.path().join("target/debug")).expect("target dirs");
        std::fs::write(workspace.path().join("target/debug/app"), b"artifact")
            .expect("target artifact");
        let after = snapshot_workspace(workspace.path()).expect("after snapshot");

        let summary = before
            .change_summary(&after)
            .expect("summary")
            .expect("artifact change");

        assert!(!summary.verification_relevant);
        assert!(
            summary
                .changed_files
                .iter()
                .all(|path| is_toolchain_artifact_path(path))
        );
    }

    #[test]
    fn artifact_classifier_rejects_noncanonical_paths() {
        assert!(!is_toolchain_artifact_path("\\target\\debug\\app"));
        assert!(!is_toolchain_artifact_path("/target/debug/app"));
        assert!(!is_toolchain_artifact_path("target//debug/app"));
        assert!(!is_toolchain_artifact_path("target/../src/lib.rs"));
        assert!(!is_toolchain_artifact_path("C:/target/debug/app"));
    }

    #[test]
    fn source_or_preexisting_or_unknown_changes_remain_verification_relevant() {
        let source_workspace = tempfile::tempdir().expect("source workspace");
        let source_before = snapshot_workspace(source_workspace.path()).expect("source before");
        std::fs::create_dir_all(source_workspace.path().join("target/debug")).expect("target dirs");
        std::fs::write(
            source_workspace.path().join("target/debug/app"),
            b"artifact",
        )
        .expect("target artifact");
        std::fs::create_dir_all(source_workspace.path().join("src")).expect("src dir");
        std::fs::write(source_workspace.path().join("src/lib.rs"), b"source").expect("source file");
        let source_after = snapshot_workspace(source_workspace.path()).expect("source after");
        assert!(
            source_before
                .change_summary(&source_after)
                .expect("source summary")
                .expect("source change")
                .verification_relevant
        );

        let preexisting_workspace = tempfile::tempdir().expect("preexisting workspace");
        std::fs::create_dir_all(preexisting_workspace.path().join("target/debug"))
            .expect("target dirs");
        std::fs::write(
            preexisting_workspace.path().join("target/debug/app"),
            b"before",
        )
        .expect("target artifact");
        let preexisting_before =
            snapshot_workspace(preexisting_workspace.path()).expect("preexisting before");
        std::fs::write(
            preexisting_workspace.path().join("target/debug/app"),
            b"after",
        )
        .expect("modified artifact");
        let preexisting_after =
            snapshot_workspace(preexisting_workspace.path()).expect("preexisting after");
        assert!(
            preexisting_before
                .change_summary(&preexisting_after)
                .expect("preexisting summary")
                .expect("preexisting change")
                .verification_relevant
        );

        let unknown_workspace = tempfile::tempdir().expect("unknown workspace");
        let unknown_before = snapshot_workspace(unknown_workspace.path()).expect("unknown before");
        std::fs::create_dir_all(unknown_workspace.path().join("generated")).expect("generated");
        std::fs::write(
            unknown_workspace.path().join("generated/cache.bin"),
            b"unknown",
        )
        .expect("unknown file");
        let unknown_after = snapshot_workspace(unknown_workspace.path()).expect("unknown after");
        assert!(
            unknown_before
                .change_summary(&unknown_after)
                .expect("unknown summary")
                .expect("unknown change")
                .verification_relevant
        );
    }

    #[test]
    fn nested_new_artifact_under_existing_source_directory_is_incidental_when_safe() {
        use cap_fs_ext::DirExt as _;
        use cap_std::ambient_authority;
        use cap_std::fs::Dir;
        use std::time::{Duration, SystemTime};

        let workspace = tempfile::tempdir().expect("workspace");
        std::fs::create_dir(workspace.path().join("src")).expect("source directory");
        let before = snapshot_workspace(workspace.path()).expect("before snapshot");
        std::fs::create_dir(workspace.path().join("src/__pycache__")).expect("cache directory");
        std::fs::write(workspace.path().join("src/__pycache__/x.pyc"), b"bytecode")
            .expect("cache artifact");
        let source_directory =
            Dir::open_ambient_dir(workspace.path().join("src"), ambient_authority())
                .expect("open source directory");
        source_directory
            .set_times(
                ".",
                None,
                Some(cap_fs_ext::SystemTimeSpec::Absolute(
                    cap_std::time::SystemTime::from_std(
                        SystemTime::now() + Duration::from_secs(10),
                    ),
                )),
            )
            .expect("touch source directory");
        let after = snapshot_workspace(workspace.path()).expect("after snapshot");
        let summary = before
            .change_summary(&after)
            .expect("summary")
            .expect("nested artifact change");

        assert!(summary.changed_files.iter().any(|path| path == "src"));
        assert!(!summary.verification_relevant);
    }

    #[test]
    fn source_directory_behavior_change_with_artifact_remains_relevant() {
        let workspace = tempfile::tempdir().expect("workspace");
        let source = workspace.path().join("src");
        std::fs::create_dir(&source).expect("source directory");
        let before_permissions = std::fs::metadata(&source)
            .expect("source metadata")
            .permissions();
        let before = snapshot_workspace(workspace.path()).expect("before snapshot");
        std::fs::create_dir(source.join("__pycache__")).expect("cache directory");
        std::fs::write(source.join("__pycache__/x.pyc"), b"bytecode").expect("cache artifact");
        let mut changed_permissions = before_permissions.clone();
        changed_permissions.set_readonly(!before_permissions.readonly());
        std::fs::set_permissions(&source, changed_permissions).expect("change source permissions");
        let after = snapshot_workspace(workspace.path()).expect("after snapshot");
        std::fs::set_permissions(&source, before_permissions).expect("restore source permissions");

        let summary = before
            .change_summary(&after)
            .expect("summary")
            .expect("source behavior change");
        assert!(summary.verification_relevant);
    }

    #[test]
    fn root_metadata_change_is_verification_relevant() {
        let workspace = tempfile::tempdir().expect("workspace");
        let before_permissions = std::fs::metadata(workspace.path())
            .expect("root metadata")
            .permissions();
        let mut changed_permissions = before_permissions.clone();
        changed_permissions.set_readonly(!before_permissions.readonly());
        std::fs::set_permissions(workspace.path(), changed_permissions)
            .expect("change root permissions");
        let before = snapshot_workspace(workspace.path()).expect("before snapshot");

        std::fs::set_permissions(workspace.path(), before_permissions)
            .expect("restore root permissions");
        let after = snapshot_workspace(workspace.path()).expect("after snapshot");

        let summary = before
            .change_summary(&after)
            .expect("summary")
            .expect("root metadata change");
        assert_eq!(summary.changed_files, ["."]);
        assert!(summary.verification_relevant);
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
    fn trusted_snapshot_observes_controller_owned_git_metadata() {
        let workspace = tempfile::tempdir().expect("workspace");
        let git = workspace.path().join(".git");
        std::fs::create_dir(&git).expect("git directory");
        let config = git.join("config");
        std::fs::write(&config, b"before").expect("git config before");
        let before = snapshot_trusted_workspace(workspace.path()).expect("before snapshot");
        std::fs::write(&config, b"after").expect("git config after");
        let after = snapshot_trusted_workspace(workspace.path()).expect("after snapshot");

        let summary = before
            .change_summary(&after)
            .expect("trusted change summary")
            .expect("trusted metadata changed");

        assert_eq!(summary.changed_files, [".git/config"]);
    }

    #[cfg(not(windows))]
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

    #[cfg(not(windows))]
    #[test]
    fn trusted_protected_directory_metadata_wave_is_ignored_but_replacement_fails_closed() {
        let workspace = tempfile::tempdir().expect("workspace");
        let protected = workspace.path().join(".singularity");
        std::fs::create_dir(&protected).expect("protected directory");
        let runtime_state = protected.join("runtime.sqlite");
        std::fs::write(&runtime_state, b"before").expect("runtime state");
        let before = snapshot_workspace(workspace.path()).expect("before snapshot");

        std::fs::write(&runtime_state, b"trusted runtime state grew").expect("trusted update");
        let after = snapshot_workspace(workspace.path()).expect("after snapshot");
        assert_eq!(
            before
                .change_summary(&after)
                .expect("trusted metadata wave"),
            None
        );

        let displaced = workspace.path().join(".singularity.displaced");
        std::fs::rename(&protected, &displaced).expect("displace protected directory");
        std::fs::create_dir(&protected).expect("replacement protected directory");
        std::fs::write(protected.join("runtime.sqlite"), b"replacement")
            .expect("replacement runtime state");
        let replaced = snapshot_workspace(workspace.path()).expect("replacement snapshot");
        assert_eq!(
            before
                .change_summary(&replaced)
                .expect_err("protected replacement must fail closed"),
            "protected workspace state changed between trusted observations"
        );
    }

    #[cfg(windows)]
    #[test]
    fn protected_sentinel_materialization_is_excluded_from_snapshot_diff() {
        let workspace = tempfile::tempdir().expect("workspace");
        let before = snapshot_workspace(workspace.path()).expect("before snapshot");
        std::fs::create_dir(workspace.path().join(".git")).expect("protected sentinel");
        let after = snapshot_workspace(workspace.path()).expect("after snapshot");

        assert_eq!(before.change_summary(&after).expect("summary"), None);
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
        let restored = snapshot_workspace(workspace.path()).expect("restored snapshot");
        std::fs::write(workspace.path().join("a").join("b"), b"changed")
            .expect("change nested file");
        let after_nested = snapshot_workspace(workspace.path()).expect("nested snapshot");
        assert_eq!(
            restored
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

    #[cfg(unix)]
    #[test]
    fn unix_workspace_root_permission_change_is_observed_without_child_noise() {
        use std::os::unix::fs::PermissionsExt as _;

        let workspace = tempfile::tempdir().expect("workspace");
        let mut initial_permissions = std::fs::metadata(workspace.path())
            .expect("initial root metadata")
            .permissions();
        initial_permissions.set_mode(0o755);
        std::fs::set_permissions(workspace.path(), initial_permissions)
            .expect("set initial root mode");
        let before = snapshot_workspace(workspace.path()).expect("before snapshot");
        let mut permissions = std::fs::metadata(workspace.path())
            .expect("root metadata")
            .permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(workspace.path(), permissions).expect("chmod root");
        let after = snapshot_workspace(workspace.path()).expect("after snapshot");

        let summary = before
            .change_summary(&after)
            .expect("summary")
            .expect("root changed");
        assert_eq!(summary.changed_files, ["."]);
        assert!(summary.verification_relevant);
    }
}
