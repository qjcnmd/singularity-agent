use std::borrow::Cow;
use std::path::{Path, PathBuf};

use crate::display_path;

#[derive(Debug, Clone)]
pub struct CanonicalWorkspacePath {
    native: PathBuf,
    display: String,
}

impl PartialEq for CanonicalWorkspacePath {
    fn eq(&self, other: &Self) -> bool {
        same_saved_path(&self.display, &other.display)
    }
}

impl Eq for CanonicalWorkspacePath {}

impl CanonicalWorkspacePath {
    pub fn from_saved(path: impl AsRef<Path>) -> Result<Self, String> {
        let path = path.as_ref();
        if !path.is_absolute() {
            return Err("saved workspace path must be absolute".to_string());
        }
        Ok(Self::from_native(path.components().collect()))
    }

    fn from_native(native: PathBuf) -> Self {
        let display = display_path(&native);
        Self { native, display }
    }

    pub fn as_path(&self) -> &Path {
        &self.native
    }

    pub fn display(&self) -> &str {
        &self.display
    }
}

pub fn saved_path_key(path: &str) -> String {
    path.to_lowercase()
}

pub fn workspace_key(path: &Path) -> String {
    let normalized: PathBuf = path.components().collect();
    display_path(&normalized).to_lowercase()
}

pub fn same_saved_path(left: &str, right: &str) -> bool {
    saved_path_key(left) == saved_path_key(right)
}

pub fn workspace_key_contains(ancestor_key: &str, candidate_key: &str) -> bool {
    let ancestor_norm = normalize_workspace_key(ancestor_key);
    let candidate_norm = normalize_workspace_key(candidate_key);
    let ancestor = trim_trailing_slashes(&ancestor_norm);
    let candidate = trim_trailing_slashes(&candidate_norm);
    if ancestor.is_empty() || candidate.is_empty() {
        return false;
    }
    if ancestor == candidate {
        return true;
    }
    let Some(rest) = candidate.strip_prefix(ancestor) else {
        return false;
    };
    ancestor.ends_with('/') || rest.starts_with('/')
}

pub fn workspace_keys_overlap(left_key: &str, right_key: &str) -> bool {
    workspace_key_contains(left_key, right_key) || workspace_key_contains(right_key, left_key)
}

fn normalize_workspace_key(key: &str) -> Cow<'_, str> {
    if key.bytes().any(|b| b == b'\\' || b.is_ascii_uppercase()) || !key.is_ascii() {
        Cow::Owned(key.replace('\\', "/").to_lowercase())
    } else {
        Cow::Borrowed(key)
    }
}

fn trim_trailing_slashes(key: &str) -> &str {
    let trimmed = key.trim_end_matches('/');
    if trimmed.is_empty() {
        if key.is_empty() { "" } else { "/" }
    } else if trimmed.ends_with(':') && key.len() > trimmed.len() {
        &key[..trimmed.len() + 1]
    } else {
        trimmed
    }
}

pub fn canonicalize_workspace(path: impl AsRef<Path>) -> Result<CanonicalWorkspacePath, String> {
    let requested = path.as_ref();
    let native = std::fs::canonicalize(requested)
        .map_err(|error| format!("workspace directory is unavailable ({}): {error}", requested.display()))?;
    let metadata = std::fs::metadata(&native).map_err(|error| {
        format!("workspace directory cannot be inspected ({}): {error}", requested.display())
    })?;
    if !metadata.is_dir() {
        return Err(format!("workspace path is not a directory: {}", requested.display()));
    }
    Ok(CanonicalWorkspacePath::from_native(native))
}

pub(crate) fn project_root(cwd: &Path) -> Result<PathBuf, String> {
    for ancestor in cwd.ancestors() {
        let marker = ancestor.join(crate::PROJECT_ROOT_MARKER);
        match std::fs::symlink_metadata(&marker) {
            Ok(_) => return Ok(ancestor.to_path_buf()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!("cannot inspect project root marker ({}): {error}", marker.display()));
            }
        }
    }
    Ok(cwd.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::{workspace_key_contains, workspace_keys_overlap};

    #[test]
    fn workspace_key_containment_handles_roots_case_separators_and_prefix_traps() {
        assert!(workspace_key_contains("c:/repo", "c:/repo"));
        assert!(workspace_key_contains("C:/Repo", "c:/repo/sub/dir"));
        assert!(workspace_key_contains(r"C:\Repo\", "c:/repo"));
        assert!(workspace_key_contains(r"c:\", r"C:\Users\dev"));
        assert!(workspace_key_contains("c:/", "c:/project"));
        assert!(workspace_key_contains("/", "/var/tmp"));
        assert!(workspace_key_contains("//server/share/", "//server/share/sub"));

        assert!(!workspace_key_contains("c:/foo", "c:/foobar"));
        assert!(!workspace_key_contains("/foo", "/foobar"));
        assert!(!workspace_key_contains("//server/share", "//server/share2"));
        assert!(!workspace_key_contains("c:/", "d:/project"));
        assert!(!workspace_key_contains("", "c:/repo"));
        assert!(!workspace_key_contains("c:/repo", ""));
    }

    #[test]
    fn workspace_keys_overlap_is_symmetric_and_rejects_siblings() {
        assert!(workspace_keys_overlap("c:/repo", "c:/repo/child"));
        assert!(workspace_keys_overlap("c:/repo/child", "C:\\Repo"));
        assert!(workspace_keys_overlap("c:/", "c:/a/b"));
        assert!(!workspace_keys_overlap("c:/repo", "c:/repo-other"));
        assert!(!workspace_keys_overlap("c:/a", "c:/b"));
    }
}
