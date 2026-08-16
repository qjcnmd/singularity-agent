//! Owner-only permission enforcement for session directories and index/backup files.
//!
//! Unix uses mode 0700/0600; Windows delegates to the shared repository ACL
//! primitive in `singularity_core`.

use super::*;

pub fn ensure_owner_only_dir(path: &Path) -> StoreResult<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let metadata = std::fs::symlink_metadata(path).map_err(|error| {
            StoreError::InvalidState(format!(
                "cannot inspect owner-only dir {}: {error}",
                path.display()
            ))
        })?;
        if !metadata.is_dir() {
            return Err(StoreError::InvalidState(format!(
                "owner-only path is not a directory: {}",
                path.display()
            )));
        }
        if metadata.permissions().mode() & 0o077 != 0 {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).map_err(
                |error| {
                    StoreError::InvalidState(format!(
                        "cannot restrict owner-only dir {}: {error}",
                        path.display()
                    ))
                },
            )?;
        }
        Ok(())
    }
    #[cfg(windows)]
    {
        singularity_core::ensure_owner_only_dir(path).map_err(|error| {
            StoreError::InvalidState(format!(
                "cannot enforce owner-only dir {}: {error}",
                path.display()
            ))
        })
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        Ok(())
    }
}

pub fn ensure_owner_only_file(path: &Path) -> StoreResult<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let metadata = std::fs::symlink_metadata(path).map_err(|error| {
            StoreError::InvalidState(format!(
                "cannot inspect owner-only file {}: {error}",
                path.display()
            ))
        })?;
        if !metadata.is_file() {
            return Err(StoreError::InvalidState(format!(
                "owner-only path is not a regular file: {}",
                path.display()
            )));
        }
        if metadata.permissions().mode() & 0o077 != 0 {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(
                |error| {
                    StoreError::InvalidState(format!(
                        "cannot restrict owner-only file {}: {error}",
                        path.display()
                    ))
                },
            )?;
        }
        Ok(())
    }
    #[cfg(windows)]
    {
        singularity_core::ensure_owner_only_file(path).map_err(|error| {
            StoreError::InvalidState(format!(
                "cannot enforce owner-only file {}: {error}",
                path.display()
            ))
        })
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        Ok(())
    }
}
