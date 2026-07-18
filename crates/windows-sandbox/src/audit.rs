use crate::acl::add_deny_write_ace;
use crate::acl::path_mask_allows;
use crate::cap::workspace_cap_sid_for_cwd;
use crate::cap::workspace_write_cap_sid_for_root;
use crate::cap::workspace_write_root_contains_path;
use crate::logging::log_note;
use crate::path_normalization::canonicalize_path;
use crate::path_safety::ensure_case_insensitive_acl_path;
use crate::resolved_permissions::ResolvedWindowsSandboxPermissions;
use crate::setup::effective_write_roots_for_permissions;
use crate::token::LocalSid;
use crate::token::world_sid;
use anyhow::Context;
use anyhow::Result;
use std::collections::HashSet;
use std::ffi::OsStr;
use std::ffi::c_void;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use std::time::Instant;
use windows_sys::Win32::Storage::FileSystem::FILE_APPEND_DATA;
use windows_sys::Win32::Storage::FileSystem::FILE_WRITE_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::FILE_WRITE_DATA;
use windows_sys::Win32::Storage::FileSystem::FILE_WRITE_EA;

// Preflight scan limits
const MAX_ITEMS_PER_DIR: i32 = 1000;
const AUDIT_TIME_LIMIT_SECS: i64 = 2;
const MAX_CHECKED_LIMIT: i32 = 50000;
// Case-insensitive suffixes (normalized to forward slashes) to skip during one-level child scan
const SKIP_DIR_SUFFIXES: &[&str] = &[
    "/windows/installer",
    "/windows/registration",
    "/programdata",
];

fn unique_push(set: &mut HashSet<PathBuf>, out: &mut Vec<PathBuf>, p: PathBuf) {
    if let Ok(abs) = p.canonicalize()
        && set.insert(abs.clone())
    {
        out.push(abs);
    }
}

fn gather_candidates(cwd: &Path, env: &std::collections::HashMap<String, String>) -> Vec<PathBuf> {
    let mut set: HashSet<PathBuf> = HashSet::new();
    let mut out: Vec<PathBuf> = Vec::new();
    // 1) CWD first (so immediate children get scanned early)
    unique_push(&mut set, &mut out, cwd.to_path_buf());
    // 2) TEMP/TMP next (often small, quick to scan)
    for k in ["TEMP", "TMP"] {
        if let Some(v) = env.get(k).cloned().or_else(|| std::env::var(k).ok()) {
            unique_push(&mut set, &mut out, PathBuf::from(v));
        }
    }
    // 3) User roots
    if let Some(up) = std::env::var_os("USERPROFILE") {
        unique_push(&mut set, &mut out, PathBuf::from(up));
    }
    if let Some(pubp) = std::env::var_os("PUBLIC") {
        unique_push(&mut set, &mut out, PathBuf::from(pubp));
    }
    // 4) PATH entries (best-effort)
    if let Some(path) = env
        .get("PATH")
        .cloned()
        .or_else(|| std::env::var("PATH").ok())
    {
        for part in std::env::split_paths(OsStr::new(&path)) {
            if !part.as_os_str().is_empty() {
                unique_push(&mut set, &mut out, part);
            }
        }
    }
    // 5) Core system roots last
    for p in [PathBuf::from("C:/"), PathBuf::from("C:/Windows")] {
        unique_push(&mut set, &mut out, p);
    }
    out
}

unsafe fn path_has_world_write_allow(path: &Path) -> Result<bool> {
    let mut world = unsafe { world_sid()? };
    let psid_world = world.as_mut_ptr() as *mut c_void;
    let write_mask = FILE_WRITE_DATA | FILE_APPEND_DATA | FILE_WRITE_EA | FILE_WRITE_ATTRIBUTES;
    path_mask_allows(
        path,
        &[psid_world],
        write_mask,
        /*require_all_bits*/ false,
    )
}

fn ensure_audit_budget(start: Instant, checked: usize) -> Result<()> {
    if start.elapsed() > Duration::from_secs(AUDIT_TIME_LIMIT_SECS as u64) {
        anyhow::bail!("world-writable audit exceeded its time limit");
    }
    if checked >= MAX_CHECKED_LIMIT as usize {
        anyhow::bail!("world-writable audit exceeded its item limit");
    }
    Ok(())
}

pub fn audit_everyone_writable(
    cwd: &Path,
    env: &std::collections::HashMap<String, String>,
    logs_base_dir: Option<&Path>,
) -> Result<Vec<PathBuf>> {
    let start = Instant::now();
    let mut flagged: Vec<PathBuf> = Vec::new();
    let mut seen: HashSet<PathBuf> = HashSet::new();
    let mut checked = 0usize;
    let check_world_writable = |path: &Path| -> Result<bool> {
        unsafe { path_has_world_write_allow(path) }.with_context(|| {
            format!(
                "world-writable audit could not read ACL for {}",
                path.display()
            )
        })
    };
    // Fast path: check CWD immediate children first so workspace issues are caught early.
    let read = std::fs::read_dir(cwd)
        .with_context(|| format!("world-writable audit could not enumerate {}", cwd.display()))?;
    for entry in read.take(MAX_ITEMS_PER_DIR as usize) {
        ensure_audit_budget(start, checked)?;
        let ent = entry.with_context(|| {
            format!(
                "world-writable audit could not read an entry under {}",
                cwd.display()
            )
        })?;
        let ft = ent.file_type().with_context(|| {
            format!(
                "world-writable audit could not inspect {}",
                ent.path().display()
            )
        })?;
        if ft.is_symlink() || !ft.is_dir() {
            continue;
        }
        let p = ent.path();
        checked += 1;
        let has = check_world_writable(&p)?;
        if has {
            let key = canonicalize_path(&p);
            if seen.insert(key) {
                flagged.push(p);
            }
        }
    }
    // Continue with broader candidate sweep
    let candidates = gather_candidates(cwd, env);
    for root in candidates {
        ensure_audit_budget(start, checked)?;
        checked += 1;
        let has_root = check_world_writable(&root)?;
        if has_root {
            let key = canonicalize_path(&root);
            if seen.insert(key) {
                flagged.push(root.clone());
            }
        }
        if root.is_dir() {
            let read = std::fs::read_dir(&root).with_context(|| {
                format!(
                    "world-writable audit could not enumerate {}",
                    root.display()
                )
            })?;
            for entry in read.take(MAX_ITEMS_PER_DIR as usize) {
                ensure_audit_budget(start, checked)?;
                let ent = entry.with_context(|| {
                    format!(
                        "world-writable audit could not read an entry under {}",
                        root.display()
                    )
                })?;
                let p = ent.path();
                // Skip reparse points (symlinks/junctions) to avoid auditing link ACLs
                let ft = ent.file_type().with_context(|| {
                    format!("world-writable audit could not inspect {}", p.display())
                })?;
                if ft.is_symlink() {
                    continue;
                }
                // Skip noisy/irrelevant Windows system subdirectories
                let pl = p.to_string_lossy().to_ascii_lowercase();
                let norm = pl.replace('\\', "/");
                if SKIP_DIR_SUFFIXES.iter().any(|s| norm.ends_with(s)) {
                    continue;
                }
                if ft.is_dir() {
                    checked += 1;
                    let has_child = check_world_writable(&p)?;
                    if has_child {
                        let key = canonicalize_path(&p);
                        if seen.insert(key) {
                            flagged.push(p);
                        }
                    }
                }
            }
        }
    }
    let elapsed_ms = start.elapsed().as_millis();
    if !flagged.is_empty() {
        let mut list = String::new();
        for p in &flagged {
            list.push_str(&format!("\n - {}", p.display()));
        }
        crate::logging::log_note(
            &format!(
                "AUDIT: world-writable scan FAILED; cwd={cwd:?}; checked={checked}; duration_ms={elapsed_ms}; flagged:{list}",
            ),
            logs_base_dir,
        );

        return Ok(flagged);
    }
    // Log success once if nothing flagged
    crate::logging::log_note(
        &format!("AUDIT: world-writable scan OK; checked={checked}; duration_ms={elapsed_ms}"),
        logs_base_dir,
    );
    Ok(Vec::new())
}

pub fn apply_world_writable_scan_and_denies_for_permissions(
    sandbox_home: &Path,
    cwd: &Path,
    env_map: &std::collections::HashMap<String, String>,
    permissions: &ResolvedWindowsSandboxPermissions,
    logs_base_dir: Option<&Path>,
) -> Result<()> {
    let flagged = audit_everyone_writable(cwd, env_map, logs_base_dir)?;
    if flagged.is_empty() {
        return Ok(());
    }
    apply_capability_denies_for_world_writable_for_permissions(
        sandbox_home,
        &flagged,
        permissions,
        cwd,
        env_map,
        logs_base_dir,
    )
    .context("world-writable audit could not apply capability deny ACEs")
}

fn apply_capability_denies_for_world_writable_for_permissions(
    sandbox_home: &Path,
    flagged: &[PathBuf],
    permissions: &ResolvedWindowsSandboxPermissions,
    cwd: &Path,
    env_map: &std::collections::HashMap<String, String>,
    logs_base_dir: Option<&Path>,
) -> Result<()> {
    if flagged.is_empty() {
        return Ok(());
    }
    if !permissions.is_enforceable_by_windows_sandbox() {
        return Ok(());
    }
    let uses_write_capabilities = permissions.uses_write_capabilities_for_cwd(cwd, env_map);
    let workspace_roots = if uses_write_capabilities {
        effective_write_roots_for_permissions(
            permissions,
            cwd,
            env_map,
            sandbox_home,
            /*write_roots_override*/ None,
        )
    } else {
        Vec::new()
    };
    let paths_to_deny = flagged
        .iter()
        .filter(|path| {
            !workspace_roots
                .iter()
                .any(|root| workspace_write_root_contains_path(root, path))
        })
        .collect::<Vec<_>>();
    for path in &paths_to_deny {
        ensure_case_insensitive_acl_path(path)?;
    }
    ensure_case_insensitive_acl_path(cwd)?;
    for root in &workspace_roots {
        ensure_case_insensitive_acl_path(root)?;
    }
    std::fs::create_dir_all(sandbox_home)?;
    let active_sids = if uses_write_capabilities {
        workspace_roots
            .iter()
            .map(|root| {
                workspace_write_cap_sid_for_root(sandbox_home, cwd, root)
                    .and_then(|sid| LocalSid::from_string(&sid))
            })
            .collect::<Result<Vec<_>>>()?
    } else {
        let sid = workspace_cap_sid_for_cwd(sandbox_home, cwd)?;
        vec![LocalSid::from_string(&sid)?]
    };
    for path in paths_to_deny {
        for active_sid in &active_sids {
            let res = unsafe { add_deny_write_ace(path, active_sid.as_ptr()) };
            match res {
                Ok(true) => log_note(
                    &format!("AUDIT: applied capability deny ACE to {}", path.display()),
                    logs_base_dir,
                ),
                Ok(false) => {}
                Err(err) => {
                    return Err(err).with_context(|| {
                        format!(
                            "world-writable audit could not deny capability writes to {}",
                            path.display()
                        )
                    });
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::gather_candidates;
    use std::collections::HashMap;
    use std::fs;

    #[test]
    fn gathers_path_entries_by_list_separator() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir_a = tmp.path().join("Tools");
        let dir_b = tmp.path().join("Bin");
        let dir_space = tmp.path().join("Program Files");
        fs::create_dir_all(&dir_a).expect("dir a");
        fs::create_dir_all(&dir_b).expect("dir b");
        fs::create_dir_all(&dir_space).expect("dir space");

        let mut env_map = HashMap::new();
        env_map.insert(
            "PATH".to_string(),
            format!(
                "{};{};{}",
                dir_a.display(),
                dir_b.display(),
                dir_space.display()
            ),
        );

        let candidates = gather_candidates(tmp.path(), &env_map);
        let canon_a = dir_a.canonicalize().expect("canon a");
        let canon_b = dir_b.canonicalize().expect("canon b");
        let canon_space = dir_space.canonicalize().expect("canon space");

        assert!(candidates.contains(&canon_a));
        assert!(candidates.contains(&canon_b));
        assert!(candidates.contains(&canon_space));
    }
}
