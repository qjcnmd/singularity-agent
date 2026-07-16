use crate::acl::revoke_deny_read_ace;
use crate::deny_read_acl::apply_deny_read_acls_with_ownership;
use crate::path_normalization::canonical_path_key_allow_missing;
use crate::path_normalization::lexical_path_key;
use crate::setup::sandbox_dir;
use crate::winutil::to_wide;
use anyhow::Context;
use anyhow::Result;
use serde::Deserialize;
use serde::Serialize;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::ffi::c_void;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::time::SystemTime;
use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Foundation::WAIT_ABANDONED_0;
use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
use windows_sys::Win32::Foundation::WAIT_TIMEOUT;
use windows_sys::Win32::Storage::FileSystem::MOVEFILE_REPLACE_EXISTING;
use windows_sys::Win32::Storage::FileSystem::MOVEFILE_WRITE_THROUGH;
use windows_sys::Win32::Storage::FileSystem::MoveFileExW;
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::System::Threading::INFINITE;
use windows_sys::Win32::System::Threading::ReleaseMutex;
use windows_sys::Win32::System::Threading::WaitForSingleObject;

const DENY_READ_ACL_STATE_FILE: &str = "deny_read_acl_state.json";
const DENY_READ_ACL_STATE_VERSION: u32 = 2;
const STATE_MUTEX_PREFIX: &str = "SingularityDenyReadState";
const EXECUTION_MUTEX_PREFIX: &str = "SingularityDenyReadExecution";

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PersistentDenyReadAclState {
    version: u32,
    principals: BTreeMap<String, Vec<PathBuf>>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    legacy_unmanaged_principals: BTreeMap<String, Vec<PathBuf>>,
}

impl Default for PersistentDenyReadAclState {
    fn default() -> Self {
        Self {
            version: DENY_READ_ACL_STATE_VERSION,
            principals: BTreeMap::new(),
            legacy_unmanaged_principals: BTreeMap::new(),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyPersistentDenyReadAclState {
    principals: BTreeMap<String, Vec<PathBuf>>,
}

pub(crate) struct StateMutex {
    handle: HANDLE,
}

impl Drop for StateMutex {
    fn drop(&mut self) {
        unsafe {
            ReleaseMutex(self.handle);
            CloseHandle(self.handle);
        }
    }
}

fn mutex_name(prefix: &str, path: &Path) -> String {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in canonical_path_key_allow_missing(path).as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!(r"Global\{prefix}_{hash:016x}")
}

#[cfg(test)]
fn state_mutex_name(path: &Path) -> String {
    mutex_name(STATE_MUTEX_PREFIX, path)
}

fn wait_named_mutex(prefix: &str, path: &Path, timeout_ms: u32) -> Result<Option<StateMutex>> {
    let name = to_wide(mutex_name(prefix, path));
    let handle = unsafe { CreateMutexW(std::ptr::null_mut(), 0, name.as_ptr()) };
    if handle == 0 {
        anyhow::bail!("CreateMutexW failed for deny-read state: {}", unsafe {
            GetLastError()
        });
    }
    let wait = unsafe { WaitForSingleObject(handle, timeout_ms) };
    match wait {
        WAIT_OBJECT_0 | WAIT_ABANDONED_0 => Ok(Some(StateMutex { handle })),
        WAIT_TIMEOUT => {
            unsafe {
                CloseHandle(handle);
            }
            Ok(None)
        }
        _ => {
            unsafe {
                CloseHandle(handle);
            }
            anyhow::bail!("WaitForSingleObject failed for deny-read state: {wait}");
        }
    }
}

pub(crate) fn lock_state(path: &Path) -> Result<StateMutex> {
    wait_named_mutex(STATE_MUTEX_PREFIX, path, INFINITE)?
        .ok_or_else(|| anyhow::anyhow!("infinite deny-read state mutex wait timed out"))
}

/// Attempts to hold the shared read principal across one complete sandbox child lifecycle.
pub(crate) fn try_lock_deny_read_execution(
    sandbox_home: &Path,
    timeout_ms: u32,
) -> Result<Option<StateMutex>> {
    let state_path = sandbox_dir(sandbox_home).join(DENY_READ_ACL_STATE_FILE);
    wait_named_mutex(EXECUTION_MUTEX_PREFIX, &state_path, timeout_ms)
}

pub(crate) fn atomic_store(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("deny-read state has no parent"))?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("create deny-read state directory {}", parent.display()))?;
    let nonce = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos() as u64);
    let temporary = path.with_file_name(format!(
        ".{}.tmp-{}-{nonce:016x}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("state"),
        std::process::id()
    ));
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)
        .with_context(|| format!("create atomic deny-read state {}", temporary.display()))?;
    let write_result = (|| -> Result<()> {
        file.write_all(bytes)
            .context("write atomic deny-read state")?;
        file.flush().context("flush atomic deny-read state")?;
        file.sync_all().context("flush deny-read state to disk")?;
        drop(file);
        let source = to_wide(&temporary);
        let destination = to_wide(path);
        let moved = unsafe {
            MoveFileExW(
                source.as_ptr(),
                destination.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        };
        if moved == 0 {
            anyhow::bail!(
                "MoveFileExW failed for atomic deny-read state: {}",
                unsafe { GetLastError() }
            );
        }
        Ok(())
    })();
    if write_result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    write_result
}

/// Reconciles persistent deny-read ACEs for one sandbox principal.
///
/// As in Codex, the current desired set is applied before stale paths owned by the same SID are
/// revoked. Only ACEs actually added by this runtime, or already present in the versioned managed
/// state, may be revoked. The reconciliation is serialized by a canonical state-path mutex and
/// committed with an atomic replace. Product command callers additionally hold the execution
/// mutex across setup, child execution, and Job Object cleanup so another workspace cannot revoke
/// a live child's protection.
///
/// # Safety
/// Caller must pass a valid SID pointer matching `principal_sid`.
pub unsafe fn sync_persistent_deny_read_acls(
    sandbox_home: &Path,
    principal_sid: &str,
    desired_paths: &[PathBuf],
    psid: *mut c_void,
) -> Result<Vec<PathBuf>> {
    let state_path = sandbox_dir(sandbox_home).join(DENY_READ_ACL_STATE_FILE);
    let _lock = lock_state(&state_path)?;
    let mut state = load_state(&state_path)?;
    let previous_managed = state
        .principals
        .get(principal_sid)
        .cloned()
        .unwrap_or_default();
    let application = unsafe { apply_deny_read_acls_with_ownership(desired_paths, psid) }?;
    let desired_keys = application
        .enforced_paths
        .iter()
        .map(|path| lexical_path_key(path))
        .collect::<BTreeSet<_>>();
    let previous_managed_keys = previous_managed
        .iter()
        .map(|path| lexical_path_key(path))
        .collect::<BTreeSet<_>>();
    let newly_managed_keys = application
        .newly_managed_paths
        .iter()
        .map(|path| lexical_path_key(path))
        .collect::<BTreeSet<_>>();
    let current_managed_paths = application
        .enforced_paths
        .iter()
        .filter(|path| {
            let key = lexical_path_key(path);
            previous_managed_keys.contains(&key) || newly_managed_keys.contains(&key)
        })
        .cloned()
        .collect::<Vec<_>>();
    let mut retained_stale_paths = Vec::new();
    let mut revoke_errors = Vec::new();
    for path in previous_managed {
        if desired_keys.contains(&lexical_path_key(&path)) {
            continue;
        }
        match std::fs::symlink_metadata(&path) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                retained_stale_paths.push(path.clone());
                revoke_errors.push(format!("{}: {error}", path.display()));
                continue;
            }
        }
        if let Err(error) = unsafe { revoke_deny_read_ace(&path, psid) } {
            retained_stale_paths.push(path.clone());
            revoke_errors.push(format!("{}: {error}", path.display()));
        }
    }
    let tracked_paths = merge_tracked_paths(&current_managed_paths, &retained_stale_paths);

    if tracked_paths.is_empty() {
        state.principals.remove(principal_sid);
    } else {
        state
            .principals
            .insert(principal_sid.to_string(), tracked_paths);
    }
    let managed_keys = state
        .principals
        .get(principal_sid)
        .into_iter()
        .flatten()
        .map(|path| lexical_path_key(path))
        .collect::<BTreeSet<_>>();
    let retained_legacy = state
        .legacy_unmanaged_principals
        .remove(principal_sid)
        .unwrap_or_default()
        .into_iter()
        .filter(|path| {
            if managed_keys.contains(&lexical_path_key(path)) {
                return false;
            }
            match std::fs::symlink_metadata(path) {
                Ok(_) => true,
                Err(error) => error.kind() != std::io::ErrorKind::NotFound,
            }
        })
        .collect::<Vec<_>>();
    if !retained_legacy.is_empty() {
        state
            .legacy_unmanaged_principals
            .insert(principal_sid.to_string(), retained_legacy);
    }
    store_state(&state_path, &state)?;
    if !revoke_errors.is_empty() {
        anyhow::bail!(
            "failed to revoke stale deny-read ACLs: {}",
            revoke_errors.join("; ")
        );
    }
    Ok(application.enforced_paths)
}

fn merge_tracked_paths(current: &[PathBuf], retained_stale: &[PathBuf]) -> Vec<PathBuf> {
    let mut keys = BTreeSet::new();
    let mut merged = Vec::new();
    for path in current.iter().chain(retained_stale) {
        if keys.insert(lexical_path_key(path)) {
            merged.push(path.clone());
        }
    }
    merged.sort_by_key(|path| lexical_path_key(path));
    merged
}

fn load_state(path: &Path) -> Result<PersistentDenyReadAclState> {
    match std::fs::read(path) {
        Ok(bytes) => {
            let value: serde_json::Value = serde_json::from_slice(&bytes)
                .with_context(|| format!("parse deny-read ACL state {}", path.display()))?;
            match value.get("version").and_then(serde_json::Value::as_u64) {
                Some(version) if version == u64::from(DENY_READ_ACL_STATE_VERSION) => {
                    serde_json::from_value(value)
                        .with_context(|| format!("parse deny-read ACL state {}", path.display()))
                }
                Some(version) => anyhow::bail!(
                    "unsupported deny-read ACL state version {version} in {}",
                    path.display()
                ),
                None => {
                    let legacy: LegacyPersistentDenyReadAclState = serde_json::from_value(value)
                        .with_context(|| {
                            format!("parse legacy deny-read ACL state {}", path.display())
                        })?;
                    Ok(PersistentDenyReadAclState {
                        legacy_unmanaged_principals: legacy.principals,
                        ..PersistentDenyReadAclState::default()
                    })
                }
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            Ok(PersistentDenyReadAclState::default())
        }
        Err(err) => {
            Err(err).with_context(|| format!("read deny-read ACL state {}", path.display()))
        }
    }
}

fn store_state(path: &Path, state: &PersistentDenyReadAclState) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(state).context("serialize deny-read ACL state")?;
    atomic_store(path, &bytes)
}

#[cfg(test)]
mod tests {
    use super::DENY_READ_ACL_STATE_FILE;
    use super::load_state;
    use super::merge_tracked_paths;
    use super::sandbox_dir;
    use super::state_mutex_name;
    use super::sync_persistent_deny_read_acls;
    use super::try_lock_deny_read_execution;
    use crate::acl::add_deny_read_ace;
    use crate::acl::dacl_has_read_deny_for_sid;
    use crate::acl::fetch_dacl_handle;
    use crate::acl::revoke_deny_read_ace;
    use crate::path_normalization::lexical_path_key;
    use crate::token::LocalSid;
    use std::os::windows::process::CommandExt;
    use std::path::Path;
    use std::path::PathBuf;
    use std::time::Duration;
    use windows_sys::Win32::Foundation::HLOCAL;
    use windows_sys::Win32::Foundation::LocalFree;

    const CHILD_ENV: &str = "SINGULARITY_DENY_READ_STATE_CHILD";
    const EXECUTION_CHILD_ENV: &str = "SINGULARITY_DENY_READ_EXECUTION_CHILD";
    const EXECUTION_ACQUIRED_ENV: &str = "SINGULARITY_DENY_READ_EXECUTION_ACQUIRED";
    const HOME_ENV: &str = "SINGULARITY_DENY_READ_STATE_HOME";
    const PATH_ENV: &str = "SINGULARITY_DENY_READ_STATE_PATH";

    #[test]
    fn state_mutex_uses_global_namespace_for_cross_session_state() {
        let name = state_mutex_name(Path::new(r"C:\sandbox\.sandbox\deny_read_acl_state.json"));
        assert!(name.starts_with(r"Global\"), "mutex name was {name}");
    }

    #[test]
    fn tracked_paths_include_only_current_and_unreconciled_entries() {
        let current = vec![PathBuf::from(r"C:\workspace\.agents")];
        let retained_stale = vec![PathBuf::from(r"C:\workspace\.git")];
        let merged = merge_tracked_paths(&current, &retained_stale);
        assert_eq!(
            merged,
            vec![
                PathBuf::from(r"C:\workspace\.agents"),
                PathBuf::from(r"C:\workspace\.git")
            ]
        );
    }

    #[test]
    fn equivalent_state_path_spellings_share_one_mutex() {
        let temp = tempfile::tempdir().expect("tempdir");
        let sandbox_home = temp.path().join("singularity-home");
        let alias = temp.path().join("singularity-home-alias");
        std::fs::create_dir_all(sandbox_dir(&sandbox_home)).expect("create sandbox state");
        create_junction(&alias, &sandbox_home);
        let ordinary = sandbox_dir(&sandbox_home).join(DENY_READ_ACL_STATE_FILE);
        let through_alias = sandbox_dir(&alias).join(DENY_READ_ACL_STATE_FILE);
        let verbatim = PathBuf::from(format!(r"\\?\{}", ordinary.display()));

        assert_eq!(
            state_mutex_name(&ordinary),
            state_mutex_name(&through_alias)
        );
        assert_eq!(state_mutex_name(&ordinary), state_mutex_name(&verbatim));
    }

    #[test]
    fn execution_mutex_covers_cross_process_child_lifetime() {
        let sandbox_home =
            PathBuf::from(std::env::var_os(HOME_ENV).unwrap_or_else(|| "unused".into()));
        if std::env::var_os(EXECUTION_CHILD_ENV).is_some() {
            let acquired =
                PathBuf::from(std::env::var_os(EXECUTION_ACQUIRED_ENV).expect("acquired marker"));
            let _guard = try_lock_deny_read_execution(&sandbox_home, u32::MAX)
                .expect("lock execution mutex")
                .expect("infinite wait acquires execution mutex");
            std::fs::write(acquired, b"acquired").expect("write acquired marker");
            return;
        }

        let temp = tempfile::tempdir().expect("tempdir");
        let sandbox_home = temp.path().join("singularity-home");
        let acquired = temp.path().join("acquired");
        std::fs::create_dir_all(&sandbox_home).expect("create sandbox home");
        let guard = try_lock_deny_read_execution(&sandbox_home, u32::MAX)
            .expect("lock parent execution mutex")
            .expect("parent acquires execution mutex");
        let executable = std::env::current_exe().expect("current test executable");
        let mut child = std::process::Command::new(executable)
            .args([
                "--exact",
                "deny_read_state::tests::execution_mutex_covers_cross_process_child_lifetime",
                "--nocapture",
            ])
            .env(EXECUTION_CHILD_ENV, "1")
            .env(HOME_ENV, &sandbox_home)
            .env(EXECUTION_ACQUIRED_ENV, &acquired)
            .spawn()
            .expect("spawn execution-lock child");

        std::thread::sleep(Duration::from_millis(250));
        assert!(
            !acquired.exists(),
            "another process acquired the execution mutex before the active lifetime ended"
        );
        drop(guard);
        assert!(child.wait().expect("wait execution-lock child").success());
        assert!(
            acquired.exists(),
            "child must acquire after the guard drops"
        );
    }

    #[test]
    fn removed_historical_path_is_not_rematerialized() {
        let temp = tempfile::tempdir().expect("tempdir");
        let sandbox_home = temp.path().join("singularity-home");
        let historical = temp.path().join("historical");
        let current = temp.path().join("current");
        std::fs::create_dir_all(&sandbox_home).expect("create sandbox home");
        std::fs::create_dir(&historical).expect("create historical path");
        std::fs::create_dir(&current).expect("create current path");
        let sid = LocalSid::from_string("S-1-5-21-1-2-3-4").expect("test SID");

        unsafe {
            sync_persistent_deny_read_acls(
                &sandbox_home,
                "S-1-5-21-1-2-3-4",
                std::slice::from_ref(&historical),
                sid.as_ptr(),
            )
        }
        .expect("apply historical deny-read state");
        std::fs::remove_dir(&historical).expect("remove historical path");
        unsafe {
            sync_persistent_deny_read_acls(
                &sandbox_home,
                "S-1-5-21-1-2-3-4",
                std::slice::from_ref(&current),
                sid.as_ptr(),
            )
        }
        .expect("reconcile current deny-read state");

        assert!(!historical.exists());
        let state = load_state(&sandbox_dir(&sandbox_home).join(DENY_READ_ACL_STATE_FILE))
            .expect("load reconciled deny-read state");
        assert_eq!(
            state.principals.get("S-1-5-21-1-2-3-4"),
            Some(&vec![current.clone()])
        );
        unsafe {
            revoke_deny_read_ace(&current, sid.as_ptr()).expect("restore current ACL");
        }
    }

    #[test]
    fn preexisting_exact_deny_is_enforced_but_never_claimed_or_revoked() {
        let temp = tempfile::tempdir().expect("tempdir");
        let sandbox_home = temp.path().join("singularity-home");
        let protected = temp.path().join("protected");
        std::fs::create_dir_all(&sandbox_home).expect("create sandbox home");
        std::fs::create_dir(&protected).expect("create protected path");
        let principal = "S-1-5-21-1-2-3-4";
        let sid = LocalSid::from_string(principal).expect("test SID");
        assert!(
            unsafe { add_deny_read_ace(&protected, sid.as_ptr()) }
                .expect("seed external exact deny")
        );

        unsafe {
            sync_persistent_deny_read_acls(
                &sandbox_home,
                principal,
                std::slice::from_ref(&protected),
                sid.as_ptr(),
            )
        }
        .expect("enforce preexisting deny");
        let state_path = sandbox_dir(&sandbox_home).join(DENY_READ_ACL_STATE_FILE);
        let state = load_state(&state_path).expect("load managed state");
        assert!(
            !state.principals.contains_key(principal),
            "a sufficient preexisting ACE must not become runtime-owned"
        );

        unsafe { sync_persistent_deny_read_acls(&sandbox_home, principal, &[], sid.as_ptr()) }
            .expect("reconcile empty desired set");
        let (dacl, security_descriptor) =
            unsafe { fetch_dacl_handle(&protected).expect("read retained DACL") };
        assert!(unsafe { dacl_has_read_deny_for_sid(dacl, sid.as_ptr()) });
        unsafe {
            LocalFree(security_descriptor as HLOCAL);
            revoke_deny_read_ace(&protected, sid.as_ptr()).expect("restore external test ACE");
        }
    }

    #[test]
    fn legacy_state_is_migrated_as_unmanaged_and_not_revoked() {
        let temp = tempfile::tempdir().expect("tempdir");
        let sandbox_home = temp.path().join("singularity-home");
        let protected = temp.path().join("legacy-protected");
        std::fs::create_dir_all(&protected).expect("create protected path");
        let principal = "S-1-5-21-1-2-3-4";
        let sid = LocalSid::from_string(principal).expect("test SID");
        assert!(
            unsafe { add_deny_read_ace(&protected, sid.as_ptr()) }.expect("seed legacy exact deny")
        );
        let state_path = sandbox_dir(&sandbox_home).join(DENY_READ_ACL_STATE_FILE);
        std::fs::create_dir_all(state_path.parent().expect("state parent"))
            .expect("create state directory");
        std::fs::write(
            &state_path,
            serde_json::to_vec(&serde_json::json!({
                "principals": { principal: [protected.clone()] }
            }))
            .expect("serialize legacy state"),
        )
        .expect("write legacy state");

        unsafe { sync_persistent_deny_read_acls(&sandbox_home, principal, &[], sid.as_ptr()) }
            .expect("migrate legacy state");
        let state = load_state(&state_path).expect("load migrated state");
        assert_eq!(state.version, super::DENY_READ_ACL_STATE_VERSION);
        assert_eq!(
            state.legacy_unmanaged_principals.get(principal),
            Some(&vec![protected.clone()])
        );
        let (dacl, security_descriptor) =
            unsafe { fetch_dacl_handle(&protected).expect("read retained legacy DACL") };
        assert!(unsafe { dacl_has_read_deny_for_sid(dacl, sid.as_ptr()) });
        unsafe {
            LocalFree(security_descriptor as HLOCAL);
            revoke_deny_read_ace(&protected, sid.as_ptr()).expect("restore legacy test ACE");
        }
    }

    #[test]
    fn cross_process_equivalent_home_spellings_reconcile_without_corruption() {
        if std::env::var_os(CHILD_ENV).is_some() {
            let sandbox_home = PathBuf::from(std::env::var_os(HOME_ENV).expect("sandbox home"));
            let path = PathBuf::from(std::env::var_os(PATH_ENV).expect("deny path"));
            let sid = LocalSid::from_string("S-1-5-21-1-2-3-4").expect("test SID");
            unsafe {
                sync_persistent_deny_read_acls(
                    &sandbox_home,
                    "S-1-5-21-1-2-3-4",
                    std::slice::from_ref(&path),
                    sid.as_ptr(),
                )
            }
            .expect("child deny-read update");
            return;
        }

        let temp = tempfile::tempdir().expect("tempdir");
        let sandbox_home = temp.path().join("singularity-home");
        let sandbox_home_alias = temp.path().join("singularity-home-alias");
        let first = temp.path().join("first");
        let second = temp.path().join("second");
        std::fs::create_dir_all(&sandbox_home).expect("create sandbox home");
        create_junction(&sandbox_home_alias, &sandbox_home);
        std::fs::create_dir(&first).expect("create first path");
        std::fs::create_dir(&second).expect("create second path");

        let executable = std::env::current_exe().expect("current test executable");
        let mut child = std::process::Command::new(executable)
            .args([
                "--exact",
                "deny_read_state::tests::cross_process_equivalent_home_spellings_reconcile_without_corruption",
                "--nocapture",
            ])
            .env(CHILD_ENV, "1")
            .env(HOME_ENV, &sandbox_home_alias)
            .env(PATH_ENV, &second)
            .spawn()
            .expect("spawn deny-read child");

        let sid = LocalSid::from_string("S-1-5-21-1-2-3-4").expect("test SID");
        unsafe {
            sync_persistent_deny_read_acls(
                &sandbox_home,
                "S-1-5-21-1-2-3-4",
                std::slice::from_ref(&first),
                sid.as_ptr(),
            )
        }
        .expect("parent deny-read update");
        assert!(child.wait().expect("wait deny-read child").success());

        let state = load_state(&sandbox_dir(&sandbox_home).join(DENY_READ_ACL_STATE_FILE))
            .expect("load merged deny-read state");
        let paths = state
            .principals
            .get("S-1-5-21-1-2-3-4")
            .expect("reconciled principal state");
        assert_eq!(paths.len(), 1);
        let active_key = lexical_path_key(&paths[0]);
        assert!(active_key == lexical_path_key(&first) || active_key == lexical_path_key(&second));

        for path in [&first, &second] {
            let (dacl, security_descriptor) =
                unsafe { fetch_dacl_handle(path).expect("read applied DACL") };
            let has_deny = unsafe { dacl_has_read_deny_for_sid(dacl, sid.as_ptr()) };
            unsafe {
                LocalFree(security_descriptor as HLOCAL);
            }
            assert_eq!(
                has_deny,
                lexical_path_key(path) == active_key,
                "ACL state disagreed for {}",
                path.display()
            );
            unsafe {
                revoke_deny_read_ace(path, sid.as_ptr()).expect("restore path ACL");
            }
        }
    }

    fn create_junction(alias: &Path, target: &Path) {
        let link = format!("\"{}\"", alias.display());
        let target = format!("\"{}\"", target.display());
        let output = std::process::Command::new("cmd.exe")
            .raw_arg("/c")
            .raw_arg("mklink")
            .raw_arg("/J")
            .raw_arg(&link)
            .raw_arg(&target)
            .output()
            .expect("run mklink");
        assert!(
            output.status.success() && alias.exists(),
            "junction fixture must be available: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
