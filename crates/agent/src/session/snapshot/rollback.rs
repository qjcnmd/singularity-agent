use std::fs;
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};

use super::{Repo, is_dir_reparse_point, with_snapshot_lock};

#[derive(Debug)]
pub struct RestoreBackup {
    snapshot_dir: PathBuf,
    cwd: PathBuf,
    entries: Vec<BackupEntry>,
}

#[derive(Debug)]
pub(super) enum BackupEntry {
    Remove(String),
    RestoreDir(String),
    RestoreBlob {
        path: String,
        oid: String,
        mode: String,
        readonly: bool,
        is_dir_symlink: bool,
    },
}

impl RestoreBackup {
    pub(super) fn new(snapshot_dir: PathBuf, cwd: PathBuf, entries: Vec<BackupEntry>) -> Self {
        Self { snapshot_dir, cwd, entries }
    }

    pub fn rollback(self) -> io::Result<()> {
        let dir = self.snapshot_dir.clone();
        let cwd = self.cwd.clone();
        with_snapshot_lock(&dir, || self.rollback_locked(Repo { dir: &dir, cwd: &cwd }))
    }

    pub(super) fn rollback_locked(self, repo: Repo<'_>) -> io::Result<()> {
        let mut first_err: Option<io::Error> = None;
        let mut record = |res: io::Result<()>| {
            if let Err(err) = res {
                first_err.get_or_insert(err);
            }
        };
        for entry in &self.entries {
            if let BackupEntry::Remove(rel) = entry {
                record(remove_backed_up_file(repo.cwd, rel));
            }
        }
        for entry in &self.entries {
            if let BackupEntry::RestoreDir(rel) = entry {
                record(restore_backed_up_dir(repo.cwd, rel));
            }
        }
        for entry in &self.entries {
            if let BackupEntry::RestoreBlob {
                path,
                oid,
                mode,
                readonly,
                is_dir_symlink,
            } = entry
                && mode != "120000"
            {
                record(restore_blob(repo, path, oid, mode, *readonly, *is_dir_symlink));
            }
        }
        for entry in &self.entries {
            if let BackupEntry::RestoreBlob {
                path,
                oid,
                mode,
                readonly,
                is_dir_symlink,
            } = entry
                && mode == "120000"
            {
                record(restore_blob(repo, path, oid, mode, *readonly, *is_dir_symlink));
            }
        }
        first_err.map_or(Ok(()), Err)
    }
}

pub(super) fn ensure_no_link_ancestor(cwd: &Path, rel: &str) -> io::Result<()> {
    let mut cur = cwd.to_path_buf();
    let parts: Vec<&str> = rel.split('/').collect();
    for part in parts.iter().take(parts.len().saturating_sub(1)) {
        cur.push(part);
        match fs::symlink_metadata(&cur) {
            Ok(meta) if meta.file_type().is_symlink() || is_dir_reparse_point(&meta) => {
                return Err(io::Error::new(
                    ErrorKind::InvalidInput,
                    format!("拒绝穿越符号链接或目录联接操作路径：{rel}"),
                ));
            }
            Ok(_) => {}
            Err(err) if err.kind() == ErrorKind::NotFound => return Ok(()),
            Err(err) => return Err(err),
        }
    }
    Ok(())
}

pub(super) fn join_rel(cwd: &Path, rel: &str) -> PathBuf {
    rel.split('/').fold(cwd.to_path_buf(), |mut acc, p| {
        acc.push(p);
        acc
    })
}

pub(super) fn prune_empty_parents(cwd: &Path, path: &Path) {
    for dir in path.ancestors().skip(1) {
        if dir == cwd || !dir.starts_with(cwd) {
            break;
        }
        let is_real_dir = fs::symlink_metadata(dir).is_ok_and(|m| m.is_dir() && !is_dir_reparse_point(&m));
        if !is_real_dir || fs::remove_dir(dir).is_err() {
            break;
        }
    }
}

fn remove_backed_up_file(cwd: &Path, rel: &str) -> io::Result<()> {
    ensure_no_link_ancestor(cwd, rel)?;
    let dst = join_rel(cwd, rel);
    match remove_file_clearing_readonly(&dst) {
        Ok(()) => {
            prune_empty_parents(cwd, &dst);
            Ok(())
        }
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

fn restore_backed_up_dir(cwd: &Path, rel: &str) -> io::Result<()> {
    ensure_no_link_ancestor(cwd, rel)?;
    fs::create_dir_all(join_rel(cwd, rel))
}

#[expect(
    clippy::permissions_set_readonly_false,
    reason = "Windows files carrying FILE_ATTRIBUTE_READONLY require clearing readonly before overwriting"
)]
fn restore_blob(
    repo: Repo<'_>,
    rel: &str,
    oid: &str,
    mode: &str,
    readonly: bool,
    is_dir_symlink: bool,
) -> io::Result<()> {
    ensure_no_link_ancestor(repo.cwd, rel)?;
    let dst = join_rel(repo.cwd, rel);
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)?;
    }
    if mode == "120000" {
        if let Err(err) = remove_file_clearing_readonly(&dst)
            && err.kind() != ErrorKind::NotFound
        {
            return Err(err);
        }
        #[cfg(windows)]
        {
            let bytes = repo.git(false, &["cat-file", "blob", oid])?;
            let target = parse_windows_symlink_target(&bytes)?;
            if is_dir_symlink {
                std::os::windows::fs::symlink_dir(&target, &dst)?;
            } else {
                std::os::windows::fs::symlink_file(&target, &dst)?;
            }
            return Ok(());
        }
        #[cfg(not(windows))]
        {
            let _ = is_dir_symlink;
            let cacheinfo = format!("{mode},{oid},{rel}");
            repo.git(true, &["update-index", "--add", "--replace", "--cacheinfo", &cacheinfo])?;
            repo.git(true, &["checkout-index", "-f", "--", rel])?;
            return Ok(());
        }
    }
    let bytes = repo.git(false, &["cat-file", "blob", oid])?;
    if let Ok(meta) = fs::symlink_metadata(&dst) {
        if meta.file_type().is_symlink() {
            remove_file_clearing_readonly(&dst)?;
        } else if meta.permissions().readonly() {
            let mut perms = meta.permissions();
            perms.set_readonly(false);
            fs::set_permissions(&dst, perms)?;
        }
    }
    fs::write(&dst, &bytes)?;
    if readonly {
        let meta = fs::symlink_metadata(&dst)?;
        let mut perms = meta.permissions();
        perms.set_readonly(true);
        fs::set_permissions(&dst, perms)?;
    }
    Ok(())
}

#[expect(
    clippy::permissions_set_readonly_false,
    reason = "Windows files carrying FILE_ATTRIBUTE_READONLY require clearing readonly before removal"
)]
pub(super) fn remove_file_clearing_readonly(path: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(path)?;
    if meta.file_type().is_symlink() {
        return if is_dir_reparse_point(&meta) {
            fs::remove_dir(path)
        } else {
            fs::remove_file(path)
        };
    }
    match fs::remove_file(path) {
        Err(err) if err.kind() == ErrorKind::PermissionDenied && meta.permissions().readonly() => {
            let mut perms = meta.permissions();
            perms.set_readonly(false);
            let _ = fs::set_permissions(path, perms);
            fs::remove_file(path)
        }
        other => other,
    }
}

pub(super) fn remove_dir_clearing_readonly(path: &Path) -> io::Result<()> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(()),
        // Git for Windows marks bare object files read-only, which makes remove_dir_all fail with PermissionDenied.
        Err(err) if err.kind() == ErrorKind::PermissionDenied => {
            clear_readonly_recursive(path);
            match fs::remove_dir_all(path) {
                Ok(()) => Ok(()),
                Err(retry_err) if retry_err.kind() == ErrorKind::NotFound => Ok(()),
                Err(retry_err) => Err(retry_err),
            }
        }
        Err(err) => Err(err),
    }
}

#[expect(
    clippy::permissions_set_readonly_false,
    reason = "Windows shadow-git loose object files carry FILE_ATTRIBUTE_READONLY and must be cleared before removal"
)]
fn clear_readonly_recursive(path: &Path) {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return;
    };
    if meta.file_type().is_symlink() || is_dir_reparse_point(&meta) {
        return;
    }
    if meta.permissions().readonly() {
        let mut perms = meta.permissions();
        perms.set_readonly(false);
        let _ = fs::set_permissions(path, perms);
    }
    if meta.is_dir()
        && let Ok(entries) = fs::read_dir(path)
    {
        for entry in entries.flatten() {
            clear_readonly_recursive(&entry.path());
        }
    }
}

#[cfg(any(windows, test))]
fn parse_windows_symlink_target(bytes: &[u8]) -> io::Result<String> {
    let raw = std::str::from_utf8(bytes).map_err(|e| io::Error::new(ErrorKind::InvalidData, e))?;
    if raw.is_empty() || raw.contains('\0') {
        return Err(io::Error::new(ErrorKind::InvalidData, "invalid symlink target blob"));
    }
    Ok(raw.replace('/', "\\"))
}

#[cfg(test)]
mod tests {
    use super::parse_windows_symlink_target;

    #[test]
    fn parse_windows_symlink_target_normalizes_separators_and_rejects_invalid() {
        assert_eq!(parse_windows_symlink_target(b"sub/target").expect("valid"), r"sub\target");
        assert!(parse_windows_symlink_target(b"").is_err());
        assert!(parse_windows_symlink_target(b"bad\0target").is_err());
        assert!(parse_windows_symlink_target(&[0xff]).is_err());
    }
}
