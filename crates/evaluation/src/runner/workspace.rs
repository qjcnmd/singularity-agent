//! Evaluation workspace 的安全复制、完整快照和变更归因。

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
#[cfg(unix)]
use std::os::unix::fs::symlink;

use cap_fs_ext::{DirExt as _, FollowSymlinks, MetadataExt as _, OpenOptionsFollowExt as _};
use cap_std::ambient_authority;
#[cfg(unix)]
use cap_std::fs::PermissionsExt as _;
use cap_std::fs::{Dir, Metadata, OpenOptions as CapOpenOptions};
use serde::Serialize;
use sha2::{Digest, Sha256};
use singularity_core::CancellationToken;
use singularity_sandbox::is_toolchain_artifact_path;

#[cfg(windows)]
const REPARSE_POINT_ATTRIBUTE: u32 = 0x0400;

const GENERATED_DIRECTORIES: &[&str] = &[".git", ".venv", "target", "node_modules"];

pub(crate) type WorkspaceSnapshot = BTreeMap<String, WorkspaceSnapshotEntry>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(super) struct WorkspaceSnapshotEntry {
    kind: WorkspaceSnapshotEntryKind,
    content_digest: Option<String>,
    platform_permissions: u32,
    length: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum WorkspaceSnapshotEntryKind {
    Directory,
    File,
    Symlink,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct WorkspaceRootIdentity {
    device: u64,
    object: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
/// 单个工作区变更及其前后摘要。
pub(super) struct WorkspaceChangeEvidence {
    pub path: String,
    pub change_kind: &'static str,
    pub before_sha256: Option<String>,
    pub after_sha256: Option<String>,
}

/// 复制一棵工作区并返回复制前确认的完整快照。
///
/// 源树通过 capability directory 以不跟随链接的方式读取；复制期间任何源对象替换或
/// 内容变化都会失败关闭。生成目录不进入快照，也不会被复制。
pub(super) fn copy_tree_checked(
    source: &Path,
    destination: &Path,
) -> Result<WorkspaceSnapshot, String> {
    let source_root = open_workspace_root(source)?;
    let source_metadata = source_root
        .dir_metadata()
        .map_err(|error| format!("failed to inspect workspace {}: {error}", source.display()))?;
    let mut snapshot = BTreeMap::new();
    snapshot.insert(
        ".".to_string(),
        snapshot_entry_from_cap(&source_metadata, None)?,
    );
    capture_source_entries(&source_root, "", &mut snapshot)?;

    let source_for_overlap = canonical_or_original(source);
    let destination = prepare_destination(&source_for_overlap, destination)?;
    copy_snapshot_entries(&source_root, &snapshot, &destination)?;

    let source_after = open_workspace_root(source)?
        .dir_metadata()
        .map_err(|error| {
            format!(
                "failed to revalidate workspace root {}: {error}",
                source.display()
            )
        })?;
    if !metadata_matches(&source_metadata, &source_after) {
        let _ = remove_partial_tree(&destination);
        return Err("workspace changed while it was being copied".to_string());
    }
    Ok(snapshot)
}

/// Materialize one isolated trial from a fresh complete source copy.
pub(super) fn materialize_prepared_workspace(
    source: &Path,
    destination: &Path,
    expected: &WorkspaceSnapshot,
    cancellation: &CancellationToken,
) -> Result<(), String> {
    if cancellation.is_cancelled() {
        return Err("evaluation cancelled before workspace materialization".to_string());
    }
    let actual = copy_tree_checked(source, destination)?;
    if cancellation.is_cancelled() {
        let _ = remove_partial_tree(destination);
        return Err("evaluation cancelled during workspace materialization".to_string());
    }
    if &actual != expected {
        let _ = remove_partial_tree(destination);
        return Err("prepared source changed before full workspace materialization".to_string());
    }
    Ok(())
}

/// Materialize a preparation tree using the same complete-copy path as trials.
pub(super) fn copy_tree_for_preparation(source: &Path, destination: &Path) -> Result<(), String> {
    copy_tree_checked(source, destination).map(|_| ())
}

fn prepare_destination(source: &Path, destination: &Path) -> Result<PathBuf, String> {
    if destination.exists() {
        return Err(format!(
            "evaluation workspace destination already exists: {}",
            destination.display()
        ));
    }
    let parent = destination.parent().ok_or_else(|| {
        format!(
            "workspace destination has no parent: {}",
            destination.display()
        )
    })?;
    let parent = fs::canonicalize(parent).map_err(|error| {
        format!(
            "failed to resolve workspace destination parent {}: {error}",
            parent.display()
        )
    })?;
    let name = destination.file_name().ok_or_else(|| {
        format!(
            "workspace destination has no file name: {}",
            destination.display()
        )
    })?;
    let destination = parent.join(name);
    if destination.starts_with(source) || source.starts_with(&destination) {
        return Err(format!(
            "evaluation workspace source and destination overlap: {} -> {}",
            source.display(),
            destination.display()
        ));
    }
    fs::create_dir(&destination).map_err(|error| {
        format!(
            "failed to create workspace destination {}: {error}",
            destination.display()
        )
    })?;
    Ok(destination)
}

fn copy_snapshot_entries(
    source_root: &Dir,
    snapshot: &WorkspaceSnapshot,
    destination: &Path,
) -> Result<(), String> {
    let root = snapshot
        .get(".")
        .ok_or_else(|| "workspace snapshot has no root entry".to_string())?;
    let mut directories = Vec::new();
    for (relative, entry) in snapshot {
        if relative == "." {
            continue;
        }
        let destination_path = destination.join(relative);
        match entry.kind {
            WorkspaceSnapshotEntryKind::Directory => {
                fs::create_dir(&destination_path).map_err(|error| {
                    format!(
                        "failed to create workspace directory {}: {error}",
                        destination_path.display()
                    )
                })?;
                directories.push((destination_path, entry));
            }
            WorkspaceSnapshotEntryKind::File | WorkspaceSnapshotEntryKind::Symlink => {
                let (parent, name) = open_relative_parent(source_root, relative)?;
                let metadata = parent.symlink_metadata(&name).map_err(|error| {
                    format!("failed to inspect workspace path {relative}: {error}")
                })?;
                if is_reparse_point_cap(&metadata) {
                    return Err(format!(
                        "workspace contains an unsupported reparse point: {relative}"
                    ));
                }
                match entry.kind {
                    WorkspaceSnapshotEntryKind::File => {
                        let mut options = CapOpenOptions::new();
                        options.read(true).follow(FollowSymlinks::No);
                        let mut input = parent.open_with(&name, &options).map_err(|error| {
                            format!("failed to open workspace file {relative}: {error}")
                        })?;
                        let before = input.metadata().map_err(|error| {
                            format!("failed to inspect workspace file {relative}: {error}")
                        })?;
                        if !metadata_matches(&metadata, &before) {
                            return Err(format!("workspace path changed while copying {relative}"));
                        }
                        let mut output = File::create(destination.join(relative)).map_err(|error| {
                            format!("failed to create materialized workspace file {relative}: {error}")
                        })?;
                        let length = copy_file_bytes(&mut input, &mut output, relative)?;
                        output.flush().map_err(|error| {
                            format!(
                                "failed to flush materialized workspace file {relative}: {error}"
                            )
                        })?;
                        validate_opened_file_capture(
                            &parent, &name, &input, &before, length, relative,
                        )?;
                        if entry.content_digest.is_none() || entry.length != length {
                            return Err(format!("workspace file changed while copying {relative}"));
                        }
                        set_snapshot_permissions(&destination.join(relative), entry)?;
                    }
                    WorkspaceSnapshotEntryKind::Symlink => {
                        #[cfg(unix)]
                        {
                            let target = parent.read_link_contents(&name).map_err(|error| {
                                format!("failed to read workspace link {relative}: {error}")
                            })?;
                            let target_bytes = target.as_os_str().as_bytes();
                            create_symlink_from_payload(target_bytes, &destination.join(relative))?;
                            let after = parent.symlink_metadata(&name).map_err(|error| {
                                format!("failed to revalidate workspace link {relative}: {error}")
                            })?;
                            if !metadata_matches(&metadata, &after) {
                                return Err(format!(
                                    "workspace link changed while copying {relative}"
                                ));
                            }
                        }
                        #[cfg(not(unix))]
                        {
                            return Err(format!(
                                "workspace contains an unsupported reparse point: {relative}"
                            ));
                        }
                    }
                    WorkspaceSnapshotEntryKind::Directory => unreachable!(),
                }
            }
        }
    }
    directories.sort_by_key(|(path, _)| std::cmp::Reverse(path.components().count()));
    for (path, entry) in directories {
        set_snapshot_permissions(&path, entry)?;
    }
    set_snapshot_permissions(destination, root)?;
    Ok(())
}

fn copy_file_bytes(
    input: &mut cap_std::fs::File,
    output: &mut File,
    relative: &str,
) -> Result<u64, String> {
    let mut buffer = [0u8; 64 * 1024];
    let mut length = 0u64;
    loop {
        let read = input
            .read(&mut buffer)
            .map_err(|error| format!("failed to read workspace file {relative}: {error}"))?;
        if read == 0 {
            break;
        }
        output.write_all(&buffer[..read]).map_err(|error| {
            format!("failed to write materialized workspace file {relative}: {error}")
        })?;
        length = length
            .checked_add(u64::try_from(read).unwrap_or(u64::MAX))
            .ok_or_else(|| "workspace copy byte count overflowed".to_string())?;
    }
    Ok(length)
}

fn capture_source_entries(
    directory: &Dir,
    prefix: &str,
    snapshot: &mut WorkspaceSnapshot,
) -> Result<(), String> {
    for entry in directory
        .read_dir(".")
        .map_err(|error| format!("failed to read workspace directory: {error}"))?
    {
        let entry = entry.map_err(|error| error.to_string())?;
        let name = entry.file_name();
        if name.to_str().is_some_and(is_generated_directory) {
            continue;
        }
        let relative = if prefix.is_empty() {
            name.to_string_lossy().into_owned()
        } else {
            format!("{prefix}/{}", name.to_string_lossy())
        };
        let path_metadata = directory
            .symlink_metadata(&name)
            .map_err(|error| format!("failed to inspect workspace path {relative}: {error}"))?;
        if is_reparse_point_cap(&path_metadata) {
            return Err(format!(
                "workspace contains an unsupported reparse point: {relative}"
            ));
        }
        if path_metadata.is_dir() {
            let child = directory.open_dir_nofollow(&name).map_err(|error| {
                format!("failed to open workspace directory {relative}: {error}")
            })?;
            let child_before = child.dir_metadata().map_err(|error| {
                format!("failed to inspect workspace directory {relative}: {error}")
            })?;
            if !metadata_matches(&path_metadata, &child_before) {
                return Err(format!(
                    "workspace path changed while opening directory {relative}"
                ));
            }
            snapshot.insert(
                relative.clone(),
                snapshot_entry_from_cap(&child_before, None)?,
            );
            capture_source_entries(&child, &relative, snapshot)?;
            let child_after = child.dir_metadata().map_err(|error| {
                format!("failed to revalidate workspace directory {relative}: {error}")
            })?;
            let path_after = directory.symlink_metadata(&name).map_err(|error| {
                format!("failed to revalidate workspace path {relative}: {error}")
            })?;
            if !metadata_matches(&child_before, &child_after)
                || !metadata_matches(&child_before, &path_after)
            {
                return Err(format!(
                    "workspace changed while directory {relative} was captured"
                ));
            }
        } else if path_metadata.is_file() {
            let mut options = CapOpenOptions::new();
            options.read(true).follow(FollowSymlinks::No);
            let mut file = directory
                .open_with(&name, &options)
                .map_err(|error| format!("failed to open workspace file {relative}: {error}"))?;
            let before = file
                .metadata()
                .map_err(|error| format!("failed to inspect workspace file {relative}: {error}"))?;
            if !metadata_matches(&path_metadata, &before) {
                return Err(format!(
                    "workspace path changed while opening file {relative}"
                ));
            }
            let (digest, length) = digest_file(&mut file, &relative)?;
            validate_opened_file_capture(directory, &name, &file, &before, length, &relative)?;
            snapshot.insert(relative, snapshot_entry_from_cap(&before, Some(digest))?);
        } else if path_metadata.is_symlink() {
            #[cfg(unix)]
            {
                let (digest, length) = digest_symlink(directory, &name, &relative)?;
                let path_after = directory.symlink_metadata(&name).map_err(|error| {
                    format!("failed to revalidate workspace link {relative}: {error}")
                })?;
                if !metadata_matches(&path_metadata, &path_after) || length != path_metadata.len() {
                    return Err(format!(
                        "workspace link changed while it was captured: {relative}"
                    ));
                }
                snapshot.insert(
                    relative,
                    snapshot_entry_from_cap(&path_metadata, Some(digest))?,
                );
            }
            #[cfg(not(unix))]
            {
                return Err(format!(
                    "workspace contains an unsupported reparse point: {relative}"
                ));
            }
        } else {
            return Err(format!(
                "workspace contains a non-regular entry: {relative}"
            ));
        }
    }
    Ok(())
}

fn is_generated_directory(name: &str) -> bool {
    GENERATED_DIRECTORIES.contains(&name)
}

fn digest_file(source: &mut cap_std::fs::File, relative: &str) -> Result<(String, u64), String> {
    let mut digest = Sha256::new();
    digest.update(b"file\0");
    let mut buffer = [0u8; 64 * 1024];
    let mut length = 0u64;
    loop {
        let read = source
            .read(&mut buffer)
            .map_err(|error| format!("failed to read workspace file {relative}: {error}"))?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
        length = length
            .checked_add(u64::try_from(read).unwrap_or(u64::MAX))
            .ok_or_else(|| "workspace snapshot byte count overflowed".to_string())?;
    }
    Ok((format!("sha256:{:x}", digest.finalize()), length))
}

#[cfg(unix)]
fn digest_symlink(directory: &Dir, name: &OsStr, relative: &str) -> Result<(String, u64), String> {
    let target = directory
        .read_link_contents(name)
        .map_err(|error| format!("failed to read workspace link {relative}: {error}"))?;
    let target_after = directory
        .read_link_contents(name)
        .map_err(|error| format!("failed to revalidate workspace link {relative}: {error}"))?;
    if target != target_after {
        return Err(format!(
            "workspace link changed while it was captured: {relative}"
        ));
    }
    let bytes = target.as_os_str().as_bytes();
    let mut digest = Sha256::new();
    digest.update(b"symlink\0");
    digest.update(bytes);
    let length = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    Ok((format!("sha256:{:x}", digest.finalize()), length))
}

/// 对工作区文件生成相对路径到摘要的完整快照。
pub(crate) fn snapshot_workspace(root: &Path) -> Result<WorkspaceSnapshot, String> {
    let root_dir = open_workspace_root(root)?;
    let metadata = root_dir
        .dir_metadata()
        .map_err(|error| format!("failed to inspect workspace {}: {error}", root.display()))?;
    let mut snapshot = BTreeMap::new();
    snapshot.insert(".".to_string(), snapshot_entry_from_cap(&metadata, None)?);
    capture_source_entries(&root_dir, "", &mut snapshot)?;
    let root_after = root_dir
        .dir_metadata()
        .map_err(|error| format!("failed to revalidate workspace root: {error}"))?;
    let path_after = open_workspace_root(root)
        .and_then(|root| root.dir_metadata().map_err(|error| error.to_string()))
        .map_err(|error| format!("failed to revalidate workspace root path: {error}"))?;
    if !metadata_matches(&metadata, &root_after) || !metadata_matches(&metadata, &path_after) {
        return Err("workspace changed while its snapshot was being captured".to_string());
    }
    Ok(snapshot)
}

pub(super) fn workspace_root_identity(path: &Path) -> Result<WorkspaceRootIdentity, String> {
    let root = open_workspace_root(path)?;
    let metadata = root
        .dir_metadata()
        .map_err(|error| format!("failed to inspect workspace root identity: {error}"))?;
    Ok(WorkspaceRootIdentity {
        device: metadata.dev(),
        object: metadata.ino(),
    })
}

fn open_workspace_root(path: &Path) -> Result<Dir, String> {
    let parent_path = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .ok_or_else(|| format!("workspace root has no final component: {}", path.display()))?;
    let parent = Dir::open_ambient_dir(parent_path, ambient_authority())
        .map_err(|error| format!("failed to open workspace root parent: {error}"))?;
    let path_metadata = parent.symlink_metadata(name).map_err(|error| {
        format!(
            "failed to inspect workspace root path {}: {error}",
            path.display()
        )
    })?;
    if is_reparse_point_cap(&path_metadata) || !path_metadata.is_dir() {
        return Err(format!(
            "workspace root is not a regular directory: {}",
            path.display()
        ));
    }
    let root = parent.open_dir_nofollow(name).map_err(|error| {
        format!("failed to open workspace root without following links: {error}")
    })?;
    let root_metadata = root
        .dir_metadata()
        .map_err(|error| format!("failed to inspect workspace root handle: {error}"))?;
    if !metadata_matches(&path_metadata, &root_metadata) {
        return Err("workspace root changed while it was being opened".to_string());
    }
    Ok(root)
}

fn open_relative_parent(root: &Dir, relative: &str) -> Result<(Dir, std::ffi::OsString), String> {
    let mut components = relative.split('/').peekable();
    let mut directory = root
        .try_clone()
        .map_err(|error| format!("workspace root clone failed: {error}"))?;
    while let Some(component) = components.next() {
        if components.peek().is_none() {
            return Ok((directory, std::ffi::OsString::from(component)));
        }
        directory = directory
            .open_dir_nofollow(component)
            .map_err(|error| format!("workspace parent open failed for {relative}: {error}"))?;
    }
    Err("workspace path is empty".to_string())
}

fn snapshot_entry_from_cap(
    metadata: &Metadata,
    content_digest: Option<String>,
) -> Result<WorkspaceSnapshotEntry, String> {
    let kind = if metadata.is_symlink() {
        WorkspaceSnapshotEntryKind::Symlink
    } else if metadata.is_dir() {
        WorkspaceSnapshotEntryKind::Directory
    } else if metadata.is_file() {
        WorkspaceSnapshotEntryKind::File
    } else {
        return Err("workspace snapshot encountered a non-regular entry".to_string());
    };
    Ok(WorkspaceSnapshotEntry {
        kind,
        content_digest,
        platform_permissions: platform_permissions_cap(metadata),
        length: if kind == WorkspaceSnapshotEntryKind::Directory {
            0
        } else {
            metadata.len()
        },
    })
}

fn metadata_identity(metadata: &Metadata) -> (u64, u64) {
    (metadata.dev(), metadata.ino())
}

fn metadata_matches(before: &Metadata, after: &Metadata) -> bool {
    metadata_identity(before) == metadata_identity(after)
        && before.is_dir() == after.is_dir()
        && before.is_file() == after.is_file()
        && before.is_symlink() == after.is_symlink()
        && before.len() == after.len()
        && platform_permissions_cap(before) == platform_permissions_cap(after)
}

fn platform_permissions_cap(metadata: &Metadata) -> u32 {
    #[cfg(unix)]
    {
        metadata.permissions().mode()
    }
    #[cfg(not(unix))]
    {
        u32::from(metadata.permissions().readonly())
    }
}

fn set_snapshot_permissions(path: &Path, entry: &WorkspaceSnapshotEntry) -> Result<(), String> {
    let mut permissions = fs::metadata(path)
        .map_err(|error| format!("failed to inspect {} permissions: {error}", path.display()))?
        .permissions();
    #[cfg(unix)]
    permissions.set_mode(entry.platform_permissions);
    #[cfg(not(unix))]
    permissions.set_readonly(entry.platform_permissions & 0x1 != 0);
    fs::set_permissions(path, permissions)
        .map_err(|error| format!("failed to set {} permissions: {error}", path.display()))
}

fn validate_opened_file_capture(
    directory: &Dir,
    name: &OsStr,
    file: &cap_std::fs::File,
    before: &Metadata,
    length: u64,
    relative: &str,
) -> Result<(), String> {
    let after = file
        .metadata()
        .map_err(|error| format!("failed to revalidate workspace file {relative}: {error}"))?;
    let path_after = directory
        .symlink_metadata(name)
        .map_err(|error| format!("failed to revalidate workspace path {relative}: {error}"))?;
    if !metadata_matches(before, &after) || !metadata_matches(before, &path_after) {
        return Err(format!(
            "workspace changed while file {relative} was captured"
        ));
    }
    if length != before.len() {
        return Err(format!(
            "workspace file length changed while it was captured: {relative}"
        ));
    }
    Ok(())
}

fn remove_partial_tree(path: &Path) -> Result<(), String> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!(
            "failed to clean partial workspace {}: {error}",
            path.display()
        )),
    }
}

#[cfg(unix)]
fn create_symlink_from_payload(target: &[u8], destination: &Path) -> Result<(), String> {
    let target = std::ffi::OsString::from_vec(target.to_vec());
    symlink(&target, destination).map_err(|error| {
        format!(
            "failed to materialize workspace link {}: {error}",
            destination.display()
        )
    })
}

#[cfg(windows)]
fn is_reparse_point_cap(metadata: &Metadata) -> bool {
    metadata.file_type().is_symlink()
        || cap_std::fs::MetadataExt::file_attributes(metadata) & REPARSE_POINT_ATTRIBUTE != 0
}

#[cfg(not(windows))]
fn is_reparse_point_cap(_metadata: &Metadata) -> bool {
    false
}

pub(crate) fn workspace_snapshot_digest(snapshot: &WorkspaceSnapshot) -> Result<String, String> {
    let canonical = serde_json::to_vec(snapshot)
        .map_err(|error| format!("failed to serialize workspace snapshot: {error}"))?;
    Ok(format!("sha256:{:x}", Sha256::digest(canonical)))
}

pub(super) fn changed_paths(before: &WorkspaceSnapshot, after: &WorkspaceSnapshot) -> Vec<String> {
    before
        .keys()
        .chain(after.keys())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter(|path| before.get(*path) != after.get(*path))
        .cloned()
        .collect()
}

pub(super) fn evaluation_changed_paths(
    before: &WorkspaceSnapshot,
    after: &WorkspaceSnapshot,
    pristine_source: &WorkspaceSnapshot,
) -> Vec<String> {
    let changed = changed_paths(before, after);
    changed
        .iter()
        .filter(|path| {
            !is_new_changed_directory_ancestor(before, after, &changed, path)
                && (pristine_source.contains_key(path.as_str())
                    || !is_toolchain_artifact_path(path))
        })
        .cloned()
        .collect()
}

fn is_new_changed_directory_ancestor(
    before: &WorkspaceSnapshot,
    after: &WorkspaceSnapshot,
    changed: &[String],
    path: &str,
) -> bool {
    if before.contains_key(path)
        || after.get(path).map(|entry| entry.kind) != Some(WorkspaceSnapshotEntryKind::Directory)
    {
        return false;
    }
    let prefix = format!("{path}/");
    changed
        .iter()
        .any(|candidate| candidate.starts_with(&prefix))
}

pub(super) fn workspace_change_evidence(
    before: &WorkspaceSnapshot,
    after: &WorkspaceSnapshot,
    pristine_source: &WorkspaceSnapshot,
) -> Vec<WorkspaceChangeEvidence> {
    evaluation_changed_paths(before, after, pristine_source)
        .into_iter()
        .map(|path| WorkspaceChangeEvidence {
            change_kind: match (before.contains_key(&path), after.contains_key(&path)) {
                (false, true) => "added",
                (true, false) => "deleted",
                (true, true) => "modified",
                (false, false) => unreachable!("changed path must exist in one snapshot"),
            },
            before_sha256: before
                .get(&path)
                .and_then(|entry| entry.content_digest.clone()),
            after_sha256: after
                .get(&path)
                .and_then(|entry| entry.content_digest.clone()),
            path,
        })
        .collect()
}

pub(super) fn patch_evidence_digest(evidence: &[WorkspaceChangeEvidence]) -> Option<String> {
    if evidence.is_empty() {
        return None;
    }
    let canonical = serde_json::to_vec(evidence).expect("workspace evidence serializes");
    Some(format!("sha256:{:x}", Sha256::digest(canonical)))
}

pub(super) fn canonical_or_original(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_excludes_generated_directories() {
        let temp = tempfile::tempdir().expect("tempdir");
        fs::create_dir(temp.path().join(".git")).expect(".git");
        fs::create_dir(temp.path().join(".venv")).expect(".venv");
        fs::create_dir(temp.path().join("target")).expect("target");
        fs::create_dir(temp.path().join("node_modules")).expect("node_modules");
        fs::write(temp.path().join("kept.txt"), "kept").expect("file");

        let snapshot = snapshot_workspace(temp.path()).expect("snapshot");
        assert!(snapshot.contains_key("kept.txt"));
        for generated in GENERATED_DIRECTORIES {
            assert!(!snapshot.contains_key(*generated));
        }
    }

    #[test]
    fn change_evidence_keeps_added_deleted_and_modified_files() {
        let source = tempfile::tempdir().expect("source");
        fs::write(source.path().join("same.txt"), "same").expect("same");
        fs::write(source.path().join("removed.txt"), "removed").expect("removed");
        let before = snapshot_workspace(source.path()).expect("before");
        fs::write(source.path().join("same.txt"), "changed").expect("changed");
        fs::remove_file(source.path().join("removed.txt")).expect("remove");
        fs::write(source.path().join("added.txt"), "added").expect("added");
        let after = snapshot_workspace(source.path()).expect("after");
        let evidence = workspace_change_evidence(&before, &after, &before);
        assert_eq!(
            evidence
                .iter()
                .find(|entry| entry.path == "same.txt")
                .unwrap()
                .change_kind,
            "modified"
        );
        assert_eq!(
            evidence
                .iter()
                .find(|entry| entry.path == "removed.txt")
                .unwrap()
                .change_kind,
            "deleted"
        );
        assert_eq!(
            evidence
                .iter()
                .find(|entry| entry.path == "added.txt")
                .unwrap()
                .change_kind,
            "added"
        );
    }

    #[test]
    fn copy_rejects_source_destination_overlap() {
        let source = tempfile::tempdir().expect("source");
        let destination = source.path().join("child");
        let error = copy_tree_checked(source.path(), &destination).expect_err("overlap");
        assert!(error.contains("overlap"));
    }
}
