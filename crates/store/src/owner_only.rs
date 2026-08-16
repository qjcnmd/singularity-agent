//! Owner-only permission enforcement for session directories and index/backup files.
//!
//! Unix uses mode 0700/0600; Windows follows the Pi strategy of no additional
//! ACL management (access is governed by the directory ACL).

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
        // Pi 策略：Windows 不做 owner-only ACL 管理，访问由目录 ACL 决定。
        let _ = path;
        Ok(())
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
        // Pi 策略：Windows 不做 owner-only ACL 管理，访问由目录 ACL 决定。
        let _ = path;
        Ok(())
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        Ok(())
    }
}
