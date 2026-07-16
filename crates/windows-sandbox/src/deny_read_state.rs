use crate::deny_read_acl::apply_deny_read_acls;
use crate::deny_read_acl::lexical_path_key;
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
use windows_sys::Win32::Storage::FileSystem::MOVEFILE_REPLACE_EXISTING;
use windows_sys::Win32::Storage::FileSystem::MOVEFILE_WRITE_THROUGH;
use windows_sys::Win32::Storage::FileSystem::MoveFileExW;
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::System::Threading::INFINITE;
use windows_sys::Win32::System::Threading::ReleaseMutex;
use windows_sys::Win32::System::Threading::WaitForSingleObject;

const DENY_READ_ACL_STATE_FILE: &str = "deny_read_acl_state.json";

#[derive(Default, Deserialize, Serialize)]
struct PersistentDenyReadAclState {
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

fn state_mutex_name(path: &Path) -> String {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in path
        .to_string_lossy()
        .replace('\\', "/")
        .to_ascii_lowercase()
        .as_bytes()
    {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!(r"Local\SingularityDenyReadState_{hash:016x}")
}

pub(crate) fn lock_state(path: &Path) -> Result<StateMutex> {
    let name = to_wide(state_mutex_name(path));
    let handle = unsafe { CreateMutexW(std::ptr::null_mut(), 0, name.as_ptr()) };
    if handle == 0 {
        anyhow::bail!("CreateMutexW failed for deny-read state: {}", unsafe {
            GetLastError()
        });
    }
    let wait = unsafe { WaitForSingleObject(handle, INFINITE) };
    if wait != WAIT_OBJECT_0 && wait != WAIT_ABANDONED_0 {
        unsafe {
            CloseHandle(handle);
        }
        anyhow::bail!("WaitForSingleObject failed for deny-read state: {wait}");
    }
    Ok(StateMutex { handle })
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

/// Reconciles the persistent deny-read ACEs for one workspace capability SID.
///
/// ACLs are intentionally monotonic: a later workspace invocation never revokes an older
/// deny-read path because another process or an outliving descendant may still depend on it.
/// The union is serialized by a per-state-path mutex and committed with an atomic replace.
/// This removes the old shared Sandbox Users last-writer-wins/revoke race while preserving a
/// conservative fail-closed ACL set.
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
    let previous = state
        .principals
        .get(principal_sid)
        .cloned()
        .unwrap_or_default();
    let union = merge_monotonic_paths(&previous, desired_paths);
    let applied_paths = unsafe { apply_deny_read_acls(&union, psid) }?;

    if applied_paths.is_empty() {
        state.principals.remove(principal_sid);
    } else {
        state
            .principals
            .insert(principal_sid.to_string(), applied_paths.clone());
    }
    store_state(&state_path, &state)?;
    Ok(applied_paths)
}

fn merge_monotonic_paths(previous: &[PathBuf], desired: &[PathBuf]) -> Vec<PathBuf> {
    let mut keys = BTreeSet::new();
    let mut merged = Vec::new();
    for path in previous.iter().chain(desired) {
        if keys.insert(lexical_path_key(path)) {
            merged.push(path.clone());
        }
    }
    merged.sort_by_key(|path| lexical_path_key(path));
    merged
}

fn load_state(path: &Path) -> Result<PersistentDenyReadAclState> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .with_context(|| format!("parse deny-read ACL state {}", path.display())),
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
    use super::merge_monotonic_paths;
    use super::sandbox_dir;
    use super::sync_persistent_deny_read_acls;
    use crate::acl::dacl_has_read_deny_for_sid;
    use crate::acl::fetch_dacl_handle;
    use crate::deny_read_acl::lexical_path_key;
    use crate::token::LocalSid;
    use std::path::PathBuf;

    const CHILD_ENV: &str = "SINGULARITY_DENY_READ_STATE_CHILD";
    const HOME_ENV: &str = "SINGULARITY_DENY_READ_STATE_HOME";
    const PATH_ENV: &str = "SINGULARITY_DENY_READ_STATE_PATH";

    #[test]
    fn state_merge_never_retracts_an_older_deny() {
        let previous = vec![PathBuf::from(r"C:\workspace\.git")];
        let desired = vec![PathBuf::from(r"C:\workspace\.agents")];
        let merged = merge_monotonic_paths(&previous, &desired);
        assert_eq!(
            merged,
            vec![
                PathBuf::from(r"C:\workspace\.agents"),
                PathBuf::from(r"C:\workspace\.git")
            ]
        );
    }

    #[test]
    fn cross_process_deny_read_state_preserves_union() {
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
        let first = temp.path().join("first");
        let second = temp.path().join("second");
        std::fs::create_dir_all(&sandbox_home).expect("create sandbox home");
        std::fs::create_dir(&first).expect("create first path");
        std::fs::create_dir(&second).expect("create second path");

        let executable = std::env::current_exe().expect("current test executable");
        let mut child = std::process::Command::new(executable)
            .args([
                "--exact",
                "deny_read_state::tests::cross_process_deny_read_state_preserves_union",
                "--nocapture",
            ])
            .env(CHILD_ENV, "1")
            .env(HOME_ENV, &sandbox_home)
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
            .expect("merged principal state");
        let keys = paths
            .iter()
            .map(|path| lexical_path_key(path))
            .collect::<Vec<_>>();
        assert!(keys.contains(&lexical_path_key(&first)));
        assert!(keys.contains(&lexical_path_key(&second)));

        for path in [&first, &second] {
            let (dacl, security_descriptor) =
                unsafe { fetch_dacl_handle(path).expect("read applied DACL") };
            let has_deny = unsafe { dacl_has_read_deny_for_sid(dacl, sid.as_ptr()) };
            unsafe {
                windows_sys::Win32::Foundation::LocalFree(
                    security_descriptor as windows_sys::Win32::Foundation::HLOCAL,
                );
            }
            assert!(has_deny, "deny-read ACE missing for {}", path.display());
        }
    }
}
