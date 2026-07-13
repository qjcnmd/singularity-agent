use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};

use serde::Serialize;
use sha2::{Digest, Sha256};
const REPARSE_POINT_ATTRIBUTE: u32 = 0x0400;
type WorkspaceSnapshot = BTreeMap<String, String>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(super) struct WorkspaceChangeEvidence {
    pub path: String,
    pub change_kind: &'static str,
    pub before_sha256: Option<String>,
    pub after_sha256: Option<String>,
    pub allowed: bool,
}

pub(super) fn apply_agent_changes(
    agent_workspace: &Path,
    destination: &Path,
    changed_files: &[String],
) -> Result<(), String> {
    for relative in changed_files {
        let source = agent_workspace.join(relative);
        let target = destination.join(relative);
        match fs::symlink_metadata(&source) {
            Ok(metadata) => {
                if is_reparse_point(&metadata) || !metadata.is_file() {
                    return Err(format!(
                        "agent change is not a regular file: {}",
                        source.display()
                    ));
                }
                let parent = target
                    .parent()
                    .ok_or_else(|| format!("agent change has no destination parent: {relative}"))?;
                fs::create_dir_all(parent).map_err(|error| {
                    format!(
                        "failed to create agent change parent {}: {error}",
                        parent.display()
                    )
                })?;
                if target.exists()
                    && !fs::symlink_metadata(&target)
                        .map_err(|error| error.to_string())?
                        .is_file()
                {
                    return Err(format!(
                        "agent change target is not a file: {}",
                        target.display()
                    ));
                }
                fs::copy(&source, &target).map_err(|error| {
                    format!(
                        "failed to copy agent change {} to {}: {error}",
                        source.display(),
                        target.display()
                    )
                })?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if target.exists() {
                    let metadata =
                        fs::symlink_metadata(&target).map_err(|error| error.to_string())?;
                    if is_reparse_point(&metadata) || !metadata.is_file() {
                        return Err(format!(
                            "deleted agent change target is not a regular file: {}",
                            target.display()
                        ));
                    }
                    fs::remove_file(&target).map_err(|error| {
                        format!(
                            "failed to apply agent deletion {}: {error}",
                            target.display()
                        )
                    })?;
                }
            }
            Err(error) => {
                return Err(format!(
                    "failed to inspect agent change {}: {error}",
                    source.display()
                ));
            }
        }
    }
    Ok(())
}
pub(super) fn copy_tree_checked(source: &Path, destination: &Path) -> Result<(), String> {
    let metadata = fs::symlink_metadata(source)
        .map_err(|error| format!("failed to inspect source {}: {error}", source.display()))?;
    if is_reparse_point(&metadata) || !metadata.is_dir() {
        return Err(format!(
            "evaluation workspace source is not a regular directory: {}",
            source.display()
        ));
    }
    if destination.exists() {
        return Err(format!(
            "evaluation workspace destination already exists: {}",
            destination.display()
        ));
    }
    let source = fs::canonicalize(source).map_err(|error| {
        format!(
            "failed to resolve evaluation workspace source {}: {error}",
            source.display()
        )
    })?;
    let destination_parent = destination.parent().ok_or_else(|| {
        format!(
            "workspace destination has no parent: {}",
            destination.display()
        )
    })?;
    let destination_parent = fs::canonicalize(destination_parent).map_err(|error| {
        format!(
            "failed to resolve workspace destination parent {}: {error}",
            destination_parent.display()
        )
    })?;
    let destination_name = destination.file_name().ok_or_else(|| {
        format!(
            "workspace destination has no file name: {}",
            destination.display()
        )
    })?;
    let destination = destination_parent.join(destination_name);
    if destination.starts_with(&source) || source.starts_with(&destination) {
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
    if let Err(error) = copy_tree_entries(&source, &destination) {
        return match fs::remove_dir_all(&destination) {
            Ok(()) => Err(error),
            Err(cleanup_error) => Err(format!(
                "{error}; failed to clean partial workspace {}: {cleanup_error}",
                destination.display()
            )),
        };
    }
    Ok(())
}

fn copy_tree_entries(source: &Path, destination: &Path) -> Result<(), String> {
    for entry in fs::read_dir(source)
        .map_err(|error| format!("failed to read workspace {}: {error}", source.display()))?
    {
        let entry = entry.map_err(|error| error.to_string())?;
        if entry.file_name() == ".git" {
            continue;
        }
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        let metadata = fs::symlink_metadata(&source_path).map_err(|error| {
            format!(
                "failed to inspect workspace path {}: {error}",
                source_path.display()
            )
        })?;
        if is_reparse_point(&metadata) {
            return Err(format!(
                "evaluation workspace contains a symlink or reparse point: {}",
                source_path.display()
            ));
        }
        if metadata.is_dir() {
            fs::create_dir(&destination_path).map_err(|error| {
                format!(
                    "failed to create workspace directory {}: {error}",
                    destination_path.display()
                )
            })?;
            copy_tree_entries(&source_path, &destination_path)?;
        } else if metadata.is_file() {
            fs::copy(&source_path, &destination_path).map_err(|error| {
                format!(
                    "failed to copy workspace file {}: {error}",
                    source_path.display()
                )
            })?;
        } else {
            return Err(format!(
                "evaluation workspace contains a non-regular entry: {}",
                source_path.display()
            ));
        }
    }
    Ok(())
}

pub(super) fn validate_tree(root: &Path) -> Result<(), String> {
    snapshot_workspace(root).map(|_| ())
}

pub(super) fn snapshot_workspace(root: &Path) -> Result<WorkspaceSnapshot, String> {
    let metadata = fs::symlink_metadata(root)
        .map_err(|error| format!("failed to inspect workspace {}: {error}", root.display()))?;
    if is_reparse_point(&metadata) || !metadata.is_dir() {
        return Err(format!(
            "workspace is not a regular directory: {}",
            root.display()
        ));
    }
    let mut snapshot = BTreeMap::new();
    snapshot_entries(root, root, &mut snapshot)?;
    Ok(snapshot)
}

pub(super) fn workspace_tree_digest(root: &Path) -> Result<String, String> {
    let snapshot = snapshot_workspace(root)?;
    let canonical = serde_json::to_vec(&snapshot)
        .map_err(|error| format!("failed to serialize workspace snapshot: {error}"))?;
    Ok(format!("sha256:{:x}", Sha256::digest(canonical)))
}

fn snapshot_entries(
    root: &Path,
    directory: &Path,
    snapshot: &mut WorkspaceSnapshot,
) -> Result<(), String> {
    for entry in fs::read_dir(directory)
        .map_err(|error| format!("failed to read workspace {}: {error}", directory.display()))?
    {
        let entry = entry.map_err(|error| error.to_string())?;
        if entry.file_name() == ".git" {
            continue;
        }
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path).map_err(|error| {
            format!(
                "failed to inspect workspace path {}: {error}",
                path.display()
            )
        })?;
        if is_reparse_point(&metadata) {
            return Err(format!(
                "workspace contains a symlink or reparse point: {}",
                path.display()
            ));
        }
        if metadata.is_dir() {
            snapshot_entries(root, &path, snapshot)?;
        } else if metadata.is_file() {
            let relative = path
                .strip_prefix(root)
                .map_err(|error| error.to_string())?
                .to_string_lossy()
                .replace('\\', "/");
            snapshot.insert(relative, file_sha256(&path)?);
        } else {
            return Err(format!(
                "workspace contains a non-regular entry: {}",
                path.display()
            ));
        }
    }
    Ok(())
}

fn file_sha256(path: &Path) -> Result<String, String> {
    let mut file = File::open(path)
        .map_err(|error| format!("failed to open workspace file {}: {error}", path.display()))?;
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).map_err(|error| {
            format!("failed to read workspace file {}: {error}", path.display())
        })?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(format!("sha256:{:x}", digest.finalize()))
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

pub(super) fn workspace_change_evidence(
    before: &WorkspaceSnapshot,
    after: &WorkspaceSnapshot,
    allowed_paths: &[singularity_evaluation::RelativePath],
) -> Vec<WorkspaceChangeEvidence> {
    changed_paths(before, after)
        .into_iter()
        .map(|path| WorkspaceChangeEvidence {
            change_kind: match (before.contains_key(&path), after.contains_key(&path)) {
                (false, true) => "added",
                (true, false) => "deleted",
                (true, true) => "modified",
                (false, false) => unreachable!("changed path must exist in one snapshot"),
            },
            before_sha256: before.get(&path).cloned(),
            after_sha256: after.get(&path).cloned(),
            allowed: path_is_allowed(&path, allowed_paths),
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

pub(super) fn path_is_allowed(
    path: &str,
    allowed_paths: &[singularity_evaluation::RelativePath],
) -> bool {
    allowed_paths.iter().any(|allowed| {
        let allowed = allowed.as_str();
        path == allowed
            || path
                .strip_prefix(allowed)
                .is_some_and(|suffix| suffix.starts_with('/'))
    })
}

fn is_reparse_point(metadata: &fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & REPARSE_POINT_ATTRIBUTE != 0
    }
    #[cfg(not(windows))]
    {
        let _ = REPARSE_POINT_ATTRIBUTE;
        false
    }
}

pub(super) fn canonical_or_original(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}
