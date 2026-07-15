use singularity_core::is_protected_path;
use singularity_windows_sandbox::AbsolutePathBuf;
use std::fmt;
use std::fs::{self, Metadata};
use std::os::windows::fs::MetadataExt;
use std::path::Path;

const MAX_PROTECTED_PATH_SCAN_DEPTH: usize = 64;
const MAX_PROTECTED_PATH_SCAN_ENTRIES: usize = 100_000;
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;

/// protected path 发现失败的稳定、脱敏错误分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProtectedPathDiscoveryError {
    InvalidWorkspace,
    WorkspaceUnavailable,
    EnumerationFailed,
    MetadataFailed,
    ReparsePointEncountered,
    OutsideWorkspace,
    NormalizationFailed,
    ScanDepthExceeded,
    ScanEntryLimitExceeded,
}

impl fmt::Display for ProtectedPathDiscoveryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let code = match self {
            Self::InvalidWorkspace => "invalid_workspace",
            Self::WorkspaceUnavailable => "workspace_unavailable",
            Self::EnumerationFailed => "enumeration_failed",
            Self::MetadataFailed => "metadata_failed",
            Self::ReparsePointEncountered => "reparse_point_encountered",
            Self::OutsideWorkspace => "outside_workspace",
            Self::NormalizationFailed => "normalization_failed",
            Self::ScanDepthExceeded => "scan_depth_exceeded",
            Self::ScanEntryLimitExceeded => "scan_entry_limit_exceeded",
        };
        write!(formatter, "protected_path_discovery_{code}")
    }
}

/// 从 canonical workspace 有界、确定性地发现现有 protected path；不跟随 symlink/reparse，遇到未知边界即拒绝执行。
pub(crate) fn discover_existing_protected_paths(
    workspace: &Path,
) -> Result<Vec<AbsolutePathBuf>, ProtectedPathDiscoveryError> {
    if !workspace.is_absolute() {
        return Err(ProtectedPathDiscoveryError::InvalidWorkspace);
    }
    if !workspace.is_dir() {
        return Err(ProtectedPathDiscoveryError::WorkspaceUnavailable);
    }

    let mut pending = vec![(workspace.to_path_buf(), 0usize)];
    let mut protected = Vec::new();
    let mut scanned_entries = 0usize;

    while let Some((directory, depth)) = pending.pop() {
        let mut entries = Vec::new();
        for entry in
            fs::read_dir(&directory).map_err(|_| ProtectedPathDiscoveryError::EnumerationFailed)?
        {
            scanned_entries = scanned_entries.saturating_add(1);
            if scanned_entries > MAX_PROTECTED_PATH_SCAN_ENTRIES {
                return Err(ProtectedPathDiscoveryError::ScanEntryLimitExceeded);
            }
            entries.push(entry.map_err(|_| ProtectedPathDiscoveryError::EnumerationFailed)?);
        }
        entries.sort_by_key(|entry| path_key(&entry.path()));

        for entry in entries.into_iter().rev() {
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path)
                .map_err(|_| ProtectedPathDiscoveryError::MetadataFailed)?;
            if metadata_is_reparse_point(&metadata) {
                return Err(ProtectedPathDiscoveryError::ReparsePointEncountered);
            }

            let relative = path
                .strip_prefix(workspace)
                .map_err(|_| ProtectedPathDiscoveryError::OutsideWorkspace)?;
            let normalized = AbsolutePathBuf::from_absolute_path_checked(&path)
                .map_err(|_| ProtectedPathDiscoveryError::NormalizationFailed)?;
            if is_protected_path(&relative.to_string_lossy()) {
                protected.push(normalized);
                continue;
            }

            if metadata.is_dir() {
                if depth >= MAX_PROTECTED_PATH_SCAN_DEPTH {
                    return Err(ProtectedPathDiscoveryError::ScanDepthExceeded);
                }
                pending.push((path, depth + 1));
            }
        }
    }

    protected.sort_by_key(|path| path_key(path.as_path()));
    protected.dedup_by(|left, right| path_key(left.as_path()) == path_key(right.as_path()));
    Ok(protected)
}

fn metadata_is_reparse_point(metadata: &Metadata) -> bool {
    metadata.file_type().is_symlink()
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

fn path_key(path: &Path) -> String {
    path.to_string_lossy()
        .replace('\\', "/")
        .to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::{ProtectedPathDiscoveryError, discover_existing_protected_paths};
    use std::collections::HashSet;
    use std::fs;
    use std::os::windows::fs::symlink_dir;
    use std::path::PathBuf;
    use tempfile::TempDir;

    #[test]
    fn discovers_nested_marker_prefix_and_suffix_paths_but_not_missing_paths() {
        let workspace = TempDir::new().expect("workspace");
        let nested = workspace.path().join("nested");
        fs::create_dir_all(&nested).expect("nested directory");
        fs::write(workspace.path().join(".env.local"), "opaque").expect("env marker");
        fs::write(nested.join("private-key.pem"), "opaque").expect("prefix marker");
        fs::write(nested.join("client.p12"), "opaque").expect("suffix marker");
        let missing = workspace.path().join("missing.pem");
        fs::write(workspace.path().join("missing.pem.disabled"), "ordinary").expect("ordinary");

        let actual = discover_existing_protected_paths(workspace.path()).expect("discover paths");
        let actual: HashSet<PathBuf> = actual.into_iter().map(|path| path.to_path_buf()).collect();
        let expected = [
            workspace.path().join(".env.local"),
            nested.join("private-key.pem"),
            nested.join("client.p12"),
        ]
        .into_iter()
        .map(|path| dunce::canonicalize(path).expect("canonical expected path"))
        .collect::<HashSet<_>>();

        assert_eq!(actual, expected);
        assert!(!actual.contains(&missing));
    }

    #[test]
    fn rejects_reparse_points_without_following_them() {
        let workspace = TempDir::new().expect("workspace");
        let outside = TempDir::new().expect("outside");
        fs::write(outside.path().join(".env.outside"), "opaque").expect("outside marker");
        let alias = workspace.path().join("linked");
        if symlink_dir(outside.path(), &alias).is_err() {
            return;
        }

        let error = discover_existing_protected_paths(workspace.path())
            .expect_err("reparse points must fail closed");

        assert_eq!(error, ProtectedPathDiscoveryError::ReparsePointEncountered);
        let outside_path = outside.path().to_string_lossy();
        assert!(!error.to_string().contains(outside_path.as_ref()));
    }

    #[test]
    fn rejects_non_directory_workspace_with_typed_error() {
        let temp = TempDir::new().expect("temp");
        let file = temp.path().join("workspace-file");
        fs::write(&file, "not a directory").expect("workspace file");

        assert_eq!(
            discover_existing_protected_paths(&file),
            Err(ProtectedPathDiscoveryError::WorkspaceUnavailable)
        );
    }
}
