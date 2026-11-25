use std::collections::HashSet;
use std::fs;
use std::io::{self, ErrorKind};
use std::path::{Component, Path, PathBuf};

use super::rollback::{
    BackupEntry, RestoreBackup, ensure_no_link_ancestor, join_rel, prune_empty_parents,
    remove_file_clearing_readonly,
};
use super::{
    EXCLUDED_DIRS, InitPolicy, Repo, canonicalize_existing, is_link_metadata, is_protected_rel, path_key,
    split_nul, validate_relative_path, validate_tree_hash,
};

struct DeleteEntry {
    path: String,
    oid: String,
}

struct WriteEntry {
    path: String,
    status: char,
    old_oid: String,
}

struct PlannedRemoval {
    path: String,
    undo: BackupEntry,
}

struct PlannedDirRemoval {
    path: String,
    empty_dir_undos: Vec<BackupEntry>,
}

struct PlannedWrite {
    path: String,
    undos: Vec<BackupEntry>,
}

struct RestorePlan {
    removals: Vec<PlannedRemoval>,
    dir_removals: Vec<PlannedDirRemoval>,
    writes: Vec<PlannedWrite>,
}

pub fn restore(snapshot_dir: &Path, cwd: &Path, target_tree: &str) -> io::Result<RestoreBackup> {
    let target = validate_tree_hash(target_tree)?;
    let repo = Repo { dir: snapshot_dir, cwd };
    repo.with_live_tree(InitPolicy::RequireExisting, |live, protected| {
        let raw_diff = repo.diff(&live, target, &["--raw", "--no-abbrev", "-z"], &[])?;
        let (to_delete, to_write) = parse_raw_diff(&raw_diff)?;
        let plan = prepare_restore(repo, &protected, to_delete, to_write)?;
        let mut applied = Vec::new();
        let result = apply_restore(repo, target, plan, &mut applied);
        let backup = RestoreBackup::new(repo.dir.to_path_buf(), repo.cwd.to_path_buf(), applied);
        if let Err(err) = result {
            if let Err(rollback_err) = backup.rollback_locked(repo) {
                tracing::warn!(
                    error = %rollback_err,
                    "failed to roll back workspace after snapshot restore failure"
                );
                return Err(io::Error::new(
                    err.kind(),
                    format!("{err}；自动回滚工作区也失败：{rollback_err}"),
                ));
            }
            return Err(err);
        }
        Ok(backup)
    })
}

fn prepare_restore(
    repo: Repo<'_>,
    protected: &[String],
    to_delete: Vec<DeleteEntry>,
    to_write: Vec<WriteEntry>,
) -> io::Result<RestorePlan> {
    let mut delete_keys = HashSet::with_capacity(to_delete.len());
    let mut removals = Vec::with_capacity(to_delete.len());
    for del in to_delete {
        if is_protected_rel(&del.path, protected) {
            continue;
        }
        let readonly = is_readonly_file(&join_rel(repo.cwd, &del.path));
        delete_keys.insert(path_key(&del.path));
        removals.push(PlannedRemoval {
            path: del.path.clone(),
            undo: BackupEntry::RestoreBlob { path: del.path, oid: del.oid, readonly },
        });
    }

    let mut dir_removals = Vec::new();
    let mut scheduled_dirs = HashSet::new();
    let mut writes = Vec::with_capacity(to_write.len());
    let cwd_canon = canonicalize_existing(repo.cwd);
    let cwd_key = app_core::workspace_key(&cwd_canon);
    for item in to_write {
        if is_protected_rel(&item.path, protected)
            || protected.iter().any(|p| is_protected_rel(p, std::slice::from_ref(&item.path)))
        {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                format!("还原目标与受保护路径冲突：{}", item.path),
            ));
        }
        let has_deleted_ancestor = check_ancestor_conflicts(
            repo.cwd,
            &cwd_canon,
            &cwd_key,
            &item.path,
            protected,
            &delete_keys,
            &mut scheduled_dirs,
            &mut dir_removals,
        )?;
        let dst = join_rel(repo.cwd, &item.path);
        let undos = match item.status {
            'M' | 'T' => {
                if !has_deleted_ancestor && let Ok(meta) = fs::symlink_metadata(&dst) {
                    if is_link_metadata(&meta) {
                        return Err(conflict(format!(
                            "还原目标与未纳入快照的符号链接或目录联接冲突：{}",
                            item.path
                        )));
                    }
                    if meta.is_dir() {
                        schedule_dir_removal(
                            &dst,
                            &item.path,
                            protected,
                            &delete_keys,
                            &mut scheduled_dirs,
                            &mut dir_removals,
                        )?;
                    }
                }
                let readonly = if has_deleted_ancestor {
                    false
                } else {
                    is_readonly_file(&dst)
                };
                vec![BackupEntry::RestoreBlob {
                    path: item.path.clone(),
                    oid: item.old_oid,
                    readonly,
                }]
            }
            'A' if has_deleted_ancestor => vec![BackupEntry::Remove(item.path.clone())],
            'A' => match fs::symlink_metadata(&dst) {
                Err(err) if err.kind() == ErrorKind::NotFound => {
                    remove_with_existing_parent(repo.cwd, &item.path)
                }
                Ok(_) if delete_keys.contains(&path_key(&item.path)) => {
                    remove_with_existing_parent(repo.cwd, &item.path)
                }
                Ok(meta) if is_link_metadata(&meta) => {
                    return Err(conflict(format!(
                        "还原目标与未纳入快照的符号链接或目录联接冲突：{}",
                        item.path
                    )));
                }
                Ok(meta) if meta.is_dir() => {
                    schedule_dir_removal(
                        &dst,
                        &item.path,
                        protected,
                        &delete_keys,
                        &mut scheduled_dirs,
                        &mut dir_removals,
                    )?;
                    vec![BackupEntry::Remove(item.path.clone())]
                }
                Ok(meta) => {
                    let oid = hash_blob(repo, &fs::read(&dst)?)?;
                    let readonly = meta.permissions().readonly();
                    let actual_rel = actual_relative_path(&cwd_canon, &cwd_key, &dst).ok_or_else(|| {
                        io::Error::new(
                            ErrorKind::InvalidInput,
                            format!("还原目标解析到工作区之外：{}", item.path),
                        )
                    })?;
                    let blob_backup = BackupEntry::RestoreBlob { path: actual_rel.clone(), oid, readonly };
                    if actual_rel != item.path {
                        removals.push(PlannedRemoval { path: actual_rel, undo: blob_backup });
                        vec![BackupEntry::Remove(item.path.clone())]
                    } else {
                        vec![blob_backup]
                    }
                }
                Err(err) => return Err(err),
            },
            other => {
                return Err(io::Error::new(
                    ErrorKind::InvalidData,
                    format!("unexpected git diff status: {other}"),
                ));
            }
        };
        writes.push(PlannedWrite { path: item.path, undos });
    }

    Ok(RestorePlan { removals, dir_removals, writes })
}

fn apply_restore(
    repo: Repo<'_>,
    target: &str,
    plan: RestorePlan,
    applied: &mut Vec<BackupEntry>,
) -> io::Result<()> {
    for rem in plan.removals {
        ensure_no_link_ancestor(repo.cwd, &rem.path)?;
        let path = join_rel(repo.cwd, &rem.path);
        match fs::symlink_metadata(&path) {
            Ok(meta) if meta.is_file() => {
                remove_file_clearing_readonly(&path)?;
                prune_empty_parents(repo.cwd, &path);
                applied.push(rem.undo);
            }
            Ok(meta) if is_link_metadata(&meta) => {
                return Err(conflict(format!("还原目标路径类型发生变更，拒绝删除：{}", rem.path)));
            }
            Ok(_) => {}
            Err(err) if err.kind() == ErrorKind::NotFound => {}
            Err(err) => return Err(err),
        }
    }
    for dir_rem in plan.dir_removals {
        ensure_no_link_ancestor(repo.cwd, &dir_rem.path)?;
        let dir = join_rel(repo.cwd, &dir_rem.path);
        match fs::symlink_metadata(&dir) {
            Ok(meta) if meta.is_dir() && !is_link_metadata(&meta) => {
                applied.extend(dir_rem.empty_dir_undos);
                fs::remove_dir_all(&dir)?;
            }
            Ok(_) => return Err(conflict(format!("还原目标路径类型发生变更，拒绝删除：{}", dir_rem.path))),
            Err(err) if err.kind() == ErrorKind::NotFound => applied.extend(dir_rem.empty_dir_undos),
            Err(err) => return Err(err),
        }
    }
    repo.git(true, &["read-tree", "--reset", target])?;
    if plan.writes.is_empty() {
        return Ok(());
    }
    for write in &plan.writes {
        ensure_no_link_ancestor(repo.cwd, &write.path)?;
        if let Ok(meta) = fs::symlink_metadata(join_rel(repo.cwd, &write.path))
            && is_link_metadata(&meta)
        {
            return Err(conflict(format!("还原目标被符号链接或目录联接占用，拒绝覆盖：{}", write.path)));
        }
    }
    let mut stdin_bytes = Vec::new();
    for write in plan.writes {
        stdin_bytes.extend_from_slice(write.path.as_bytes());
        stdin_bytes.push(0);
        applied.extend(write.undos);
    }
    repo.git_with_stdin(true, true, &["checkout-index", "-f", "-z", "--stdin"], Some(&stdin_bytes))?;
    Ok(())
}

#[expect(
    clippy::too_many_arguments,
    reason = "ancestor validation threads workspace canonicalization and shared removal plan state"
)]
fn check_ancestor_conflicts(
    cwd: &Path,
    cwd_canon: &Path,
    cwd_key: &str,
    rel: &str,
    protected: &[String],
    delete_keys: &HashSet<String>,
    scheduled_dirs: &mut HashSet<String>,
    dir_removals: &mut Vec<PlannedDirRemoval>,
) -> io::Result<bool> {
    let mut cur = PathBuf::new();
    let parts: Vec<&str> = rel.split('/').collect();
    for part in parts.iter().take(parts.len().saturating_sub(1)) {
        cur.push(part);
        let anc_rel = cur.to_string_lossy().replace('\\', "/");
        if delete_keys.contains(&path_key(&anc_rel)) {
            return Ok(true);
        }
        let anc_path = cwd.join(&cur);
        match fs::symlink_metadata(&anc_path) {
            Ok(meta) if !meta.is_dir() || is_link_metadata(&meta) => {
                return Err(conflict(format!("还原路径的父级被未纳入快照的文件或链接占用：{anc_rel}")));
            }
            Ok(_) => {
                if cfg!(windows)
                    && let Some(actual_anc) = actual_relative_path(cwd_canon, cwd_key, &anc_path)
                    && actual_anc != anc_rel
                {
                    schedule_dir_removal(
                        &anc_path,
                        &actual_anc,
                        protected,
                        delete_keys,
                        scheduled_dirs,
                        dir_removals,
                    )?;
                    return Ok(true);
                }
            }
            Err(err) if err.kind() == ErrorKind::NotFound => {}
            Err(err) => return Err(err),
        }
    }
    Ok(false)
}

fn schedule_dir_removal(
    dir: &Path,
    rel: &str,
    protected: &[String],
    delete_keys: &HashSet<String>,
    scheduled_dirs: &mut HashSet<String>,
    dir_removals: &mut Vec<PlannedDirRemoval>,
) -> io::Result<()> {
    if !scheduled_dirs.insert(path_key(rel)) {
        return Ok(());
    }
    let mut empty_dirs = Vec::new();
    inspect_conflicting_dir(dir, rel, protected, delete_keys, &mut empty_dirs)?;
    dir_removals.push(PlannedDirRemoval {
        path: rel.to_owned(),
        empty_dir_undos: empty_dirs.into_iter().map(BackupEntry::RestoreDir).collect(),
    });
    Ok(())
}

fn remove_with_existing_parent(cwd: &Path, rel: &str) -> Vec<BackupEntry> {
    let mut undos = vec![BackupEntry::Remove(rel.to_owned())];
    if let Some(existing_parent) = deepest_existing_dir(cwd, rel) {
        undos.push(BackupEntry::RestoreDir(existing_parent));
    }
    undos
}

fn deepest_existing_dir(cwd: &Path, rel: &str) -> Option<String> {
    let parts: Vec<&str> = rel.split('/').collect();
    if parts.len() <= 1 {
        return None;
    }
    let mut cur = cwd.to_path_buf();
    let mut deepest = None;
    let mut joined = String::new();
    for part in &parts[..parts.len() - 1] {
        cur.push(part);
        if !fs::symlink_metadata(&cur).is_ok_and(|m| m.is_dir() && !is_link_metadata(&m)) {
            break;
        }
        if !joined.is_empty() {
            joined.push('/');
        }
        joined.push_str(part);
        deepest = Some(joined.clone());
    }
    deepest
}

fn inspect_conflicting_dir(
    dir: &Path,
    base_rel: &str,
    protected: &[String],
    delete_keys: &HashSet<String>,
    empty_dirs: &mut Vec<String>,
) -> io::Result<()> {
    let mut has_children = false;
    for entry in fs::read_dir(dir)? {
        has_children = true;
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let child_rel = format!("{base_rel}/{name}");
        if is_protected_rel(&child_rel, protected)
            || EXCLUDED_DIRS.iter().any(|ex| name.eq_ignore_ascii_case(ex))
        {
            return Err(conflict(format!("还原目标与受保护或未纳入快照的目录冲突：{child_rel}")));
        }
        let meta = fs::symlink_metadata(entry.path())?;
        if is_link_metadata(&meta) {
            return Err(conflict(format!("还原目标与未纳入快照的符号链接或目录联接冲突：{child_rel}")));
        }
        if meta.is_dir() {
            inspect_conflicting_dir(&entry.path(), &child_rel, protected, delete_keys, empty_dirs)?;
        } else if !delete_keys.contains(&path_key(&child_rel)) {
            return Err(conflict(format!("还原目标与未纳入快照的文件冲突：{child_rel}")));
        }
    }
    if !has_children {
        empty_dirs.push(base_rel.to_owned());
    }
    Ok(())
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
        if old_mode == "120000" || new_mode == "120000" {
            return Err(io::Error::new(ErrorKind::InvalidInput, format!("不支持还原符号链接：{rel}")));
        }
        match status {
            'D' => {
                validate_tree_hash(old_oid)?;
                to_delete.push(DeleteEntry { path: rel, oid: old_oid.to_owned() });
            }
            'A' | 'M' | 'T' => {
                if status != 'A' {
                    validate_tree_hash(old_oid)?;
                }
                to_write.push(WriteEntry {
                    path: rel,
                    status,
                    old_oid: old_oid.to_owned(),
                });
            }
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

fn conflict(message: String) -> io::Error {
    io::Error::new(ErrorKind::AlreadyExists, message)
}

fn actual_relative_path(cwd_canon: &Path, cwd_key: &str, dst: &Path) -> Option<String> {
    let dst_canon = canonicalize_existing(dst);
    let dst_key = app_core::workspace_key(&dst_canon);
    if !app_core::workspace_key_contains(cwd_key, &dst_key) {
        return None;
    }
    let rel = dst_canon
        .components()
        .skip(cwd_canon.components().count())
        .filter_map(|c| match c {
            Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/");
    (!rel.is_empty()).then_some(rel)
}

fn is_readonly_file(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| !is_link_metadata(&m) && m.permissions().readonly())
}
