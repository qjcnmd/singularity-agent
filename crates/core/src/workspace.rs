use std::path::{Path, PathBuf};

use crate::display_path;

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

pub fn canonicalize_workspace(path: impl AsRef<Path>) -> Result<PathBuf, String> {
    let requested = path.as_ref();
    let native = std::fs::canonicalize(requested)
        .map_err(|error| format!("workspace directory is unavailable ({}): {error}", requested.display()))?;
    let metadata = std::fs::metadata(&native).map_err(|error| {
        format!("workspace directory cannot be inspected ({}): {error}", requested.display())
    })?;
    if !metadata.is_dir() {
        return Err(format!("workspace path is not a directory: {}", requested.display()));
    }
    Ok(native)
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
