//! Trusted, bounded workspace snapshots used to bind mutating commands to verification.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::io::Read;
use std::path::Path;
use std::path::PathBuf;
use std::time::UNIX_EPOCH;

#[cfg(unix)]
use std::os::unix::ffi::OsStrExt as _;
#[cfg(windows)]
use std::os::windows::ffi::OsStrExt as _;

#[cfg(windows)]
use cap_fs_ext::{DirExt as _, FollowSymlinks, MetadataExt as _, OpenOptionsFollowExt as _};
use cap_std::ambient_authority;
use cap_std::fs::{Dir, Metadata, OpenOptions};
use serde::Serialize;
use sha2::{Digest, Sha256};
use singularity_core::{
    is_protected_path, is_public_certificate_only_pem, is_public_certificate_pem_path,
};
#[cfg(windows)]
use singularity_windows_sandbox::{
    AbsolutePathBuf, WorkspaceChangeObservation, WorkspacePathChangeKind,
    open_pinned_workspace_path,
};
#[cfg(windows)]
use std::os::windows::io::AsRawHandle;
#[cfg(windows)]
use windows_sys::Win32::Storage::FileSystem::{
    FILE_GENERIC_READ, FILE_ID_INFO, FileIdInfo, GetFileInformationByHandleEx,
};

use super::WorkspaceChangeSummary;

const MAX_SNAPSHOT_ENTRIES: usize = 100_000;
const MAX_SNAPSHOT_DEPTH: usize = 256;
const MAX_SNAPSHOT_FILE_BYTES: u64 = 512 * 1024 * 1024;
const MAX_CHANGED_FILES: usize = 64;
const MAX_CHANGED_PATH_CHARS: usize = 512;
const MAX_PUBLIC_CERTIFICATE_PEM_BYTES: u64 = 1024 * 1024;

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
    /// Full object identity. On Windows this is the FILE_ID_INFO 128-bit identifier.
    file_id: [u8; 16],
    links: u64,
}

/// A capability-relative snapshot that contains only bounded metadata and content digests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct WorkspaceSnapshot {
    root: EntryMetadata,
    entries: BTreeMap<String, SnapshotEntry>,
    protected_paths: BTreeSet<String>,
    explicit_protected_paths: BTreeSet<String>,
    protected_entries: BTreeMap<String, EntryMetadata>,
}

#[cfg(windows)]
pub(super) enum IncrementalSnapshot {
    Updated(WorkspaceSnapshot, IncrementalSnapshotWork),
    FullRequired,
}

#[cfg(windows)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct IncrementalSnapshotWork {
    pub(super) entries_read: usize,
    pub(super) content_bytes_read: u64,
}

impl WorkspaceSnapshot {
    #[cfg(windows)]
    pub(super) fn full_scan_work(&self) -> IncrementalSnapshotWork {
        let content_bytes_read = self
            .entries
            .values()
            .filter_map(|entry| match entry {
                SnapshotEntry::File { metadata, .. } => Some(metadata.length),
                SnapshotEntry::Directory { .. } | SnapshotEntry::Symlink { .. } => None,
            })
            .sum();
        IncrementalSnapshotWork {
            entries_read: self.entries.len().saturating_add(1),
            content_bytes_read,
        }
    }

    /// Return the pinned Unix identity used to bind a transaction to this workspace root.
    #[cfg(unix)]
    pub(super) fn root_identity(&self) -> (u64, u64) {
        (self.root.device, self.root.inode)
    }

    /// Compare two trusted observations and produce the final changed paths and diff digest.
    #[cfg(any(unix, test))]
    pub(super) fn change_summary(
        &self,
        after: &Self,
    ) -> Result<Option<WorkspaceChangeSummary>, String> {
        match self.observed_change(after)? {
            (false, None) => Ok(None),
            (true, Some(summary)) => Ok(Some(summary)),
            (true, None) => {
                Err("workspace change exceeds the bounded verification scope".to_string())
            }
            _ => Err("workspace change observation is inconsistent".to_string()),
        }
    }

    /// Distinguishes a proven final-state change from an exact bounded path summary.
    ///
    /// Backends may safely report `Changed` when the complete snapshot proves a large change,
    /// while withholding an imprecise summary from model-facing revision accounting.
    pub(super) fn observed_change(
        &self,
        after: &Self,
    ) -> Result<(bool, Option<WorkspaceChangeSummary>), String> {
        #[cfg(windows)]
        if self.protected_paths != after.protected_paths {
            return Ok((true, None));
        }
        let changed_files = self.changed_paths(after)?;
        if changed_files.is_empty() {
            return Ok((false, None));
        }
        if !changed_paths_are_bounded(&changed_files) {
            if let Some(compacted) =
                compact_new_toolchain_artifact_paths(self, after, &changed_files)
            {
                return self
                    .complete_diff_digest(after)
                    .map(|digest| {
                        WorkspaceChangeSummary::new(compacted, digest)
                            .with_verification_relevant(false)
                    })
                    .map(|summary| (true, Some(summary)));
            }
            return Ok((true, None));
        }
        self.summary_for_paths(after, changed_files)
            .map(|summary| (true, Some(summary)))
    }

    /// Summarize a trusted control-plane transaction without applying the model-facing path cap.
    ///
    /// Large source preparations retain the complete bounded before/after snapshot as the digest
    /// input and use `.` as the honest transaction-wide path projection. Ordinary agent commands
    /// continue to require the exact bounded path list through `change_summary`.
    #[cfg(any(windows, test))]
    pub(super) fn trusted_change_summary(
        &self,
        after: &Self,
    ) -> Result<Option<WorkspaceChangeSummary>, String> {
        match self.observed_change(after)? {
            (false, None) => return Ok(None),
            (true, Some(summary)) => return Ok(Some(summary)),
            (true, None) => {}
            _ => return Err("workspace change observation is inconsistent".to_string()),
        }
        Ok(Some(WorkspaceChangeSummary::new(
            vec![".".to_string()],
            self.complete_diff_digest(after)?,
        )))
    }

    fn complete_diff_digest(&self, after: &Self) -> Result<String, String> {
        let encoded = serde_json::to_vec(&(
            &self.root,
            &after.root,
            &self.entries,
            &after.entries,
            &self.protected_paths,
            &after.protected_paths,
            &self.protected_entries,
            &after.protected_entries,
        ))
        .map_err(|error| format!("trusted workspace change encoding failed: {error}"))?;
        Ok(format!("sha256:{:x}", Sha256::digest(encoded)))
    }

    fn changed_paths(&self, after: &Self) -> Result<Vec<String>, String> {
        if !entry_identity_matches(&self.root, &after.root) {
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
            .chain(self.protected_paths.iter())
            .chain(after.protected_paths.iter())
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
        Ok(changed_files)
    }

    fn summary_for_paths(
        &self,
        after: &Self,
        changed_files: Vec<String>,
    ) -> Result<WorkspaceChangeSummary, String> {
        let changed_entries = changed_files
            .iter()
            .map(|path| (path, self.entries.get(path), after.entries.get(path)))
            .collect::<Vec<_>>();
        let encoded = serde_json::to_vec(&(self.root.clone(), after.root.clone(), changed_entries))
            .map_err(|error| format!("workspace change summary encoding failed: {error}"))?;
        let verification_relevant =
            workspace_change_is_verification_relevant(self, after, &changed_files);
        Ok(WorkspaceChangeSummary::new(
            changed_files,
            format!("sha256:{:x}", Sha256::digest(encoded)),
        )
        .with_verification_relevant(verification_relevant))
    }

    /// Compare the workspace baseline while ignoring trusted metadata churn on protected
    /// directories. Protected object existence, type, identity and permissions remain
    /// authoritative; volatile directory contents, length and timestamps stay outside the
    /// agent-visible revision.
    #[cfg(unix)]
    pub(super) fn transaction_baseline_matches(&self, after: &Self) -> bool {
        entry_identity_matches(&self.root, &after.root)
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

/// Refreshes only paths named by one complete Windows directory-change observation.
///
/// A new directory is read once as a subtree. Renames, hardlinks and incomplete observations
/// require a full snapshot; callers must never interpret those cases as unchanged.
#[cfg(windows)]
pub(super) fn update_workspace_snapshot_as_sandbox_user(
    workspace: &Path,
    before: &WorkspaceSnapshot,
    observation: &WorkspaceChangeObservation,
    explicit_protected_paths: &[PathBuf],
) -> Result<IncrementalSnapshot, String> {
    let explicit_protected_paths =
        workspace_relative_protected_paths(workspace, explicit_protected_paths)?;
    let root = Dir::open_ambient_dir(workspace, ambient_authority())
        .map_err(|error| format!("workspace root revalidation failed: {error}"))?;
    let root_before = entry_metadata_from_dir(&root)
        .map_err(|error| format!("workspace root metadata failed: {error}"))?;
    if !entry_identity_matches(&before.root, &root_before)
        || root_behavior_changed(&before.root, &root_before)
    {
        return Err(
            "workspace root identity or behavior drifted from the cached snapshot".to_string(),
        );
    }
    let mut after = before.clone();
    if before.explicit_protected_paths != explicit_protected_paths {
        if !before
            .explicit_protected_paths
            .is_subset(&explicit_protected_paths)
        {
            return Ok(IncrementalSnapshot::FullRequired);
        }
        for path in explicit_protected_paths.difference(&before.explicit_protected_paths) {
            if path_or_ancestor_is_explicitly_protected(path, &before.protected_paths) {
                continue;
            }
            if !is_public_certificate_pem_path(path)
                || !matches!(before.entries.get(path), Some(SnapshotEntry::File { .. }))
            {
                return Ok(IncrementalSnapshot::FullRequired);
            }
            let Some((parent, name)) = open_observed_parent(&root, path)? else {
                return Ok(IncrementalSnapshot::FullRequired);
            };
            if !is_public_certificate_entry(&parent, Path::new(&name))? {
                return Ok(IncrementalSnapshot::FullRequired);
            }
        }
        after.explicit_protected_paths = explicit_protected_paths;
    }
    let changes = match observation {
        WorkspaceChangeObservation::Unchanged => {
            after.root = root_before;
            return Ok(IncrementalSnapshot::Updated(
                after,
                IncrementalSnapshotWork {
                    entries_read: 1,
                    content_bytes_read: 0,
                },
            ));
        }
        WorkspaceChangeObservation::Unknown => return Ok(IncrementalSnapshot::FullRequired),
        WorkspaceChangeObservation::Changed(changes) => changes,
    };
    if changes.is_empty() {
        return Err("workspace change monitor returned an empty changed set".to_string());
    }
    if changes.iter().any(|change| {
        matches!(
            change.kind,
            WorkspacePathChangeKind::RenamedOld | WorkspacePathChangeKind::RenamedNew
        )
    }) {
        return Ok(IncrementalSnapshot::FullRequired);
    }
    for change in changes {
        validate_observed_relative_path(&change.path)?;
        if path_or_ancestor_is_protected(&change.path)
            || path_or_ancestor_is_explicitly_protected(&change.path, &before.protected_paths)
        {
            return Ok(IncrementalSnapshot::FullRequired);
        }
    }

    let mut total_file_bytes = snapshot_file_bytes(&after.entries)?;
    let mut work = IncrementalSnapshotWork::default();
    let mut changed_paths = changes.iter().fold(
        BTreeMap::<String, WorkspacePathChangeKind>::new(),
        |mut paths, change| {
            paths
                .entry(change.path.clone())
                .and_modify(|kind| {
                    if matches!(change.kind, WorkspacePathChangeKind::Added) {
                        *kind = WorkspacePathChangeKind::Added;
                    }
                })
                .or_insert(change.kind);
            paths
        },
    );
    let added_paths = changed_paths
        .iter()
        .filter_map(|(path, kind)| {
            matches!(kind, WorkspacePathChangeKind::Added).then_some(path.clone())
        })
        .collect::<BTreeSet<_>>();
    changed_paths.retain(|path, _| {
        !observed_parent_paths(path)
            .iter()
            .any(|parent| added_paths.contains(parent))
    });
    let parents = changed_paths
        .keys()
        .flat_map(|path| observed_parent_paths(path))
        .collect::<BTreeSet<_>>();
    for (path, kind) in changed_paths {
        match refresh_observed_path(
            workspace,
            &root,
            &path,
            kind,
            &mut after,
            &mut total_file_bytes,
            &mut work,
        )? {
            IncrementalRefresh::Updated => {}
            IncrementalRefresh::FullRequired => return Ok(IncrementalSnapshot::FullRequired),
        }
    }

    for parent in parents {
        match refresh_observed_path(
            workspace,
            &root,
            &parent,
            WorkspacePathChangeKind::Modified,
            &mut after,
            &mut total_file_bytes,
            &mut work,
        )? {
            IncrementalRefresh::Updated => {}
            IncrementalRefresh::FullRequired => return Ok(IncrementalSnapshot::FullRequired),
        }
    }
    let root_after = entry_metadata_from_dir(&root)
        .map_err(|error| format!("workspace root revalidation failed: {error}"))?;
    if root_before != root_after {
        return Err("workspace changed while its incremental snapshot was captured".to_string());
    }
    after.root = root_before;
    work.entries_read = work.entries_read.saturating_add(1);
    if after.entries.len() > MAX_SNAPSHOT_ENTRIES {
        return Err("workspace change snapshot exceeds the entry bound".to_string());
    }
    Ok(IncrementalSnapshot::Updated(after, work))
}

#[cfg(windows)]
enum IncrementalRefresh {
    Updated,
    FullRequired,
}

#[cfg(windows)]
fn refresh_observed_path(
    workspace: &Path,
    root: &Dir,
    path: &str,
    kind: WorkspacePathChangeKind,
    snapshot: &mut WorkspaceSnapshot,
    total_file_bytes: &mut u64,
    work: &mut IncrementalSnapshotWork,
) -> Result<IncrementalRefresh, String> {
    let previous = snapshot.entries.get(path).cloned();
    if matches!(
        &previous,
        Some(SnapshotEntry::File {
            metadata: EntryMetadata { links, .. },
            ..
        }) if *links != 1
    ) {
        return Ok(IncrementalRefresh::FullRequired);
    }
    let Some((parent, name)) = open_observed_parent(root, path)? else {
        return Ok(IncrementalRefresh::FullRequired);
    };
    let metadata = match parent.symlink_metadata(&name) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            remove_snapshot_path(snapshot, path, total_file_bytes)?;
            return Ok(IncrementalRefresh::Updated);
        }
        Err(error) => {
            return Err(format!(
                "workspace incremental path metadata failed for {path}: {error}"
            ));
        }
    };
    if metadata.file_type().is_symlink()
        || metadata.is_symlink()
        || cap_std::fs::MetadataExt::file_attributes(&metadata) & 0x0400 != 0
    {
        return Err(format!(
            "workspace incremental snapshot found a reparse point: {path}"
        ));
    }
    if metadata.is_dir() {
        if matches!(kind, WorkspacePathChangeKind::Added) {
            if previous.is_some() {
                return Ok(IncrementalRefresh::FullRequired);
            }
            let child = parent.open_dir_nofollow(&name).map_err(|error| {
                format!("workspace incremental directory open failed for {path}: {error}")
            })?;
            let before = entry_metadata_from_dir(&child).map_err(|error| {
                format!("workspace incremental directory metadata failed for {path}: {error}")
            })?;
            let existing_entries = snapshot
                .entries
                .len()
                .saturating_add(snapshot.protected_paths.len())
                .saturating_add(snapshot.protected_entries.len());
            let mut state = SnapshotState {
                workspace_root: workspace.to_path_buf(),
                entries: BTreeMap::new(),
                protected_paths: BTreeSet::new(),
                protected_entries: BTreeMap::new(),
                visited_entries: existing_entries,
                total_file_bytes: *total_file_bytes,
            };
            visit_directory(
                &child,
                Path::new(path),
                path.split('/').count(),
                true,
                false,
                &snapshot.explicit_protected_paths,
                &mut state,
            )?;
            let after = entry_metadata_from_dir(&child).map_err(|error| {
                format!("workspace incremental directory revalidation failed for {path}: {error}")
            })?;
            let path_after = entry_metadata_at(&parent, Path::new(&name)).map_err(|error| {
                format!("workspace incremental path revalidation failed for {path}: {error}")
            })?;
            if before != after || before != path_after {
                return Err(format!(
                    "workspace changed while directory {path} was incrementally captured"
                ));
            }
            if state.entries.values().any(|entry| {
                matches!(
                    entry,
                    SnapshotEntry::File {
                        metadata: EntryMetadata { links, .. },
                        ..
                    } if *links != 1
                )
            }) {
                return Ok(IncrementalRefresh::FullRequired);
            }
            let entries_read = state.visited_entries.saturating_sub(existing_entries);
            let content_bytes = state
                .total_file_bytes
                .checked_sub(*total_file_bytes)
                .ok_or_else(|| "workspace incremental snapshot size underflowed".to_string())?;
            *total_file_bytes = state.total_file_bytes;
            snapshot.entries.extend(state.entries);
            snapshot.protected_paths.extend(state.protected_paths);
            snapshot.protected_entries.extend(state.protected_entries);
            snapshot.entries.insert(
                path.to_string(),
                SnapshotEntry::Directory { metadata: before },
            );
            work.entries_read = work
                .entries_read
                .saturating_add(entries_read)
                .saturating_add(1);
            work.content_bytes_read = work
                .content_bytes_read
                .checked_add(content_bytes)
                .ok_or_else(|| "workspace incremental read count overflowed".to_string())?;
            return Ok(IncrementalRefresh::Updated);
        }
        if !matches!(previous, Some(SnapshotEntry::Directory { .. })) {
            return Ok(IncrementalRefresh::FullRequired);
        }
        let child = parent.open_dir_nofollow(&name).map_err(|error| {
            format!("workspace incremental directory open failed for {path}: {error}")
        })?;
        let before = entry_metadata_from_dir(&child).map_err(|error| {
            format!("workspace incremental directory metadata failed for {path}: {error}")
        })?;
        let after = entry_metadata_from_dir(&child).map_err(|error| {
            format!("workspace incremental directory revalidation failed for {path}: {error}")
        })?;
        let path_after = entry_metadata_at(&parent, Path::new(&name)).map_err(|error| {
            format!("workspace incremental path revalidation failed for {path}: {error}")
        })?;
        if before != after || before != path_after {
            return Err(format!(
                "workspace changed while directory {path} was incrementally captured"
            ));
        }
        snapshot.entries.insert(
            path.to_string(),
            SnapshotEntry::Directory { metadata: before },
        );
        work.entries_read = work.entries_read.saturating_add(1);
        return Ok(IncrementalRefresh::Updated);
    }
    if !metadata.is_file() {
        return Err(format!(
            "workspace incremental snapshot found an unsupported object: {path}"
        ));
    }
    remove_snapshot_entry_bytes(snapshot.entries.get(path), total_file_bytes)?;
    let mut state = SnapshotState {
        workspace_root: workspace.to_path_buf(),
        entries: BTreeMap::new(),
        protected_paths: BTreeSet::new(),
        protected_entries: BTreeMap::new(),
        visited_entries: 1,
        total_file_bytes: *total_file_bytes,
    };
    let absolute_parent = path.rsplit_once('/').map_or_else(
        || workspace.to_path_buf(),
        |(parent, _)| workspace.join(parent),
    );
    let entry = snapshot_file(&parent, Path::new(&name), &absolute_parent, &mut state)?;
    if matches!(
        &entry,
        SnapshotEntry::File {
            metadata: EntryMetadata { links, .. },
            ..
        } if *links != 1
    ) {
        return Ok(IncrementalRefresh::FullRequired);
    }
    let content_bytes = match &entry {
        SnapshotEntry::File { metadata, .. } => metadata.length,
        SnapshotEntry::Directory { .. } | SnapshotEntry::Symlink { .. } => 0,
    };
    *total_file_bytes = state.total_file_bytes;
    snapshot.entries.insert(path.to_string(), entry);
    work.entries_read = work.entries_read.saturating_add(1);
    work.content_bytes_read = work
        .content_bytes_read
        .checked_add(content_bytes)
        .ok_or_else(|| "workspace incremental read count overflowed".to_string())?;
    Ok(IncrementalRefresh::Updated)
}

#[cfg(windows)]
fn open_observed_parent(
    root: &Dir,
    path: &str,
) -> Result<Option<(Dir, std::ffi::OsString)>, String> {
    let mut components = path.split('/').peekable();
    let mut directory = root
        .try_clone()
        .map_err(|error| format!("workspace incremental root clone failed: {error}"))?;
    while let Some(component) = components.next() {
        if components.peek().is_none() {
            return Ok(Some((directory, std::ffi::OsString::from(component))));
        }
        directory = match directory.open_dir_nofollow(component) {
            Ok(child) => child,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(format!(
                    "workspace incremental parent open failed for {path}: {error}"
                ));
            }
        };
    }
    Err("workspace incremental path is empty".to_string())
}

#[cfg(windows)]
fn remove_snapshot_path(
    snapshot: &mut WorkspaceSnapshot,
    path: &str,
    total_file_bytes: &mut u64,
) -> Result<(), String> {
    let prefix = format!("{path}/");
    let removed = snapshot
        .entries
        .keys()
        .filter(|candidate| *candidate == path || candidate.starts_with(&prefix))
        .cloned()
        .collect::<Vec<_>>();
    for removed_path in removed {
        let entry = snapshot.entries.remove(&removed_path);
        remove_snapshot_entry_bytes(entry.as_ref(), total_file_bytes)?;
    }
    Ok(())
}

#[cfg(windows)]
fn remove_snapshot_entry_bytes(
    entry: Option<&SnapshotEntry>,
    total_file_bytes: &mut u64,
) -> Result<(), String> {
    let Some(SnapshotEntry::File { metadata, .. }) = entry else {
        return Ok(());
    };
    *total_file_bytes = total_file_bytes
        .checked_sub(metadata.length)
        .ok_or_else(|| "workspace incremental snapshot size underflowed".to_string())?;
    Ok(())
}

#[cfg(windows)]
fn snapshot_file_bytes(entries: &BTreeMap<String, SnapshotEntry>) -> Result<u64, String> {
    entries.values().try_fold(0u64, |total, entry| {
        let length = match entry {
            SnapshotEntry::File { metadata, .. } => metadata.length,
            SnapshotEntry::Directory { .. } | SnapshotEntry::Symlink { .. } => 0,
        };
        total
            .checked_add(length)
            .ok_or_else(|| "workspace incremental snapshot size overflowed".to_string())
    })
}

#[cfg(windows)]
fn observed_parent_paths(path: &str) -> Vec<String> {
    let mut parts = path.split('/').collect::<Vec<_>>();
    let mut parents = Vec::new();
    parts.pop();
    while !parts.is_empty() {
        parents.push(parts.join("/"));
        parts.pop();
    }
    parents
}

pub(super) fn validate_observed_relative_path(path: &str) -> Result<(), String> {
    if path.is_empty()
        || path.contains('\\')
        || path.starts_with('/')
        || path.len() > MAX_CHANGED_PATH_CHARS
        || path
            .split('/')
            .any(|part| part.is_empty() || matches!(part, "." | "..") || part.contains(':'))
    {
        return Err("workspace change monitor returned an unsafe relative path".to_string());
    }
    Ok(())
}

#[cfg(windows)]
fn path_or_ancestor_is_protected(path: &str) -> bool {
    let mut current = String::new();
    path.split('/').any(|component| {
        if !current.is_empty() {
            current.push('/');
        }
        current.push_str(component);
        is_protected_path(component) || is_protected_path(&current)
    })
}

#[cfg(windows)]
fn path_or_ancestor_is_explicitly_protected(
    path: &str,
    protected_paths: &BTreeSet<String>,
) -> bool {
    protected_paths
        .iter()
        .any(|protected| path == protected || path.starts_with(&format!("{protected}/")))
}

pub(super) fn changed_paths_are_bounded(changed_files: &[String]) -> bool {
    changed_files.len() <= MAX_CHANGED_FILES
        && changed_files
            .iter()
            .all(|path| path.chars().count() <= MAX_CHANGED_PATH_CHARS)
}

fn compact_new_toolchain_artifact_paths(
    before: &WorkspaceSnapshot,
    after: &WorkspaceSnapshot,
    changed_files: &[String],
) -> Option<Vec<String>> {
    if workspace_change_is_verification_relevant(before, after, changed_files) {
        return None;
    }
    let mut candidates = changed_files
        .iter()
        .filter(|path| is_toolchain_artifact_path(path))
        .cloned()
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| {
        left.split('/')
            .count()
            .cmp(&right.split('/').count())
            .then_with(|| left.cmp(right))
    });
    let mut roots = Vec::<String>::new();
    for path in candidates {
        if roots
            .iter()
            .any(|root| path == *root || path.starts_with(&format!("{root}/")))
        {
            continue;
        }
        roots.push(path);
    }
    (!roots.is_empty() && changed_paths_are_bounded(&roots)).then_some(roots)
}

fn workspace_change_is_verification_relevant(
    before: &WorkspaceSnapshot,
    after: &WorkspaceSnapshot,
    changed_files: &[String],
) -> bool {
    changed_files.iter().any(|path| {
        if is_incidental_artifact_ancestor_change(before, after, changed_files, path) {
            return false;
        }
        if is_toolchain_artifact_path(path) {
            return before.entries.contains_key(path);
        }
        true
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
        && entry_identity_matches(before, after)
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
            && entry_identity_matches(before, after)
    } else {
        before == after
    }
}

/// Snapshot the workspace through a directory capability without following links.
#[cfg(any(not(target_os = "windows"), test))]
pub(super) fn snapshot_workspace(workspace: &Path) -> Result<WorkspaceSnapshot, String> {
    snapshot_workspace_with_protected_paths(workspace, true, true, &[])
}

/// Snapshot ordinary workspace state as a sandbox account without opening protected file content.
#[cfg(target_os = "windows")]
pub(super) fn snapshot_workspace_as_sandbox_user(
    workspace: &Path,
    explicit_protected_paths: &[PathBuf],
) -> Result<WorkspaceSnapshot, String> {
    snapshot_workspace_with_protected_paths(workspace, true, false, explicit_protected_paths)
}

#[cfg(target_os = "windows")]
pub(super) fn snapshot_workspace_as_sandbox_user_for_cached_root(
    workspace: &Path,
    before: &WorkspaceSnapshot,
    explicit_protected_paths: &[PathBuf],
) -> Result<WorkspaceSnapshot, String> {
    let snapshot = snapshot_workspace_as_sandbox_user(workspace, explicit_protected_paths)?;
    if !entry_identity_matches(&before.root, &snapshot.root)
        || root_behavior_changed(&before.root, &snapshot.root)
    {
        return Err(
            "workspace root identity or behavior drifted from the cached snapshot".to_string(),
        );
    }
    Ok(snapshot)
}

/// Revalidate only the cached workspace root identity and security behavior.
///
/// This is intentionally metadata-only: protected-path cache reuse must never enumerate the
/// workspace before the bounded no-follow ACL revalidation performed by the Windows sandbox.
#[cfg(target_os = "windows")]
pub(super) fn validate_cached_workspace_root(
    workspace: &Path,
    before: &WorkspaceSnapshot,
) -> Result<(), String> {
    let root = Dir::open_ambient_dir(workspace, ambient_authority())
        .map_err(|error| format!("workspace root revalidation failed: {error}"))?;
    let current = entry_metadata_from_dir(&root)
        .map_err(|error| format!("workspace root metadata failed: {error}"))?;
    if !entry_identity_matches(&before.root, &current)
        || root_behavior_changed(&before.root, &current)
    {
        return Err(
            "workspace root identity or behavior drifted from the cached snapshot".to_string(),
        );
    }
    Ok(())
}

/// A no-follow identity for one concrete protected path retained across command checkpoints.
#[cfg(target_os = "windows")]
#[derive(Clone)]
pub(super) struct CachedProtectedPath {
    pub(super) path: AbsolutePathBuf,
    device: u64,
    file_id: [u8; 16],
}

/// Capture protected object identities without traversing any unrelated workspace entry.
#[cfg(target_os = "windows")]
pub(super) fn capture_cached_protected_paths(
    workspace: &Path,
    paths: &[AbsolutePathBuf],
) -> Result<Vec<CachedProtectedPath>, String> {
    let root = Dir::open_ambient_dir(workspace, ambient_authority())
        .map_err(|error| format!("protected path identity root open failed: {error}"))?;
    paths
        .iter()
        .map(|path| {
            let Some(relative) = path.as_path().strip_prefix(workspace).ok() else {
                return Err("protected path identity escaped the workspace root".to_string());
            };
            let relative = workspace_relative_path(relative)?;
            let Some((parent, name)) = open_observed_parent(&root, &relative)? else {
                return Err("protected path identity target disappeared".to_string());
            };
            let metadata = entry_metadata_at(&parent, Path::new(&name))
                .map_err(|error| format!("protected path identity metadata failed: {error}"))?;
            Ok(CachedProtectedPath {
                path: path.clone(),
                device: metadata.device,
                file_id: metadata.file_id,
            })
        })
        .collect()
}

/// Revalidate cached protected object identities with no-follow parent traversal.
#[cfg(target_os = "windows")]
pub(super) fn validate_cached_protected_paths(
    workspace: &Path,
    paths: &[CachedProtectedPath],
) -> Result<(), String> {
    let root = Dir::open_ambient_dir(workspace, ambient_authority())
        .map_err(|error| format!("protected path identity root open failed: {error}"))?;
    for cached in paths {
        let Some(relative) = cached.path.as_path().strip_prefix(workspace).ok() else {
            return Err("protected path identity escaped the workspace root".to_string());
        };
        let relative = workspace_relative_path(relative)?;
        let Some((parent, name)) = open_observed_parent(&root, &relative)? else {
            return Err("cached protected path disappeared".to_string());
        };
        let metadata = entry_metadata_at(&parent, Path::new(&name))
            .map_err(|error| format!("cached protected path identity metadata failed: {error}"))?;
        if metadata.device != cached.device || metadata.file_id != cached.file_id {
            return Err("cached protected path identity changed".to_string());
        }
    }
    Ok(())
}

/// Snapshot controller-owned workspace metadata as ordinary transactional content.
pub(super) fn snapshot_trusted_workspace(workspace: &Path) -> Result<WorkspaceSnapshot, String> {
    snapshot_workspace_with_protected_paths(workspace, false, false, &[])
}

/// Snapshot a trusted workspace through an already pinned root handle.
#[cfg(target_os = "windows")]
pub(super) fn snapshot_trusted_workspace_from_handle(
    workspace: &Path,
    handle: &std::fs::File,
) -> Result<WorkspaceSnapshot, String> {
    let root = Dir::from_std_file(
        handle
            .try_clone()
            .map_err(|error| format!("workspace change snapshot handle clone failed: {error}"))?,
    );
    snapshot_opened_directory(root, workspace, false, false, &BTreeSet::new())
}

#[cfg(windows)]
fn workspace_relative_protected_paths(
    workspace: &Path,
    protected_paths: &[PathBuf],
) -> Result<BTreeSet<String>, String> {
    let workspace = dunce::simplified(workspace);
    protected_paths
        .iter()
        .filter_map(|path| {
            let path = dunce::simplified(path);
            let relative = path.strip_prefix(workspace).ok()?;
            if relative.as_os_str().is_empty() {
                return Some(Err(
                    "workspace root cannot be an opaque protected path".to_string()
                ));
            }
            Some(workspace_relative_path(relative))
        })
        .collect()
}

#[cfg(not(windows))]
fn workspace_relative_protected_paths(
    _workspace: &Path,
    _protected_paths: &[PathBuf],
) -> Result<BTreeSet<String>, String> {
    Ok(BTreeSet::new())
}

fn snapshot_workspace_with_protected_paths(
    workspace: &Path,
    protect_paths: bool,
    inspect_public_certificates: bool,
    explicit_protected_paths: &[PathBuf],
) -> Result<WorkspaceSnapshot, String> {
    let root = Dir::open_ambient_dir(workspace, ambient_authority())
        .map_err(|error| format!("workspace change snapshot is unavailable: {error}"))?;
    let explicit_protected_paths =
        workspace_relative_protected_paths(workspace, explicit_protected_paths)?;
    snapshot_opened_directory(
        root,
        workspace,
        protect_paths,
        inspect_public_certificates,
        &explicit_protected_paths,
    )
}

fn snapshot_opened_directory(
    root: Dir,
    workspace: &Path,
    protect_paths: bool,
    inspect_public_certificates: bool,
    explicit_protected_paths: &BTreeSet<String>,
) -> Result<WorkspaceSnapshot, String> {
    let root_before = entry_metadata_from_dir(&root)
        .map_err(|error| format!("workspace root metadata failed: {error}"))?;
    let mut state = SnapshotState {
        workspace_root: workspace.to_path_buf(),
        entries: BTreeMap::new(),
        protected_paths: BTreeSet::new(),
        protected_entries: BTreeMap::new(),
        visited_entries: 0,
        total_file_bytes: 0,
    };
    visit_directory(
        &root,
        Path::new(""),
        0,
        protect_paths,
        inspect_public_certificates,
        explicit_protected_paths,
        &mut state,
    )?;
    let root_after = entry_metadata_from_dir(&root)
        .map_err(|error| format!("workspace root revalidation failed: {error}"))?;
    if root_before != root_after {
        return Err("workspace changed while its root snapshot was being captured".to_string());
    }
    Ok(WorkspaceSnapshot {
        root: root_before,
        entries: state.entries,
        protected_paths: state.protected_paths,
        explicit_protected_paths: explicit_protected_paths.clone(),
        protected_entries: state.protected_entries,
    })
}

struct SnapshotState {
    workspace_root: PathBuf,
    entries: BTreeMap<String, SnapshotEntry>,
    protected_paths: BTreeSet<String>,
    protected_entries: BTreeMap<String, EntryMetadata>,
    visited_entries: usize,
    total_file_bytes: u64,
}

fn visit_directory(
    directory: &Dir,
    relative_parent: &Path,
    depth: usize,
    protect_paths: bool,
    inspect_public_certificates: bool,
    explicit_protected_paths: &BTreeSet<String>,
    state: &mut SnapshotState,
) -> Result<(), String> {
    if depth > MAX_SNAPSHOT_DEPTH {
        return Err("workspace change snapshot exceeds the depth bound".to_string());
    }
    let absolute_parent = state.workspace_root.join(relative_parent);
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
        let file_type = entry
            .file_type()
            .map_err(|error| format!("workspace change snapshot type failed: {error}"))?;
        #[cfg(windows)]
        if is_reserved_windows_name(&name) {
            let metadata = entry
                .metadata()
                .map_err(|error| format!("workspace change snapshot metadata failed: {error}"))?;
            if cap_std::fs::MetadataExt::file_attributes(&metadata) & 0x0400 != 0 {
                return Err(format!(
                    "workspace change snapshot found a reparse point: {relative_text}"
                ));
            }
            if !metadata.is_file() {
                return Err(format!(
                    "workspace change snapshot found an unsupported object: {relative_text}"
                ));
            }
            let snapshot_entry =
                snapshot_file(directory, Path::new(&name), &absolute_parent, state)?;
            state.entries.insert(relative_text, snapshot_entry);
            continue;
        }
        if protect_paths
            && (is_protected_path(name_text)
                || is_protected_path(&relative_text)
                || explicit_protected_paths.contains(&relative_text))
        {
            if inspect_public_certificates
                && file_type.is_file()
                && is_public_certificate_pem_path(&relative_text)
                && is_public_certificate_entry(directory, Path::new(&name))?
            {
                let snapshot_entry =
                    snapshot_file(directory, Path::new(&name), &absolute_parent, state)?;
                if !is_public_certificate_entry(directory, Path::new(&name))? {
                    return Err(
                        "public certificate changed while its workspace snapshot was captured"
                            .to_string(),
                    );
                }
                state.entries.insert(relative_text, snapshot_entry);
                continue;
            }
            #[cfg(windows)]
            state.protected_paths.insert(relative_text.clone());
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
        #[cfg(windows)]
        let is_link = file_type.is_symlink()
            || entry
                .metadata()
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
            let before = entry_metadata_from_dir(&child)
                .map_err(|error| format!("workspace change directory metadata failed: {error}"))?;
            visit_directory(
                &child,
                &relative,
                depth + 1,
                protect_paths,
                inspect_public_certificates,
                explicit_protected_paths,
                state,
            )?;
            let after = entry_metadata_from_dir(&child).map_err(|error| {
                format!("workspace change directory revalidation failed: {error}")
            })?;
            let path_after = entry_metadata_at(directory, Path::new(&name))
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
            snapshot_file(directory, Path::new(&name), &absolute_parent, state)?
        } else {
            return Err(format!(
                "workspace change snapshot found an unsupported object: {relative_text}"
            ));
        };
        state.entries.insert(relative_text, snapshot_entry);
    }
    Ok(())
}

#[cfg(windows)]
fn is_reserved_windows_name(name: &OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    let stem = name.split_once('.').map_or(name, |(stem, _)| stem);
    let stem = stem.trim_end().to_uppercase();
    matches!(
        stem.as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM0"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "COM¹"
            | "COM²"
            | "COM³"
            | "LPT0"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
            | "LPT¹"
            | "LPT²"
            | "LPT³"
    )
}

fn snapshot_symlink(directory: &Dir, name: &Path) -> Result<SnapshotEntry, String> {
    let metadata_before = entry_metadata_at(directory, name)
        .map_err(|error| format!("workspace change link metadata failed: {error}"))?;
    let first = directory
        .read_link_contents(name)
        .map_err(|error| format!("workspace change link read failed: {error}"))?;
    let second = directory
        .read_link_contents(name)
        .map_err(|error| format!("workspace change link revalidation failed: {error}"))?;
    if first != second {
        return Err("workspace changed while its link snapshot was being captured".to_string());
    }
    let metadata_after = entry_metadata_at(directory, name)
        .map_err(|error| format!("workspace change link revalidation failed: {error}"))?;
    if metadata_before != metadata_after {
        return Err("workspace changed while its link snapshot was being captured".to_string());
    }
    Ok(SnapshotEntry::Symlink {
        metadata: metadata_before,
        target_digest: hash_os_str(first.as_os_str()),
    })
}

fn is_public_certificate_entry(directory: &Dir, name: &Path) -> Result<bool, String> {
    let mut options = OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    let mut file = directory
        .open_with(name, &options)
        .map_err(|error| format!("public certificate open failed: {error}"))?;
    let before = entry_metadata_from_file(&file)
        .map_err(|error| format!("public certificate metadata failed: {error}"))?;
    if before.object_kind != 1 || before.length > MAX_PUBLIC_CERTIFICATE_PEM_BYTES {
        return Ok(false);
    }
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(MAX_PUBLIC_CERTIFICATE_PEM_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("public certificate read failed: {error}"))?;
    if bytes.len() as u64 > MAX_PUBLIC_CERTIFICATE_PEM_BYTES {
        return Ok(false);
    }
    let after = entry_metadata_from_file(&file)
        .map_err(|error| format!("public certificate revalidation failed: {error}"))?;
    let path_after = entry_metadata_at(directory, name)
        .map_err(|error| format!("public certificate path revalidation failed: {error}"))?;
    if before != after || before != path_after {
        return Err("public certificate changed while it was classified".to_string());
    }
    Ok(is_public_certificate_only_pem(&bytes, |der| {
        let certificate = rustls_pki_types::CertificateDer::from(der);
        webpki::EndEntityCert::try_from(&certificate).is_ok()
    }))
}

fn snapshot_file(
    directory: &Dir,
    name: &Path,
    _absolute_parent: &Path,
    state: &mut SnapshotState,
) -> Result<SnapshotEntry, String> {
    #[cfg(windows)]
    if is_reserved_windows_name(name.as_os_str()) {
        return snapshot_reserved_file(directory, name, _absolute_parent, state);
    }
    let mut options = OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    let mut file = directory.open_with(name, &options).map_err(|error| {
        format!(
            "workspace change file open failed for {}: {error}",
            name.display()
        )
    })?;
    let before = entry_metadata_from_file(&file)
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
    let after = entry_metadata_from_file(&file)
        .map_err(|error| format!("workspace change file revalidation failed: {error}"))?;
    let path_after = entry_metadata_at(directory, name)
        .map_err(|error| format!("workspace change path revalidation failed: {error}"))?;
    if before != after || before != path_after {
        return Err("workspace changed while its file snapshot was being captured".to_string());
    }
    Ok(SnapshotEntry::File {
        metadata: before,
        content_digest: format!("sha256:{:x}", hasher.finalize()),
    })
}

#[cfg(windows)]
fn snapshot_reserved_file(
    directory: &Dir,
    name: &Path,
    absolute_parent: &Path,
    state: &mut SnapshotState,
) -> Result<SnapshotEntry, String> {
    let parent_handle = directory
        .try_clone()
        .map_err(|error| format!("workspace change reserved-file parent clone failed: {error}"))?
        .into_std_file();
    let target = absolute_parent.join(name);
    let open = || {
        open_pinned_workspace_path(&parent_handle, absolute_parent, &target, FILE_GENERIC_READ)
            .map_err(|error| {
                format!(
                    "workspace change reserved-file open failed for {}: {error}",
                    target.display()
                )
            })?
            .ok_or_else(|| {
                format!(
                    "workspace change reserved-file path escaped its parent: {}",
                    target.display()
                )
            })
    };
    let mut file = cap_std::fs::File::from_std(open()?);
    let before = entry_metadata_from_file(&file)
        .map_err(|error| format!("workspace change reserved-file metadata failed: {error}"))?;
    if before.object_kind != 1 {
        return Err(format!(
            "workspace change snapshot found an unsupported object: {}",
            target.display()
        ));
    }
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
            .map_err(|error| format!("workspace change reserved-file read failed: {error}"))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let after_handle = entry_metadata_from_file(&file)
        .map_err(|error| format!("workspace change reserved-file revalidation failed: {error}"))?;
    let second = cap_std::fs::File::from_std(open()?);
    let after_path = entry_metadata_from_file(&second).map_err(|error| {
        format!("workspace change reserved-file path revalidation failed: {error}")
    })?;
    if before != after_handle || before != after_path {
        return Err(
            "workspace changed while its reserved file snapshot was being captured".to_string(),
        );
    }
    Ok(SnapshotEntry::File {
        metadata: before,
        content_digest: format!("sha256:{:x}", hasher.finalize()),
    })
}

#[cfg(not(windows))]
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
        file_id: {
            let mut file_id = [0u8; 16];
            file_id[..8].copy_from_slice(&metadata.ino().to_le_bytes());
            file_id
        },
        links: metadata.nlink(),
    })
}

#[cfg(windows)]
fn entry_metadata_from_dir(directory: &Dir) -> std::io::Result<EntryMetadata> {
    let file = directory.try_clone()?.into_std_file();
    let metadata = directory.dir_metadata().map_err(|error| {
        std::io::Error::other(format!("directory metadata clone failed: {error}"))
    })?;
    entry_metadata_from_open_file(&metadata, &file)
}

#[cfg(windows)]
fn entry_metadata_at(directory: &Dir, name: &Path) -> std::io::Result<EntryMetadata> {
    let path_metadata = directory.symlink_metadata(name)?;
    if path_metadata.is_dir() && !path_metadata.is_symlink() {
        let child = directory.open_dir_nofollow(name)?;
        return entry_metadata_from_dir(&child);
    }
    let mut options = OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    let file = directory.open_with(name, &options)?;
    entry_metadata_from_file(&file)
}

#[cfg(windows)]
fn entry_metadata_from_file(file: &cap_std::fs::File) -> std::io::Result<EntryMetadata> {
    let metadata = file.metadata()?;
    let standard_file = file.try_clone()?.into_std();
    entry_metadata_from_open_file(&metadata, &standard_file)
}

#[cfg(windows)]
fn entry_metadata_from_open_file(
    metadata: &Metadata,
    file: &std::fs::File,
) -> std::io::Result<EntryMetadata> {
    let mut information: FILE_ID_INFO = unsafe { std::mem::zeroed() };
    let queried = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle() as _,
            FileIdInfo,
            &mut information as *mut _ as *mut _,
            std::mem::size_of::<FILE_ID_INFO>() as u32,
        )
    };
    if queried == 0 {
        return Err(std::io::Error::last_os_error());
    }
    let file_id = information.FileId.Identifier;
    let inode = u64::from_le_bytes(file_id[..8].try_into().expect("FILE_ID_128 has 16 bytes"));
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
        device: information.VolumeSerialNumber,
        inode,
        file_id,
        links: metadata.nlink(),
    })
}

#[cfg(not(windows))]
fn entry_metadata_from_dir(directory: &Dir) -> std::io::Result<EntryMetadata> {
    directory
        .dir_metadata()
        .and_then(|metadata| entry_metadata(&metadata))
}

#[cfg(not(windows))]
fn entry_metadata_at(directory: &Dir, name: &Path) -> std::io::Result<EntryMetadata> {
    directory
        .symlink_metadata(name)
        .and_then(|metadata| entry_metadata(&metadata))
}

#[cfg(not(windows))]
fn entry_metadata_from_file(file: &cap_std::fs::File) -> std::io::Result<EntryMetadata> {
    file.metadata()
        .and_then(|metadata| entry_metadata(&metadata))
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

fn entry_identity_matches(before: &EntryMetadata, after: &EntryMetadata) -> bool {
    before.device == after.device && before.file_id == after.file_id
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
    #[cfg(windows)]
    use super::{
        IncrementalSnapshot, snapshot_workspace_as_sandbox_user,
        update_workspace_snapshot_as_sandbox_user,
    };
    use super::{is_toolchain_artifact_path, snapshot_trusted_workspace, snapshot_workspace};
    use crate::WorkspaceChangeSummary;
    #[cfg(windows)]
    use singularity_windows_sandbox::{
        WorkspaceChangeObservation, WorkspacePathChange, WorkspacePathChangeKind,
    };

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
    fn public_summary_validation_reuses_producer_bounds_and_path_rules() {
        let valid = WorkspaceChangeSummary::new(
            vec!["src/lib.rs".to_string()],
            format!("sha256:{}", "a".repeat(64)),
        );
        assert!(valid.validate().is_ok());

        let root_projection = WorkspaceChangeSummary::new(
            vec![".".to_string()],
            format!("sha256:{}", "b".repeat(64)),
        );
        assert!(root_projection.validate().is_ok());

        for path in ["/absolute", "nested\\value", "nested/../value", ""] {
            let invalid = WorkspaceChangeSummary::new(
                vec![path.to_string()],
                format!("sha256:{}", "c".repeat(64)),
            );
            assert!(
                invalid.validate().is_err(),
                "invalid path accepted: {path:?}"
            );
        }

        let invalid_digest =
            WorkspaceChangeSummary::new(vec!["src/lib.rs".to_string()], "sha256:not-a-digest");
        assert!(invalid_digest.validate().is_err());
    }

    #[test]
    fn trusted_summary_collapses_only_the_path_projection_for_large_transactions() {
        let workspace = tempfile::tempdir().expect("workspace");
        let before = snapshot_trusted_workspace(workspace.path()).expect("before snapshot");
        for index in 0..=super::MAX_CHANGED_FILES {
            std::fs::write(
                workspace.path().join(format!("file-{index:03}.txt")),
                b"payload",
            )
            .expect("write transaction file");
        }
        let after = snapshot_trusted_workspace(workspace.path()).expect("after snapshot");

        assert!(before.change_summary(&after).is_err());
        let summary = before
            .trusted_change_summary(&after)
            .expect("trusted summary")
            .expect("changed transaction");
        assert_eq!(summary.changed_files, ["."]);
        assert!(summary.diff_digest.starts_with("sha256:"));
        assert_eq!(summary.diff_digest.len(), "sha256:".len() + 64);

        std::fs::write(workspace.path().join("file-000.txt"), b"different")
            .expect("change transaction content");
        let changed = snapshot_trusted_workspace(workspace.path()).expect("changed snapshot");
        let changed = before
            .trusted_change_summary(&changed)
            .expect("changed trusted summary")
            .expect("changed transaction");
        assert_ne!(summary.diff_digest, changed.diff_digest);
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
    fn large_new_toolchain_artifact_tree_uses_a_bounded_complete_summary() {
        let workspace = tempfile::tempdir().expect("workspace");
        let before = snapshot_workspace(workspace.path()).expect("before snapshot");
        let cache = workspace.path().join(".pytest_cache/v/cache");
        std::fs::create_dir_all(&cache).expect("cache tree");
        for index in 0..=super::MAX_CHANGED_FILES {
            std::fs::write(cache.join(format!("node-{index:03}.json")), b"artifact")
                .expect("cache artifact");
        }
        let after = snapshot_workspace(workspace.path()).expect("after snapshot");

        let summary = before
            .change_summary(&after)
            .expect("complete artifact summary")
            .expect("artifact change");

        assert_eq!(summary.changed_files, [".pytest_cache"]);
        assert!(!summary.verification_relevant);
        assert!(summary.is_trusted_artifact_only());
        assert!(summary.diff_digest.starts_with("sha256:"));
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
    fn nested_new_artifact_under_existing_artifact_directory_is_incidental_when_safe() {
        use cap_fs_ext::DirExt as _;
        use cap_std::ambient_authority;
        use cap_std::fs::Dir;
        use std::time::{Duration, SystemTime};

        let workspace = tempfile::tempdir().expect("workspace");
        let cache = workspace.path().join("__pycache__");
        std::fs::create_dir(&cache).expect("artifact directory");
        std::fs::write(cache.join("test_calculator.pyc"), b"existing artifact")
            .expect("existing artifact");
        let before = snapshot_workspace(workspace.path()).expect("before snapshot");

        std::fs::write(cache.join("calculator.pyc"), b"new artifact").expect("new artifact");
        let cache_directory =
            Dir::open_ambient_dir(&cache, ambient_authority()).expect("open artifact directory");
        cache_directory
            .set_times(
                ".",
                None,
                Some(cap_fs_ext::SystemTimeSpec::Absolute(
                    cap_std::time::SystemTime::from_std(
                        SystemTime::now() + Duration::from_secs(10),
                    ),
                )),
            )
            .expect("touch artifact directory");
        let after = snapshot_workspace(workspace.path()).expect("after snapshot");
        let summary = before
            .change_summary(&after)
            .expect("summary")
            .expect("nested artifact change");

        assert_eq!(
            summary.changed_files,
            [
                "__pycache__".to_string(),
                "__pycache__/calculator.pyc".to_string()
            ]
        );
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
    fn snapshot_tracks_reserved_named_file_content() {
        let workspace = tempfile::tempdir().expect("workspace");
        let nul_path = format!(r"\\?\{}\nul", workspace.path().to_string_lossy());
        std::fs::write(&nul_path, b"before").expect("create reserved-name fixture");

        let before = snapshot_trusted_workspace(workspace.path()).expect("reserved-name snapshot");
        std::fs::write(&nul_path, b"after").expect("modify reserved-name fixture");
        let after =
            snapshot_trusted_workspace(workspace.path()).expect("snapshot after modification");

        let summary = before
            .change_summary(&after)
            .expect("reserved-name change summary")
            .expect("reserved-name content changed");
        assert_eq!(summary.changed_files, ["nul"]);
    }

    #[cfg(windows)]
    #[test]
    fn snapshot_rejects_reserved_named_directory() {
        let workspace = tempfile::tempdir().expect("workspace");
        let nul_path = format!(r"\\?\{}\nul", workspace.path().to_string_lossy());
        std::fs::create_dir(&nul_path).expect("create reserved-name directory fixture");

        let error = snapshot_trusted_workspace(workspace.path()).expect_err("reserved directory");
        assert!(
            error.contains("unsupported object"),
            "unexpected error: {error}"
        );
    }

    #[cfg(windows)]
    #[test]
    fn protected_path_creation_is_observed_without_exposing_a_summary() {
        let workspace = tempfile::tempdir().expect("workspace");
        let before = snapshot_workspace(workspace.path()).expect("before snapshot");
        std::fs::create_dir(workspace.path().join(".git")).expect("protected sentinel");
        let after = snapshot_workspace(workspace.path()).expect("after snapshot");

        assert_eq!(before.observed_change(&after), Ok((true, None)));
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
    fn unix_symlink_snapshot_preserves_absolute_target_without_resolving_it() {
        use std::os::unix::fs::symlink;

        let workspace = tempfile::tempdir().expect("workspace");
        let link = workspace.path().join("link");
        symlink("/path/that/does/not/exist", &link).expect("first absolute link");
        let before = snapshot_workspace(workspace.path()).expect("before snapshot");
        std::fs::remove_file(&link).expect("remove first link");
        symlink("/another/missing/target", &link).expect("second absolute link");
        let after = snapshot_workspace(workspace.path()).expect("after snapshot");

        assert_eq!(
            before
                .change_summary(&after)
                .expect("summary")
                .expect("target changed")
                .changed_files,
            ["link"]
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

    #[cfg(windows)]
    #[test]
    fn unchanged_observation_revalidates_only_the_root_identity() {
        let workspace = tempfile::tempdir().expect("workspace");
        std::fs::write(workspace.path().join("value.txt"), b"value").expect("write file");
        let before = snapshot_workspace(workspace.path()).expect("before snapshot");

        let IncrementalSnapshot::Updated(after, work) = update_workspace_snapshot_as_sandbox_user(
            workspace.path(),
            &before,
            &WorkspaceChangeObservation::Unchanged,
            &[],
        )
        .expect("revalidate unchanged snapshot") else {
            panic!("unchanged observation must not require a full snapshot");
        };
        assert_eq!(after, before);
        assert_eq!(work.entries_read, 1);
        assert_eq!(work.content_bytes_read, 0);
    }

    #[cfg(windows)]
    #[test]
    fn explicit_protected_directory_is_opaque_and_changes_invalidate_reuse() {
        let workspace = tempfile::tempdir().expect("workspace");
        let opaque = workspace.path().join(".opaque");
        std::fs::create_dir(&opaque).expect("opaque directory");
        std::fs::write(opaque.join("value.txt"), b"secret").expect("opaque content");

        let snapshot =
            snapshot_workspace_as_sandbox_user(workspace.path(), std::slice::from_ref(&opaque))
                .expect("snapshot with explicit protected path");
        assert!(snapshot.protected_paths.contains(".opaque"));
        assert!(
            snapshot
                .entries
                .keys()
                .all(|path| path != ".opaque" && !path.starts_with(".opaque/"))
        );

        assert!(matches!(
            update_workspace_snapshot_as_sandbox_user(
                workspace.path(),
                &snapshot,
                &WorkspaceChangeObservation::Unchanged,
                &[],
            )
            .expect("policy change outcome"),
            IncrementalSnapshot::FullRequired
        ));
    }

    #[cfg(windows)]
    #[test]
    fn newly_explicit_existing_protected_path_keeps_the_incremental_snapshot() {
        let workspace = tempfile::tempdir().expect("workspace");
        let protected = workspace.path().join(".agents");
        std::fs::create_dir(&protected).expect("protected directory");
        std::fs::write(protected.join("value.txt"), b"secret").expect("protected content");
        let before = snapshot_workspace(workspace.path()).expect("before snapshot");
        assert!(before.protected_paths.contains(".agents"));

        let IncrementalSnapshot::Updated(after, work) = update_workspace_snapshot_as_sandbox_user(
            workspace.path(),
            &before,
            &WorkspaceChangeObservation::Unchanged,
            std::slice::from_ref(&protected),
        )
        .expect("expanded protected set") else {
            panic!("an already-opaque path must not require a full snapshot");
        };

        assert!(after.explicit_protected_paths.contains(".agents"));
        assert_eq!(work.entries_read, 1);
        assert_eq!(work.content_bytes_read, 0);
    }

    #[cfg(windows)]
    #[test]
    fn unchanged_observation_fails_closed_when_the_root_disappears() {
        let workspace = tempfile::tempdir().expect("workspace");
        std::fs::write(workspace.path().join("value.txt"), b"value").expect("write file");
        let before = snapshot_workspace(workspace.path()).expect("before snapshot");
        let removed = workspace.path().to_path_buf();
        workspace.close().expect("remove workspace");

        let error = match update_workspace_snapshot_as_sandbox_user(
            &removed,
            &before,
            &WorkspaceChangeObservation::Unchanged,
            &[],
        ) {
            Ok(_) => panic!("missing root must fail closed"),
            Err(error) => error,
        };
        assert!(error.contains("root revalidation failed"));
    }

    #[cfg(windows)]
    #[test]
    fn changed_observation_rejects_root_replacement() {
        let parent = tempfile::tempdir().expect("parent");
        let workspace = parent.path().join("workspace");
        let displaced = parent.path().join("displaced");
        std::fs::create_dir(&workspace).expect("workspace");
        std::fs::write(workspace.join("value.txt"), b"value").expect("value");
        let before = snapshot_workspace(&workspace).expect("before snapshot");
        std::fs::rename(&workspace, &displaced).expect("displace root");
        std::fs::create_dir(&workspace).expect("replacement root");
        std::fs::write(workspace.join("value.txt"), b"value").expect("replacement value");
        let observation = WorkspaceChangeObservation::Changed(vec![WorkspacePathChange {
            path: "value.txt".to_string(),
            kind: WorkspacePathChangeKind::Modified,
        }]);

        let error =
            match update_workspace_snapshot_as_sandbox_user(&workspace, &before, &observation, &[])
            {
                Ok(_) => panic!("root replacement must fail closed"),
                Err(error) => error,
            };
        assert!(error.contains("root identity or behavior drifted"));
    }

    #[cfg(windows)]
    #[test]
    fn unchanged_observation_rejects_same_content_root_replacement() {
        let parent = tempfile::tempdir().expect("parent");
        let workspace = parent.path().join("workspace");
        let displaced = parent.path().join("displaced");
        std::fs::create_dir(&workspace).expect("workspace");
        std::fs::write(workspace.join("value.txt"), b"value").expect("value");
        let before = snapshot_workspace(&workspace).expect("before snapshot");
        std::fs::rename(&workspace, &displaced).expect("displace root");
        std::fs::create_dir(&workspace).expect("replacement root");
        std::fs::write(workspace.join("value.txt"), b"value").expect("replacement value");

        let error = match update_workspace_snapshot_as_sandbox_user(
            &workspace,
            &before,
            &WorkspaceChangeObservation::Unchanged,
            &[],
        ) {
            Ok(_) => panic!("replacement root must fail closed even without path hints"),
            Err(error) => error,
        };
        assert!(error.contains("root identity or behavior drifted"));
    }

    #[cfg(windows)]
    #[test]
    fn incomplete_observations_do_not_hide_root_replacement() {
        let observations = [
            WorkspaceChangeObservation::Unknown,
            WorkspaceChangeObservation::Changed(vec![WorkspacePathChange {
                path: "old.txt".to_string(),
                kind: WorkspacePathChangeKind::RenamedOld,
            }]),
            WorkspaceChangeObservation::Changed(vec![WorkspacePathChange {
                path: ".git/config".to_string(),
                kind: WorkspacePathChangeKind::Modified,
            }]),
        ];
        for observation in observations {
            let parent = tempfile::tempdir().expect("parent");
            let workspace = parent.path().join("workspace");
            let displaced = parent.path().join("displaced");
            std::fs::create_dir(&workspace).expect("workspace");
            std::fs::write(workspace.join("value.txt"), b"value").expect("value");
            let before = snapshot_workspace(&workspace).expect("before snapshot");
            std::fs::rename(&workspace, &displaced).expect("displace root");
            std::fs::create_dir(&workspace).expect("replacement root");
            std::fs::write(workspace.join("value.txt"), b"value").expect("replacement value");

            let error = match update_workspace_snapshot_as_sandbox_user(
                &workspace,
                &before,
                &observation,
                &[],
            ) {
                Ok(_) => panic!("incomplete observation must not hide root replacement"),
                Err(error) => error,
            };
            assert!(error.contains("root identity or behavior drifted"));
        }
    }

    #[cfg(windows)]
    #[test]
    fn single_file_observation_refreshes_only_the_changed_snapshot_entry() {
        let workspace = tempfile::tempdir().expect("workspace");
        std::fs::write(workspace.path().join("changed.txt"), b"before").expect("write changed");
        std::fs::write(workspace.path().join("stable.txt"), b"stable").expect("write stable");
        let before = snapshot_workspace(workspace.path()).expect("before snapshot");
        std::fs::write(workspace.path().join("changed.txt"), b"after").expect("modify file");
        let observation = WorkspaceChangeObservation::Changed(vec![WorkspacePathChange {
            path: "changed.txt".to_string(),
            kind: WorkspacePathChangeKind::Modified,
        }]);

        let IncrementalSnapshot::Updated(after, work) =
            update_workspace_snapshot_as_sandbox_user(workspace.path(), &before, &observation, &[])
                .expect("incremental snapshot")
        else {
            panic!("single file change must remain incremental");
        };
        let changed = before.changed_paths(&after).expect("changed paths");
        assert!(changed.contains(&"changed.txt".to_string()));
        assert!(!changed.contains(&"stable.txt".to_string()));
        assert_eq!(work.entries_read, 2);
        assert_eq!(work.content_bytes_read, b"after".len() as u64);
    }

    #[cfg(windows)]
    #[test]
    fn added_directory_subtree_is_captured_once_without_rereading_existing_files() {
        const FILES: usize = 2_001;
        let workspace = tempfile::tempdir().expect("workspace");
        std::fs::write(workspace.path().join("stable.txt"), b"stable").expect("stable file");
        let before = snapshot_workspace(workspace.path()).expect("before snapshot");
        let added = workspace.path().join("new-dir");
        std::fs::create_dir(&added).expect("new directory");
        for index in 0..FILES {
            std::fs::write(added.join(format!("file-{index:04}.txt")), b"x").expect("added file");
        }

        let mut changes = vec![WorkspacePathChange {
            path: "new-dir".to_string(),
            kind: WorkspacePathChangeKind::Added,
        }];
        changes.extend((0..FILES).map(|index| WorkspacePathChange {
            path: format!("new-dir/file-{index:04}.txt"),
            kind: WorkspacePathChangeKind::Added,
        }));
        let IncrementalSnapshot::Updated(after, work) = update_workspace_snapshot_as_sandbox_user(
            workspace.path(),
            &before,
            &WorkspaceChangeObservation::Changed(changes),
            &[],
        )
        .expect("new directory outcome") else {
            panic!("a complete added subtree must remain incremental");
        };
        assert_eq!(
            after,
            snapshot_workspace(workspace.path()).expect("full comparison")
        );
        assert_eq!(work.entries_read, FILES + 2);
        assert_eq!(work.content_bytes_read, FILES as u64);
    }

    #[cfg(windows)]
    #[test]
    fn added_subtree_still_discovers_nested_protected_paths() {
        let workspace = tempfile::tempdir().expect("workspace");
        let before = snapshot_workspace(workspace.path()).expect("before snapshot");
        let protected = workspace.path().join("generated").join(".ssh");
        std::fs::create_dir_all(&protected).expect("protected directory");
        std::fs::write(protected.join("secret"), b"secret").expect("protected content");

        let IncrementalSnapshot::Updated(after, _) = update_workspace_snapshot_as_sandbox_user(
            workspace.path(),
            &before,
            &WorkspaceChangeObservation::Changed(vec![WorkspacePathChange {
                path: "generated".to_string(),
                kind: WorkspacePathChangeKind::Added,
            }]),
            &[],
        )
        .expect("incremental snapshot") else {
            panic!("a complete added subtree must remain incremental");
        };

        assert!(after.protected_paths.contains("generated/.ssh"));
        assert!(
            after
                .entries
                .keys()
                .all(|path| !path.starts_with("generated/.ssh/"))
        );
        assert_eq!(before.observed_change(&after), Ok((true, None)));
    }

    #[cfg(windows)]
    #[test]
    fn unknown_observation_requires_a_full_snapshot() {
        let workspace = tempfile::tempdir().expect("workspace");
        let before = snapshot_workspace(workspace.path()).expect("before snapshot");
        assert!(matches!(
            update_workspace_snapshot_as_sandbox_user(
                workspace.path(),
                &before,
                &WorkspaceChangeObservation::Unknown,
                &[],
            )
            .expect("unknown outcome"),
            IncrementalSnapshot::FullRequired
        ));
    }

    #[cfg(windows)]
    #[test]
    fn hardlink_change_requires_a_full_snapshot_for_peer_metadata() {
        let workspace = tempfile::tempdir().expect("workspace");
        std::fs::write(workspace.path().join("first.txt"), b"value").expect("write source");
        let before = snapshot_workspace(workspace.path()).expect("before snapshot");
        std::fs::hard_link(
            workspace.path().join("first.txt"),
            workspace.path().join("second.txt"),
        )
        .expect("create hardlink");
        let observation = WorkspaceChangeObservation::Changed(vec![WorkspacePathChange {
            path: "second.txt".to_string(),
            kind: WorkspacePathChangeKind::Added,
        }]);

        assert!(matches!(
            update_workspace_snapshot_as_sandbox_user(
                workspace.path(),
                &before,
                &observation,
                &[],
            )
            .expect("hardlink outcome"),
            IncrementalSnapshot::FullRequired
        ));
    }
}
