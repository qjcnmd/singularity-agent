use std::collections::HashSet;
use std::fs;
use std::io::{self, ErrorKind};
use std::path::{Component, Path, PathBuf};

use super::{
    EXCLUDED_DIRS, Repo, canonicalize_existing, is_protected_rel, remove_file_clearing_readonly, split_nul,
    validate_relative_path, validate_tree_hash, with_snapshot_lock,
};

pub struct RestoreBackup {
    snapshot_dir: PathBuf,
    cwd: PathBuf,
    entries: Vec<BackupEntry>,
}

enum BackupEntry {
    Remove(String),
    RestoreDir(String),
    RestoreBlob {
        path: String,
        oid: String,
        mode: String,
        readonly: bool,
    },
}

struct DeleteEntry {
    path: String,
    mode: String,
    oid: String,
}

struct WriteEntry {
    path: String,
    status: char,
    old_mode: String,
    old_oid: String,
}

impl RestoreBackup {
    pub fn rollback(self) -> io::Result<()> {
        let dir = self.snapshot_dir.clone();
        let cwd = self.cwd.clone();
        with_snapshot_lock(&dir, || {
            self.rollback_locked(Repo { dir: &dir, cwd: &cwd });
            Ok(())
        })
    }

    fn rollback_locked(self, repo: Repo<'_>) {
        for entry in &self.entries {
            if let BackupEntry::Remove(rel) = entry {
                let dst = join_rel(repo.cwd, rel);
                let _ = remove_file_clearing_readonly(&dst);
                prune_empty_parents(repo.cwd, &dst);
            }
        }
        for entry in &self.entries {
            if let BackupEntry::RestoreDir(rel) = entry {
                let _ = fs::create_dir_all(join_rel(repo.cwd, rel));
            }
        }
        for entry in &self.entries {
            if let BackupEntry::RestoreBlob { path, oid, mode, readonly } = entry {
                restore_blob(repo, path, oid, mode, *readonly);
            }
        }
    }
}

pub fn restore(snapshot_dir: &Path, cwd: &Path, target_tree: &str) -> io::Result<RestoreBackup> {
    let target = validate_tree_hash(target_tree)?;
    let repo = Repo { dir: snapshot_dir, cwd };
    repo.with_live_tree(|live, protected| {
        let raw_diff = repo.diff(&live, target, &["--raw", "-z"], &[])?;
        let (to_delete, to_write) = parse_raw_diff(&raw_diff)?;
        let (backup, plan) = prepare_restore(repo, &protected, to_delete, to_write)?;
        if let Err(err) = apply_restore(repo, target, &plan) {
            backup.rollback_locked(repo);
            return Err(err);
        }
        Ok(backup)
    })
}

struct RestorePlan {
    remove_files: Vec<String>,
    remove_empty_dirs: Vec<String>,
    write_paths: Vec<String>,
}

fn prepare_restore(
    repo: Repo<'_>,
    protected: &[String],
    to_delete: Vec<DeleteEntry>,
    to_write: Vec<WriteEntry>,
) -> io::Result<(RestoreBackup, RestorePlan)> {
    let mut delete_keys = HashSet::with_capacity(to_delete.len());
    let mut entries = Vec::new();
    let mut remove_files = Vec::with_capacity(to_delete.len());
    for del in to_delete {
        if is_protected_rel(&del.path, protected) {
            continue;
        }
        let readonly = is_readonly_file(&join_rel(repo.cwd, &del.path));
        delete_keys.insert(path_key(&del.path));
        remove_files.push(del.path.clone());
        entries.push(BackupEntry::RestoreBlob {
            path: del.path,
            oid: del.oid,
            mode: del.mode,
            readonly,
        });
    }

    let mut remove_empty_dirs = Vec::new();
    let mut write_paths = Vec::with_capacity(to_write.len());
    let cwd_canon = canonicalize_existing(repo.cwd);
    for item in to_write {
        if is_protected_rel(&item.path, protected)
            || protected.iter().any(|p| is_protected_rel(p, std::slice::from_ref(&item.path)))
        {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                format!("还原目标与受保护路径冲突：{}", item.path),
            ));
        }
        check_ancestor_conflicts(repo.cwd, &item.path, &delete_keys)?;
        let dst = join_rel(repo.cwd, &item.path);
        match item.status {
            'M' | 'T' => {
                if let Ok(meta) = fs::symlink_metadata(&dst)
                    && meta.is_dir()
                {
                    let _ = inspect_conflicting_dir(&dst, &item.path, protected, &delete_keys)?;
                    remove_empty_dirs.push(item.path.clone());
                }
                let readonly = is_readonly_file(&dst);
                entries.push(BackupEntry::RestoreBlob {
                    path: item.path.clone(),
                    oid: item.old_oid,
                    mode: item.old_mode,
                    readonly,
                });
            }
            'A' => match fs::symlink_metadata(&dst) {
                Err(err) if err.kind() == ErrorKind::NotFound => {
                    entries.push(BackupEntry::Remove(item.path.clone()));
                }
                Ok(_) if delete_keys.contains(&path_key(&item.path)) => {
                    entries.push(BackupEntry::Remove(item.path.clone()));
                }
                Ok(meta) if meta.is_dir() => {
                    let has_files = inspect_conflicting_dir(&dst, &item.path, protected, &delete_keys)?;
                    remove_empty_dirs.push(item.path.clone());
                    entries.push(BackupEntry::Remove(item.path.clone()));
                    if !has_files {
                        entries.push(BackupEntry::RestoreDir(item.path.clone()));
                    }
                }
                Ok(meta) if meta.file_type().is_symlink() => {
                    return Err(io::Error::new(
                        ErrorKind::AlreadyExists,
                        format!("还原目标与未纳入快照的符号链接冲突：{}", item.path),
                    ));
                }
                Ok(meta) => {
                    let bytes = fs::read(&dst)?;
                    let oid = hash_blob(repo, &bytes)?;
                    let readonly = meta.permissions().readonly();
                    let actual_rel =
                        actual_relative_path(&cwd_canon, &dst).unwrap_or_else(|| item.path.clone());
                    if actual_rel != item.path {
                        remove_files.push(actual_rel.clone());
                        entries.push(BackupEntry::Remove(item.path.clone()));
                    }
                    entries.push(BackupEntry::RestoreBlob {
                        path: actual_rel,
                        oid,
                        mode: "100644".to_owned(),
                        readonly,
                    });
                }
                Err(err) => return Err(err),
            },
            other => {
                return Err(io::Error::new(
                    ErrorKind::InvalidData,
                    format!("unexpected git diff status: {other}"),
                ));
            }
        }
        write_paths.push(item.path);
    }

    Ok((
        RestoreBackup {
            snapshot_dir: repo.dir.to_path_buf(),
            cwd: repo.cwd.to_path_buf(),
            entries,
        },
        RestorePlan {
            remove_files,
            remove_empty_dirs,
            write_paths,
        },
    ))
}

fn apply_restore(repo: Repo<'_>, target: &str, plan: &RestorePlan) -> io::Result<()> {
    for rel in &plan.remove_files {
        let path = join_rel(repo.cwd, rel);
        match fs::symlink_metadata(&path) {
            Ok(meta) if meta.is_file() || meta.file_type().is_symlink() => {
                remove_file_clearing_readonly(&path)?;
                prune_empty_parents(repo.cwd, &path);
            }
            Ok(_) => {}
            Err(err) if err.kind() == ErrorKind::NotFound => {}
            Err(err) => return Err(err),
        }
    }
    for rel in &plan.remove_empty_dirs {
        let dir = join_rel(repo.cwd, rel);
        if dir.exists() {
            fs::remove_dir_all(&dir)?;
        }
    }
    repo.git(true, &["read-tree", "--reset", target])?;
    if plan.write_paths.is_empty() {
        return Ok(());
    }
    let mut stdin_bytes = Vec::new();
    for path in &plan.write_paths {
        stdin_bytes.extend_from_slice(path.as_bytes());
        stdin_bytes.push(0);
    }
    repo.git_with_stdin(true, true, &["checkout-index", "-f", "-z", "--stdin"], Some(&stdin_bytes))?;
    Ok(())
}

fn check_ancestor_conflicts(cwd: &Path, rel: &str, delete_keys: &HashSet<String>) -> io::Result<()> {
    let mut cur = PathBuf::new();
    let parts: Vec<&str> = rel.split('/').collect();
    for part in parts.iter().take(parts.len().saturating_sub(1)) {
        cur.push(part);
        let anc_rel = cur.to_string_lossy().replace('\\', "/");
        if delete_keys.contains(&path_key(&anc_rel)) {
            continue;
        }
        match fs::symlink_metadata(cwd.join(&cur)) {
            Ok(meta) if !meta.is_dir() => {
                return Err(io::Error::new(
                    ErrorKind::AlreadyExists,
                    format!("还原路径的父级被未纳入快照的文件占用：{anc_rel}"),
                ));
            }
            Ok(_) => {}
            Err(err) if err.kind() == ErrorKind::NotFound => {}
            Err(err) => return Err(err),
        }
    }
    Ok(())
}

fn inspect_conflicting_dir(
    dir: &Path,
    base_rel: &str,
    protected: &[String],
    delete_keys: &HashSet<String>,
) -> io::Result<bool> {
    let mut has_files = false;
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let child_rel = format!("{base_rel}/{name}");
        if is_protected_rel(&child_rel, protected)
            || EXCLUDED_DIRS.iter().any(|ex| name.eq_ignore_ascii_case(ex))
        {
            return Err(io::Error::new(
                ErrorKind::AlreadyExists,
                format!("还原目标与受保护或未纳入快照的目录冲突：{child_rel}"),
            ));
        }
        let meta = fs::symlink_metadata(entry.path())?;
        if meta.is_dir() {
            if inspect_conflicting_dir(&entry.path(), &child_rel, protected, delete_keys)? {
                has_files = true;
            }
        } else {
            if !delete_keys.contains(&path_key(&child_rel)) {
                return Err(io::Error::new(
                    ErrorKind::AlreadyExists,
                    format!("还原目标与未纳入快照的文件冲突：{child_rel}"),
                ));
            }
            has_files = true;
        }
    }
    Ok(has_files)
}

fn parse_raw_diff(raw: &[u8]) -> io::Result<(Vec<DeleteEntry>, Vec<WriteEntry>)> {
    let mut to_delete = Vec::new();
    let mut to_write = Vec::new();
    let mut fields = split_nul(raw);
    while let (Some(header_raw), Some(path_raw)) = (fields.next(), fields.next()) {
        let header =
            std::str::from_utf8(header_raw).map_err(|e| io::Error::new(ErrorKind::InvalidData, e))?;
        let path_str =
            std::str::from_utf8(path_raw).map_err(|e| io::Error::new(ErrorKind::InvalidData, e))?;
        let rel = validate_relative_path(path_str)?;
        let mut parts = header.split_ascii_whitespace();
        let old_mode = parts.next().and_then(|m| m.strip_prefix(':')).ok_or_else(|| {
            io::Error::new(ErrorKind::InvalidData, format!("invalid diff header: {header}"))
        })?;
        let new_mode = parts.next().unwrap_or_default();
        let old_oid = parts.next().unwrap_or_default();
        let _new_oid = parts.next().unwrap_or_default();
        let status = parts.next().and_then(|s| s.chars().next()).unwrap_or_default();
        if old_mode == "160000" || new_mode == "160000" {
            return Err(io::Error::new(ErrorKind::InvalidInput, format!("不支持还原嵌套子模块目录：{rel}")));
        }
        match status {
            'D' => to_delete.push(DeleteEntry {
                path: rel,
                mode: old_mode.to_owned(),
                oid: old_oid.to_owned(),
            }),
            'A' | 'M' | 'T' => to_write.push(WriteEntry {
                path: rel,
                status,
                old_mode: old_mode.to_owned(),
                old_oid: old_oid.to_owned(),
            }),
            other => {
                return Err(io::Error::new(
                    ErrorKind::InvalidData,
                    format!("unsupported diff status: {other}"),
                ));
            }
        }
    }
    Ok((to_delete, to_write))
}

fn hash_blob(repo: Repo<'_>, bytes: &[u8]) -> io::Result<String> {
    let out =
        repo.git_with_stdin(false, true, &["hash-object", "-w", "--no-filters", "--stdin"], Some(bytes))?;
    let oid = String::from_utf8(out).map_err(|e| io::Error::new(ErrorKind::InvalidData, e))?;
    Ok(validate_tree_hash(oid.trim())?.to_owned())
}

#[expect(
    clippy::permissions_set_readonly_false,
    reason = "Windows files carrying FILE_ATTRIBUTE_READONLY require clearing readonly before overwriting"
)]
fn restore_blob(repo: Repo<'_>, rel: &str, oid: &str, mode: &str, readonly: bool) {
    let dst = join_rel(repo.cwd, rel);
    if let Some(parent) = dst.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if mode == "120000" {
        let cacheinfo = format!("{mode},{oid},{rel}");
        let _ = remove_file_clearing_readonly(&dst);
        let _ = repo.git(true, &["update-index", "--add", "--cacheinfo", &cacheinfo]);
        let _ = repo.git(true, &["checkout-index", "-f", "--", rel]);
        return;
    }
    let Ok(bytes) = repo.git(false, &["cat-file", "blob", oid]) else {
        return;
    };
    if let Ok(meta) = fs::symlink_metadata(&dst) {
        if meta.file_type().is_symlink() {
            let _ = fs::remove_file(&dst);
        } else if meta.permissions().readonly() {
            let mut perms = meta.permissions();
            perms.set_readonly(false);
            let _ = fs::set_permissions(&dst, perms);
        }
    }
    if fs::write(&dst, &bytes).is_ok()
        && readonly
        && let Ok(meta) = fs::symlink_metadata(&dst)
    {
        let mut perms = meta.permissions();
        perms.set_readonly(true);
        let _ = fs::set_permissions(&dst, perms);
    }
}

fn join_rel(cwd: &Path, rel: &str) -> PathBuf {
    rel.split('/').fold(cwd.to_path_buf(), |mut acc, p| {
        acc.push(p);
        acc
    })
}

fn prune_empty_parents(cwd: &Path, path: &Path) {
    for dir in path.ancestors().skip(1) {
        if dir == cwd || !dir.starts_with(cwd) || fs::remove_dir(dir).is_err() {
            break;
        }
    }
}

fn actual_relative_path(cwd_canon: &Path, dst: &Path) -> Option<String> {
    let dst_canon = canonicalize_existing(dst);
    let cwd_count = cwd_canon.components().count();
    let rel = dst_canon
        .components()
        .skip(cwd_count)
        .filter_map(|c| match c {
            Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/");
    (!rel.is_empty()).then_some(rel)
}

fn is_readonly_file(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| !m.file_type().is_symlink() && m.permissions().readonly())
}

fn path_key(rel: &str) -> String {
    if cfg!(windows) {
        rel.to_lowercase()
    } else {
        rel.to_owned()
    }
}
