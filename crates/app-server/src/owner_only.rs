//! 会话目录及索引备份文件的属主权限约束管理。
//!
//! 在 Unix 系统上通过文件模式（0700 目录）限制仅当前属主具备读写权限。

use std::path::Path;

#[cfg(unix)]
use super::session_index::SessionIndexError;
use super::session_index::SessionIndexResult;

pub fn ensure_owner_only_dir(path: &Path) -> SessionIndexResult<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let metadata = std::fs::symlink_metadata(path).map_err(|error| {
            SessionIndexError::InvalidState(format!(
                "cannot inspect owner-only dir {}: {error}",
                path.display()
            ))
        })?;
        if !metadata.is_dir() {
            return Err(SessionIndexError::InvalidState(format!(
                "owner-only path is not a directory: {}",
                path.display()
            )));
        }
        if metadata.permissions().mode() & 0o077 != 0 {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).map_err(
                |error| {
                    SessionIndexError::InvalidState(format!(
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
        let _ = path;
        Ok(())
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        Ok(())
    }
}
