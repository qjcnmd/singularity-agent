//! 会话目录与文件的属主权限约束。
//!
//! Unix 上通过文件模式（0700 目录 / 0600 文件）限制仅当前属主可读写；
//! Windows 上依赖目录继承 ACL，不做额外收紧。

use std::path::Path;

fn restrict(
    path: &Path,
    mode: u32,
    kind: &str,
    is_expected: impl Fn(&std::fs::Metadata) -> bool,
) -> Result<(), String> {
    let metadata = std::fs::symlink_metadata(path).map_err(|error| {
        format!(
            "cannot inspect owner-only {kind} {}: {error}",
            path.display()
        )
    })?;
    if !is_expected(&metadata) {
        return Err(format!(
            "owner-only path is not a {kind}: {}",
            path.display()
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).map_err(
                |error| {
                    format!(
                        "cannot restrict owner-only {kind} {}: {error}",
                        path.display()
                    )
                },
            )?;
        }
    }
    #[cfg(not(unix))]
    {
        let _ = mode;
    }
    Ok(())
}

pub fn ensure_owner_only_dir(path: &Path) -> Result<(), String> {
    restrict(path, 0o700, "directory", std::fs::Metadata::is_dir)
}

pub fn ensure_owner_only_file(path: &Path) -> Result<(), String> {
    restrict(path, 0o600, "file", std::fs::Metadata::is_file)
}

/// 创建目录并确保属主权限（Unix 0700）。
pub fn create_owner_only_dir(path: &Path) -> Result<(), String> {
    std::fs::create_dir_all(path)
        .map_err(|error| format!("failed to create {}: {error}", path.display()))?;
    ensure_owner_only_dir(path)
}
