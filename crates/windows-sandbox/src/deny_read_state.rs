use crate::acl::deny_read_acl_fingerprint;
use crate::acl::revoke_deny_read_ace_with_fingerprint;
use crate::deny_read_acl::ManagedDenyReadAcl;
use crate::deny_read_acl::apply_deny_read_acls_with_ownership_before_set;
use crate::deny_read_acl::plan_deny_read_acl_paths;
use crate::path_normalization::lexical_path_key;
use crate::path_safety::canonicalize_case_insensitive_state_path;
use crate::path_safety::ensure_case_insensitive_acl_path;
use crate::setup::sandbox_dir;
use crate::token::current_user_sid_bytes;
use crate::winutil::resolve_sid;
use crate::winutil::string_from_sid_bytes;
use crate::winutil::to_wide;
use anyhow::Context;
use anyhow::Result;
use rand::Rng;
use rand::SeedableRng;
use rand::rngs::SmallRng;
use serde::Deserialize;
use serde::Serialize;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::ffi::c_void;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::ptr;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::Foundation::ERROR_ALREADY_EXISTS;
use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Foundation::HLOCAL;
use windows_sys::Win32::Foundation::LocalFree;
use windows_sys::Win32::Foundation::WAIT_ABANDONED_0;
use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
use windows_sys::Win32::Foundation::WAIT_TIMEOUT;
use windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
use windows_sys::Win32::Security::PSECURITY_DESCRIPTOR;
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::MOVEFILE_REPLACE_EXISTING;
use windows_sys::Win32::Storage::FileSystem::MOVEFILE_WRITE_THROUGH;
use windows_sys::Win32::Storage::FileSystem::MoveFileExW;
use windows_sys::Win32::Storage::FileSystem::SYNCHRONIZE;
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::System::Threading::INFINITE;
use windows_sys::Win32::System::Threading::MUTEX_MODIFY_STATE;
use windows_sys::Win32::System::Threading::OpenMutexW;
use windows_sys::Win32::System::Threading::ReleaseMutex;
use windows_sys::Win32::System::Threading::WaitForSingleObject;

const DENY_READ_ACL_STATE_FILE: &str = "deny_read_acl_state.json";
const DENY_READ_ACL_STATE_VERSION: u32 = 4;
const STATE_MUTEX_PREFIX: &str = "SingularityDenyReadState";
const EXECUTION_MUTEX_PREFIX: &str = "SingularityDenyReadExecution";
const RUNNER_LEASE_MUTEX_PREFIX: &str = r"Global\SingularityDenyReadRunner_";
const MAX_ACTIVE_RUNNER_LEASES: usize = 64;
const RUNNER_LEASE_NONCE_HEX_LEN: usize = 32;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PersistentDenyReadAclState {
    version: u32,
    principals: BTreeMap<String, Vec<ManagedDenyReadAcl>>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pending_principals: BTreeMap<String, Vec<ManagedDenyReadAcl>>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    legacy_unmanaged_principals: BTreeMap<String, Vec<PathBuf>>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    active_runner_leases: BTreeSet<String>,
}

impl Default for PersistentDenyReadAclState {
    fn default() -> Self {
        Self {
            version: DENY_READ_ACL_STATE_VERSION,
            principals: BTreeMap::new(),
            pending_principals: BTreeMap::new(),
            legacy_unmanaged_principals: BTreeMap::new(),
            active_runner_leases: BTreeSet::new(),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyPersistentDenyReadAclState {
    principals: BTreeMap<String, Vec<PathBuf>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct VersionTwoPersistentDenyReadAclState {
    version: u32,
    principals: BTreeMap<String, Vec<PathBuf>>,
    #[serde(default)]
    legacy_unmanaged_principals: BTreeMap<String, Vec<PathBuf>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct VersionThreePersistentDenyReadAclState {
    version: u32,
    principals: BTreeMap<String, Vec<ManagedDenyReadAcl>>,
    #[serde(default)]
    legacy_unmanaged_principals: BTreeMap<String, Vec<PathBuf>>,
    #[serde(default)]
    active_runner_leases: BTreeSet<String>,
}

pub(crate) struct StateMutex {
    handle: HANDLE,
}

/// Couples a state mutex with the exact canonical path identity used to name it.
pub(crate) struct StateLock {
    _mutex: StateMutex,
    path: PathBuf,
}

impl StateLock {
    /// Returns the canonical path that must be used for all I/O protected by this lock.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
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
    for byte in lexical_path_key(path).as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!(r"Global\{prefix}_{hash:016x}")
}

#[cfg(test)]
fn state_mutex_name(path: &Path) -> Result<String> {
    let path = canonicalize_case_insensitive_state_path(path)?;
    Ok(mutex_name(STATE_MUTEX_PREFIX, &path))
}

fn wait_raw_named_mutex(name: &str, timeout_ms: u32) -> Result<Option<StateMutex>> {
    let name = to_wide(name);
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

fn wait_existing_runner_mutex(name: &str) -> Result<StateMutex> {
    let name = to_wide(name);
    // SAFETY: `name` is a live, NUL-terminated UTF-16 buffer for the duration of the call. The
    // returned HANDLE is owned here and is either transferred to `StateMutex` or closed below.
    let handle = unsafe { OpenMutexW(SYNCHRONIZE | MUTEX_MODIFY_STATE, 0, name.as_ptr()) };
    if handle == 0 {
        // SAFETY: this immediately reads the calling thread's last-error value after OpenMutexW.
        anyhow::bail!(
            "open registered deny-read runner lease failed: {}",
            unsafe { GetLastError() }
        );
    }
    // SAFETY: `handle` is a valid mutex HANDLE opened above and remains owned by this function.
    let wait = unsafe { WaitForSingleObject(handle, INFINITE) };
    match wait {
        WAIT_OBJECT_0 | WAIT_ABANDONED_0 => Ok(StateMutex { handle }),
        _ => {
            // SAFETY: this branch has not transferred the owned HANDLE into `StateMutex`.
            unsafe {
                CloseHandle(handle);
            }
            anyhow::bail!("wait for registered deny-read runner lease failed: {wait}");
        }
    }
}

fn wait_named_mutex(prefix: &str, path: &Path, timeout_ms: u32) -> Result<Option<StateMutex>> {
    let path = canonicalize_case_insensitive_state_path(path)?;
    wait_raw_named_mutex(&mutex_name(prefix, &path), timeout_ms)
}

pub(crate) fn lock_state(path: &Path) -> Result<StateLock> {
    let path = canonicalize_case_insensitive_state_path(path)?;
    let mutex = wait_raw_named_mutex(&mutex_name(STATE_MUTEX_PREFIX, &path), INFINITE)?
        .ok_or_else(|| anyhow::anyhow!("infinite deny-read state mutex wait timed out"))?;
    Ok(StateLock {
        _mutex: mutex,
        path,
    })
}

/// Attempts to hold the shared read principal across one complete sandbox child lifecycle.
pub(crate) fn try_lock_deny_read_execution(
    sandbox_home: &Path,
    timeout_ms: u32,
) -> Result<Option<StateMutex>> {
    let state_path = sandbox_dir(sandbox_home).join(DENY_READ_ACL_STATE_FILE);
    wait_named_mutex(EXECUTION_MUTEX_PREFIX, &state_path, timeout_ms)
}

/// Runner-owned mutex guard that survives loss of the calling parent process.
pub struct RunnerLeaseGuard {
    mutex: Option<StateMutex>,
}

/// Parent registration handle kept alive until the runner confirms child startup.
pub(crate) struct RegisteredRunnerLease {
    lease_name: String,
    handle: HANDLE,
}

impl RegisteredRunnerLease {
    pub(crate) fn name(&self) -> &str {
        &self.lease_name
    }
}

impl Drop for RegisteredRunnerLease {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.handle);
        }
    }
}

impl RunnerLeaseGuard {
    /// Releases the lease only after the runner has completed Job Object cleanup.
    pub fn release(mut self) {
        drop(self.mutex.take());
    }
}

/// Creates and registers a runner lease before the parent sends a spawn request.
///
/// The parent keeps this handle open through `SpawnReady`, preserving the exact DACL that permits
/// both the real user and the selected sandbox user to open the mutex. The runner then owns the
/// mutex through Job Object cleanup.
pub(crate) fn register_runner_lease(
    sandbox_home: &Path,
    sandbox_username: &str,
) -> Result<RegisteredRunnerLease> {
    let state_path = sandbox_dir(sandbox_home).join(DENY_READ_ACL_STATE_FILE);
    let lock = lock_state(&state_path)?;
    let state_path = lock.path();
    let mut state = load_state(state_path)?;
    if state.active_runner_leases.len() >= MAX_ACTIVE_RUNNER_LEASES {
        anyhow::bail!(
            "deny-read runner lease limit reached: {}",
            state.active_runner_leases.len()
        );
    }
    let current_sid = string_from_sid_bytes(&current_user_sid_bytes()?)
        .map_err(anyhow::Error::msg)
        .context("resolve current user SID for deny-read runner lease")?;
    let sandbox_sid = string_from_sid_bytes(&resolve_sid(sandbox_username)?)
        .map_err(anyhow::Error::msg)
        .context("resolve sandbox user SID for deny-read runner lease")?;
    let sddl = to_wide(format!(
        "D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;{current_sid})(A;;GA;;;{sandbox_sid})"
    ));
    let mut security_descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
    let converted = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            1,
            &mut security_descriptor,
            ptr::null_mut(),
        )
    };
    if converted == 0 {
        anyhow::bail!(
            "create deny-read runner lease security descriptor failed: {}",
            unsafe { GetLastError() }
        );
    }
    let mut security_attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: security_descriptor,
        bInheritHandle: 0,
    };
    let nonce: u128 = SmallRng::from_entropy().r#gen();
    let lease_name = format!("{RUNNER_LEASE_MUTEX_PREFIX}{nonce:032x}");
    let wide_name = to_wide(&lease_name);
    let handle = unsafe {
        CreateMutexW(
            &mut security_attributes as *mut SECURITY_ATTRIBUTES,
            0,
            wide_name.as_ptr(),
        )
    };
    let create_error = unsafe { GetLastError() };
    unsafe {
        LocalFree(security_descriptor as HLOCAL);
    }
    if handle == 0 {
        anyhow::bail!("CreateMutexW failed for deny-read runner lease: {create_error}");
    }
    if create_error == ERROR_ALREADY_EXISTS {
        unsafe {
            CloseHandle(handle);
        }
        anyhow::bail!("deny-read runner lease collision");
    }
    if !state.active_runner_leases.insert(lease_name.clone()) {
        unsafe {
            CloseHandle(handle);
        }
        anyhow::bail!("deny-read runner lease collision");
    }
    if let Err(error) = store_state(state_path, &state) {
        unsafe {
            CloseHandle(handle);
        }
        return Err(error);
    }
    Ok(RegisteredRunnerLease { lease_name, handle })
}

/// Acquires the unpredictable, DACL-protected lease created by the parent before child spawn.
///
/// Persistent lease bookkeeping belongs to the parent. The runner executes as a distinct sandbox
/// identity and must not read the real user's state directory to validate the kernel object it was
/// handed over the parent-authenticated pipe.
pub fn acquire_registered_runner_lease(lease_name: &str) -> Result<RunnerLeaseGuard> {
    validate_runner_lease_name(lease_name)?;
    let mutex = wait_existing_runner_mutex(lease_name)?;
    Ok(RunnerLeaseGuard { mutex: Some(mutex) })
}

/// Waits for active runner-owned leases and prunes released or abandoned entries.
pub(crate) fn reconcile_runner_leases(sandbox_home: &Path, timeout_ms: u32) -> Result<bool> {
    let state_path = sandbox_dir(sandbox_home).join(DENY_READ_ACL_STATE_FILE);
    let deadline = (timeout_ms != INFINITE)
        .then(|| Instant::now() + Duration::from_millis(u64::from(timeout_ms)));
    let (state_path, leases) = {
        let lock = lock_state(&state_path)?;
        let state_path = lock.path().to_path_buf();
        let leases = load_state(&state_path)?
            .active_runner_leases
            .into_iter()
            .collect::<Vec<_>>();
        (state_path, leases)
    };
    for lease_name in leases {
        validate_runner_lease_name(&lease_name)?;
        let remaining_ms = deadline.map_or(INFINITE, |deadline| {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                0
            } else {
                remaining.as_millis().clamp(1, u128::from(u32::MAX)) as u32
            }
        });
        if remaining_ms == 0 {
            return Ok(false);
        }
        let Some(lease_mutex) = wait_raw_named_mutex(&lease_name, remaining_ms)? else {
            return Ok(false);
        };
        remove_runner_lease(&state_path, &lease_name)?;
        drop(lease_mutex);
    }
    Ok(true)
}

fn remove_runner_lease(state_path: &Path, lease_name: &str) -> Result<()> {
    let lock = lock_state(state_path)?;
    let state_path = lock.path();
    let mut state = load_state(state_path)?;
    if state.active_runner_leases.remove(lease_name) {
        store_state(state_path, &state)?;
    }
    Ok(())
}

unsafe fn recover_pending_principal(
    state: &mut PersistentDenyReadAclState,
    principal_sid: &str,
    psid: *mut c_void,
) -> Result<bool> {
    let Some(pending) = state.pending_principals.get(principal_sid).cloned() else {
        return Ok(false);
    };
    let mut managed = state
        .principals
        .get(principal_sid)
        .cloned()
        .unwrap_or_default();
    for entry in pending {
        let current = match std::fs::symlink_metadata(&entry.path) {
            Ok(_) => {
                unsafe { deny_read_acl_fingerprint(&entry.path, psid) }.with_context(|| {
                    format!(
                        "recover pending deny-read ACL fingerprint {}",
                        entry.path.display()
                    )
                })?
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "inspect pending deny-read ACL target {}",
                        entry.path.display()
                    )
                });
            }
        };
        if current == entry.fingerprint {
            upsert_managed_path(&mut managed, entry);
        } else if !current.is_empty() {
            anyhow::bail!(
                "pending deny-read ACL fingerprint changed for {}",
                entry.path.display()
            );
        }
    }
    state.pending_principals.remove(principal_sid);
    if managed.is_empty() {
        state.principals.remove(principal_sid);
    } else {
        managed.sort_by_key(|entry| lexical_path_key(&entry.path));
        state.principals.insert(principal_sid.to_string(), managed);
    }
    Ok(true)
}

fn upsert_managed_path(entries: &mut Vec<ManagedDenyReadAcl>, candidate: ManagedDenyReadAcl) {
    let key = lexical_path_key(&candidate.path);
    if let Some(existing) = entries
        .iter_mut()
        .find(|entry| lexical_path_key(&entry.path) == key)
    {
        *existing = candidate;
    } else {
        entries.push(candidate);
    }
}

fn validate_runner_lease_name(lease_name: &str) -> Result<()> {
    let Some(nonce) = lease_name.strip_prefix(RUNNER_LEASE_MUTEX_PREFIX) else {
        anyhow::bail!("invalid deny-read runner lease name");
    };
    if nonce.len() != RUNNER_LEASE_NONCE_HEX_LEN
        || !nonce.chars().all(|character| character.is_ascii_hexdigit())
    {
        anyhow::bail!("invalid deny-read runner lease name");
    }
    Ok(())
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
/// mutex across setup and normal child cleanup; a registered runner-owned mutex extends the same
/// exclusion through Job Object cleanup when the calling parent crashes.
///
/// # Safety
/// Caller must pass a valid SID pointer matching `principal_sid`.
pub unsafe fn sync_persistent_deny_read_acls(
    sandbox_home: &Path,
    principal_sid: &str,
    desired_paths: &[PathBuf],
    psid: *mut c_void,
) -> Result<Vec<PathBuf>> {
    drop(plan_deny_read_acl_paths(desired_paths)?);
    let state_path = sandbox_dir(sandbox_home).join(DENY_READ_ACL_STATE_FILE);
    let lock = lock_state(&state_path)?;
    let state_path = lock.path();
    let mut state = load_state(state_path)?;
    for managed in state
        .principals
        .get(principal_sid)
        .into_iter()
        .flatten()
        .chain(
            state
                .pending_principals
                .get(principal_sid)
                .into_iter()
                .flatten(),
        )
    {
        ensure_case_insensitive_acl_path(&managed.path)?;
    }
    for path in state
        .legacy_unmanaged_principals
        .get(principal_sid)
        .into_iter()
        .flatten()
    {
        ensure_case_insensitive_acl_path(path)?;
    }
    if unsafe { recover_pending_principal(&mut state, principal_sid, psid) }? {
        store_state(state_path, &state).context("commit recovered pending deny-read ownership")?;
    }
    let previous_managed = state
        .principals
        .get(principal_sid)
        .cloned()
        .unwrap_or_default();
    let application_result = unsafe {
        apply_deny_read_acls_with_ownership_before_set(desired_paths, psid, &mut |pending| {
            upsert_managed_path(
                state
                    .pending_principals
                    .entry(principal_sid.to_string())
                    .or_default(),
                pending.clone(),
            );
            store_state(state_path, &state)
                .context("journal pending deny-read ownership before ACL mutation")
        })
    };
    let application = match application_result {
        Ok(application) => application,
        Err(error) => {
            let recovery = unsafe { recover_pending_principal(&mut state, principal_sid, psid) }
                .and_then(|changed| {
                    changed
                        .then(|| store_state(state_path, &state))
                        .transpose()
                        .map(|_| ())
                });
            return match recovery {
                Ok(()) => Err(error),
                Err(recovery_error) => Err(error.context(format!(
                    "pending deny-read ownership recovery failed: {recovery_error}"
                ))),
            };
        }
    };
    let desired_keys = application
        .enforced_paths
        .iter()
        .map(|path| lexical_path_key(path))
        .collect::<BTreeSet<_>>();
    let mut previous_by_key = previous_managed
        .into_iter()
        .map(|managed| (lexical_path_key(&managed.path), managed))
        .collect::<BTreeMap<_, _>>();
    let mut newly_managed_by_key = application
        .newly_managed_paths
        .into_iter()
        .map(|managed| (lexical_path_key(&managed.path), managed))
        .collect::<BTreeMap<_, _>>();
    let mut current_managed = Vec::new();
    for path in &application.enforced_paths {
        let key = lexical_path_key(path);
        if let Some(mut managed) = newly_managed_by_key.remove(&key) {
            managed.path = path.clone();
            current_managed.push(managed);
        } else if let Some(mut managed) = previous_by_key.remove(&key) {
            managed.path = path.clone();
            current_managed.push(managed);
        }
    }
    let mut retained_stale = Vec::new();
    let mut revoke_error_count = 0usize;
    let mut preferred_revoke_error = None;
    for (_, managed) in previous_by_key {
        if desired_keys.contains(&lexical_path_key(&managed.path)) {
            continue;
        }
        match std::fs::symlink_metadata(&managed.path) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                revoke_error_count = revoke_error_count.saturating_add(1);
                retain_preferred_reconciliation_error(
                    &mut preferred_revoke_error,
                    anyhow::Error::new(error).context(format!(
                        "inspect stale deny-read target {}",
                        managed.path.display()
                    )),
                );
                retained_stale.push(managed);
                continue;
            }
        }
        let current_fingerprint = match unsafe { deny_read_acl_fingerprint(&managed.path, psid) } {
            Ok(fingerprint) => fingerprint,
            Err(error) => {
                revoke_error_count = revoke_error_count.saturating_add(1);
                retain_preferred_reconciliation_error(
                    &mut preferred_revoke_error,
                    error.context(format!(
                        "inspect stale deny-read ACL {}",
                        managed.path.display()
                    )),
                );
                retained_stale.push(managed);
                continue;
            }
        };
        if current_fingerprint.is_empty() {
            continue;
        }
        if current_fingerprint != managed.fingerprint {
            revoke_error_count = revoke_error_count.saturating_add(1);
            retain_preferred_reconciliation_error(
                &mut preferred_revoke_error,
                anyhow::anyhow!(
                    "ownership fingerprint changed (current={current_fingerprint:?}, expected={:?})",
                    managed.fingerprint
                )
                .context(format!(
                    "refuse stale deny-read ACL revocation for {}",
                    managed.path.display()
                )),
            );
            retained_stale.push(managed);
            continue;
        }
        if let Err(error) = unsafe {
            revoke_deny_read_ace_with_fingerprint(&managed.path, psid, &managed.fingerprint)
        } {
            revoke_error_count = revoke_error_count.saturating_add(1);
            retain_preferred_reconciliation_error(
                &mut preferred_revoke_error,
                error.context(format!(
                    "revoke stale deny-read ACL {}",
                    managed.path.display()
                )),
            );
            retained_stale.push(managed);
        }
    }
    let tracked_paths = merge_tracked_paths(&current_managed, &retained_stale);

    if tracked_paths.is_empty() {
        state.principals.remove(principal_sid);
    } else {
        state
            .principals
            .insert(principal_sid.to_string(), tracked_paths);
    }
    state.pending_principals.remove(principal_sid);
    let managed_keys = state
        .principals
        .get(principal_sid)
        .into_iter()
        .flatten()
        .map(|managed| lexical_path_key(&managed.path))
        .collect::<BTreeSet<_>>();
    let retained_legacy = state
        .legacy_unmanaged_principals
        .remove(principal_sid)
        .unwrap_or_default()
        .into_iter()
        .filter(|path| {
            let key = lexical_path_key(path);
            if managed_keys.contains(&key) {
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
    store_state(state_path, &state)?;
    if let Some(error) = preferred_revoke_error {
        let detail = format!("{error:#}");
        return Err(error.context(format!(
            "failed to revoke stale deny-read ACLs: {detail} ({revoke_error_count} failure(s))"
        )));
    }
    Ok(application.enforced_paths)
}

fn retain_preferred_reconciliation_error(
    retained: &mut Option<anyhow::Error>,
    candidate: anyhow::Error,
) {
    let candidate_is_typed_acl = candidate
        .chain()
        .any(|cause| cause.is::<crate::acl::WindowsAclError>());
    let retained_is_typed_acl = retained.as_ref().is_some_and(|error| {
        error
            .chain()
            .any(|cause| cause.is::<crate::acl::WindowsAclError>())
    });
    if retained.is_none() || (candidate_is_typed_acl && !retained_is_typed_acl) {
        *retained = Some(candidate);
    }
}

fn merge_tracked_paths(
    current: &[ManagedDenyReadAcl],
    retained_stale: &[ManagedDenyReadAcl],
) -> Vec<ManagedDenyReadAcl> {
    let mut keys = BTreeSet::new();
    let mut merged = Vec::new();
    for managed in current.iter().chain(retained_stale) {
        if keys.insert(lexical_path_key(&managed.path)) {
            merged.push(managed.clone());
        }
    }
    merged.sort_by_key(|managed| lexical_path_key(&managed.path));
    merged
}

fn load_state(path: &Path) -> Result<PersistentDenyReadAclState> {
    match std::fs::read(path) {
        Ok(bytes) => {
            let value: serde_json::Value = serde_json::from_slice(&bytes)
                .with_context(|| format!("parse deny-read ACL state {}", path.display()))?;
            match value.get("version").and_then(serde_json::Value::as_u64) {
                Some(version) if version == u64::from(DENY_READ_ACL_STATE_VERSION) => {
                    let state: PersistentDenyReadAclState = serde_json::from_value(value)
                        .with_context(|| format!("parse deny-read ACL state {}", path.display()))?;
                    validate_state_paths(&state).with_context(|| {
                        format!("validate deny-read ACL state paths {}", path.display())
                    })?;
                    validate_state(&state).with_context(|| {
                        format!("validate deny-read ACL state {}", path.display())
                    })?;
                    Ok(state)
                }
                Some(3) => {
                    let previous: VersionThreePersistentDenyReadAclState =
                        serde_json::from_value(value).with_context(|| {
                            format!("parse version 3 deny-read ACL state {}", path.display())
                        })?;
                    if previous.version != 3 {
                        anyhow::bail!(
                            "invalid version 3 deny-read ACL state in {}",
                            path.display()
                        );
                    }
                    let state = PersistentDenyReadAclState {
                        principals: previous.principals,
                        legacy_unmanaged_principals: previous.legacy_unmanaged_principals,
                        active_runner_leases: previous.active_runner_leases,
                        ..PersistentDenyReadAclState::default()
                    };
                    validate_state_paths(&state).with_context(|| {
                        format!(
                            "validate version 3 deny-read ACL state paths {}",
                            path.display()
                        )
                    })?;
                    validate_state(&state).with_context(|| {
                        format!("validate version 3 deny-read ACL state {}", path.display())
                    })?;
                    Ok(state)
                }
                Some(2) => {
                    let previous: VersionTwoPersistentDenyReadAclState =
                        serde_json::from_value(value).with_context(|| {
                            format!("parse version 2 deny-read ACL state {}", path.display())
                        })?;
                    if previous.version != 2 {
                        anyhow::bail!(
                            "invalid version 2 deny-read ACL state in {}",
                            path.display()
                        );
                    }
                    let state = PersistentDenyReadAclState {
                        legacy_unmanaged_principals: merge_legacy_principals(
                            previous.principals,
                            previous.legacy_unmanaged_principals,
                        ),
                        ..PersistentDenyReadAclState::default()
                    };
                    validate_state_paths(&state).with_context(|| {
                        format!(
                            "validate version 2 deny-read ACL state paths {}",
                            path.display()
                        )
                    })?;
                    validate_state(&state).with_context(|| {
                        format!("validate version 2 deny-read ACL state {}", path.display())
                    })?;
                    Ok(state)
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
                    let state = PersistentDenyReadAclState {
                        legacy_unmanaged_principals: legacy.principals,
                        ..PersistentDenyReadAclState::default()
                    };
                    validate_state_paths(&state).with_context(|| {
                        format!(
                            "validate legacy deny-read ACL state paths {}",
                            path.display()
                        )
                    })?;
                    validate_state(&state).with_context(|| {
                        format!("validate legacy deny-read ACL state {}", path.display())
                    })?;
                    Ok(state)
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

fn merge_legacy_principals(
    first: BTreeMap<String, Vec<PathBuf>>,
    second: BTreeMap<String, Vec<PathBuf>>,
) -> BTreeMap<String, Vec<PathBuf>> {
    let mut merged = first;
    for (principal, paths) in second {
        let current = merged.entry(principal).or_default();
        current.extend(paths);
    }
    for current in merged.values_mut() {
        current.sort();
        current.dedup();
    }
    merged
}

fn validate_state_paths(state: &PersistentDenyReadAclState) -> Result<()> {
    for path in state
        .principals
        .values()
        .flatten()
        .chain(state.pending_principals.values().flatten())
        .map(|managed| &managed.path)
        .chain(state.legacy_unmanaged_principals.values().flatten())
    {
        ensure_case_insensitive_acl_path(path)?;
    }
    Ok(())
}

fn validate_state(state: &PersistentDenyReadAclState) -> Result<()> {
    if state.version != DENY_READ_ACL_STATE_VERSION {
        anyhow::bail!("deny-read ACL state version does not match current schema");
    }
    if state.active_runner_leases.len() > MAX_ACTIVE_RUNNER_LEASES {
        anyhow::bail!(
            "deny-read runner lease limit exceeded: {}",
            state.active_runner_leases.len()
        );
    }
    for lease_name in &state.active_runner_leases {
        validate_runner_lease_name(lease_name)?;
    }
    for entries in state
        .principals
        .values()
        .chain(state.pending_principals.values())
    {
        let mut keys = BTreeSet::new();
        for managed in entries {
            if !managed.path.is_absolute()
                || managed.fingerprint.is_empty()
                || !keys.insert(lexical_path_key(&managed.path))
            {
                anyhow::bail!("invalid managed deny-read ACL state entry");
            }
        }
    }
    for path in state.legacy_unmanaged_principals.values().flatten() {
        if !path.is_absolute() {
            anyhow::bail!("invalid legacy deny-read ACL state entry");
        }
    }
    Ok(())
}

fn store_state(path: &Path, state: &PersistentDenyReadAclState) -> Result<()> {
    validate_state_paths(state)?;
    validate_state(state)?;
    let bytes = serde_json::to_vec_pretty(state).context("serialize deny-read ACL state")?;
    atomic_store(path, &bytes)
}

#[cfg(test)]
mod tests {
    use super::DENY_READ_ACL_STATE_FILE;
    use super::ManagedDenyReadAcl;
    use super::PersistentDenyReadAclState;
    use super::load_state;
    use super::lock_state;
    use super::merge_tracked_paths;
    use super::reconcile_runner_leases;
    use super::register_runner_lease;
    use super::retain_preferred_reconciliation_error;
    use super::sandbox_dir;
    use super::state_mutex_name;
    use super::store_state;
    use super::sync_persistent_deny_read_acls;
    use super::try_lock_deny_read_execution;
    use crate::acl::AclOperation;
    use crate::acl::WindowsAclError;
    use crate::acl::add_deny_read_ace;
    use crate::acl::add_deny_write_ace;
    use crate::acl::dacl_has_read_deny_for_sid;
    use crate::acl::deny_read_acl_fingerprint;
    use crate::acl::fetch_dacl_handle;
    use crate::acl::revoke_deny_read_ace;
    use crate::path_normalization::lexical_path_key;
    use crate::path_safety::CaseSensitivityTestOutcome;
    use crate::path_safety::ProtectedMetadataError;
    use crate::path_safety::override_case_sensitivity_for_test;
    use crate::token::LocalSid;
    use std::os::windows::process::CommandExt;
    use std::path::Path;
    use std::path::PathBuf;
    use std::time::Duration;
    use windows_sys::Win32::Foundation::HLOCAL;
    use windows_sys::Win32::Foundation::LocalFree;

    fn create_acl_target(path: &Path, is_directory: bool) {
        if is_directory {
            std::fs::create_dir(path).expect("create ACL target directory");
        } else {
            std::fs::write(path, b"protected").expect("create ACL target file");
        }
    }

    const CHILD_ENV: &str = "SINGULARITY_DENY_READ_STATE_CHILD";
    const EXECUTION_CHILD_ENV: &str = "SINGULARITY_DENY_READ_EXECUTION_CHILD";
    const EXECUTION_ACQUIRED_ENV: &str = "SINGULARITY_DENY_READ_EXECUTION_ACQUIRED";

    #[test]
    fn reconciliation_preserves_typed_acl_failure_over_generic_errors() {
        let mut retained = None;
        retain_preferred_reconciliation_error(
            &mut retained,
            anyhow::anyhow!("generic stale-state mismatch"),
        );
        retain_preferred_reconciliation_error(
            &mut retained,
            anyhow::Error::new(WindowsAclError {
                operation: AclOperation::OpenTarget,
                code: 5,
            })
            .context("reopen stale target"),
        );

        let retained = retained.expect("preferred reconciliation failure");
        assert!(retained.chain().any(|cause| cause.is::<WindowsAclError>()));
    }
    const HOME_ENV: &str = "SINGULARITY_DENY_READ_STATE_HOME";
    const PATH_ENV: &str = "SINGULARITY_DENY_READ_STATE_PATH";

    fn managed(path: PathBuf) -> ManagedDenyReadAcl {
        serde_json::from_value(serde_json::json!({
            "path": path,
            "fingerprint": {
                "entries": [{
                    "flags": 0,
                    "mask": 1
                }]
            }
        }))
        .expect("deserialize managed deny-read fixture")
    }

    #[test]
    fn state_mutex_uses_global_namespace_for_cross_session_state() {
        let name = state_mutex_name(Path::new(r"C:\sandbox\.sandbox\deny_read_acl_state.json"))
            .expect("validated state mutex name");
        assert!(name.starts_with(r"Global\"), "mutex name was {name}");
    }

    #[test]
    fn state_mutex_rejects_case_sensitive_identity_before_lowercase_keying() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let _case_sensitive = override_case_sensitivity_for_test(
            tmp.path(),
            CaseSensitivityTestOutcome::CaseSensitive,
        );
        let state_path = tmp.path().join(DENY_READ_ACL_STATE_FILE);

        let error = match lock_state(&state_path) {
            Ok(_) => panic!("case-sensitive state identity must fail before mutex naming"),
            Err(error) => error,
        };

        assert_eq!(
            error.downcast_ref::<ProtectedMetadataError>(),
            Some(&ProtectedMetadataError::CaseSensitiveDirectoryUnsupported {
                path: tmp.path().to_path_buf(),
            })
        );
    }

    #[test]
    fn tracked_paths_include_only_current_and_unreconciled_entries() {
        let current = vec![managed(PathBuf::from(r"C:\workspace\.agents"))];
        let retained_stale = vec![managed(PathBuf::from(r"C:\workspace\.git"))];
        let merged = merge_tracked_paths(&current, &retained_stale);
        assert_eq!(
            merged
                .into_iter()
                .map(|managed| managed.path)
                .collect::<Vec<_>>(),
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
            state_mutex_name(&ordinary).expect("ordinary mutex name"),
            state_mutex_name(&through_alias).expect("alias mutex name")
        );
        assert_eq!(
            state_mutex_name(&ordinary).expect("ordinary mutex name"),
            state_mutex_name(&verbatim).expect("verbatim mutex name")
        );
    }

    #[test]
    fn state_lock_keeps_mutex_and_io_on_one_canonical_identity() {
        let temp = tempfile::tempdir().expect("tempdir");
        let first_home = temp.path().join("first-home");
        let second_home = temp.path().join("second-home");
        let alias = temp.path().join("home-alias");
        std::fs::create_dir_all(sandbox_dir(&first_home)).expect("create first state directory");
        std::fs::create_dir_all(sandbox_dir(&second_home)).expect("create second state directory");
        create_junction(&alias, &first_home);
        let alias_state = sandbox_dir(&alias).join(DENY_READ_ACL_STATE_FILE);
        let first_state = sandbox_dir(&first_home).join(DENY_READ_ACL_STATE_FILE);
        let second_state = sandbox_dir(&second_home).join(DENY_READ_ACL_STATE_FILE);

        let lock = lock_state(&alias_state).expect("lock first canonical state identity");
        std::fs::remove_dir(&alias).expect("remove first junction");
        create_junction(&alias, &second_home);
        store_state(lock.path(), &PersistentDenyReadAclState::default())
            .expect("store through locked canonical identity");

        assert!(
            first_state.exists(),
            "locked identity must receive state I/O"
        );
        assert!(
            !second_state.exists(),
            "retargeted alias must not redirect state I/O"
        );
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
    fn abandoned_registration_is_pruned_before_a_late_runner_can_spawn() {
        let temp = tempfile::tempdir().expect("tempdir");
        let sandbox_home = temp.path().join("singularity-home");
        let username = std::env::var("USERNAME").expect("current Windows username");
        let registration =
            register_runner_lease(&sandbox_home, &username).expect("register runner lease");
        let lease_name = registration.name().to_string();
        drop(registration);

        assert!(reconcile_runner_leases(&sandbox_home, 1_000).expect("reconcile abandoned lease"));
        assert!(
            super::acquire_registered_runner_lease(&lease_name).is_err(),
            "a late runner must not spawn after its registration was reconciled"
        );
        let state = load_state(&sandbox_dir(&sandbox_home).join(DENY_READ_ACL_STATE_FILE))
            .expect("load reconciled lease state");
        assert!(state.active_runner_leases.is_empty());
    }

    #[test]
    fn active_runner_lease_does_not_depend_on_parent_state_path() {
        let temp = tempfile::tempdir().expect("tempdir");
        let sandbox_home = temp.path().join("singularity-home");
        let username = std::env::var("USERNAME").expect("current Windows username");
        let registration =
            register_runner_lease(&sandbox_home, &username).expect("register runner lease");
        let lease_name = registration.name().to_string();

        std::fs::remove_dir_all(&sandbox_home).expect("remove parent-only state directory");

        let lease = super::acquire_registered_runner_lease(&lease_name)
            .expect("runner acquires the parent-created lease without reading parent state");
        lease.release();
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
            state
                .principals
                .get("S-1-5-21-1-2-3-4")
                .map(|managed| managed.iter().map(|entry| entry.path.clone()).collect()),
            Some(vec![current.clone()])
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
    fn pending_applied_file_and_directory_ownership_is_promoted_after_interrupted_commit() {
        let temp = tempfile::tempdir().expect("tempdir");
        let principal = "S-1-5-21-1-2-3-4";
        let sid = LocalSid::from_string(principal).expect("test SID");
        for (label, is_directory) in [("file", false), ("directory", true)] {
            let sandbox_home = temp.path().join(format!("{label}-sandbox-home"));
            let protected = temp.path().join(format!("{label}-protected"));
            std::fs::create_dir_all(&sandbox_home).expect("create sandbox home");
            create_acl_target(&protected, is_directory);
            assert!(
                unsafe { add_deny_read_ace(&protected, sid.as_ptr()) }
                    .expect("seed interrupted runtime deny")
            );
            let fingerprint = unsafe { deny_read_acl_fingerprint(&protected, sid.as_ptr()) }
                .expect("fingerprint interrupted deny");
            let state_path = sandbox_dir(&sandbox_home).join(DENY_READ_ACL_STATE_FILE);
            let mut state = PersistentDenyReadAclState::default();
            state.pending_principals.insert(
                principal.to_string(),
                vec![ManagedDenyReadAcl {
                    path: protected.clone(),
                    fingerprint,
                }],
            );
            store_state(&state_path, &state).expect("store interrupted pending state");

            unsafe {
                sync_persistent_deny_read_acls(
                    &sandbox_home,
                    principal,
                    std::slice::from_ref(&protected),
                    sid.as_ptr(),
                )
            }
            .expect("recover pending ownership");

            let recovered = load_state(&state_path).expect("load recovered state");
            assert!(recovered.pending_principals.is_empty());
            assert_eq!(
                recovered
                    .principals
                    .get(principal)
                    .map(|entries| entries.iter().map(|entry| entry.path.clone()).collect()),
                Some(vec![protected.clone()])
            );
            unsafe {
                revoke_deny_read_ace(&protected, sid.as_ptr()).expect("restore protected ACL")
            };
        }
    }

    #[test]
    fn runtime_owned_file_and_directory_fingerprints_revoke_cleanly() {
        let temp = tempfile::tempdir().expect("tempdir");
        let principal = "S-1-5-21-1-2-3-4";
        let sid = LocalSid::from_string(principal).expect("test SID");
        for (label, is_directory) in [("file", false), ("directory", true)] {
            let sandbox_home = temp.path().join(format!("{label}-sandbox-home"));
            let protected = temp.path().join(format!("{label}-protected"));
            std::fs::create_dir_all(&sandbox_home).expect("create sandbox home");
            create_acl_target(&protected, is_directory);

            unsafe {
                sync_persistent_deny_read_acls(
                    &sandbox_home,
                    principal,
                    std::slice::from_ref(&protected),
                    sid.as_ptr(),
                )
            }
            .expect("apply managed deny-read ACL");

            let state_path = sandbox_dir(&sandbox_home).join(DENY_READ_ACL_STATE_FILE);
            let state = load_state(&state_path).expect("load managed state");
            let managed = state
                .principals
                .get(principal)
                .and_then(|entries| entries.first())
                .expect("runtime-owned fingerprint");
            assert_eq!(
                managed.fingerprint,
                unsafe { deny_read_acl_fingerprint(&protected, sid.as_ptr()) }
                    .expect("read applied fingerprint"),
                "{label}"
            );

            unsafe { sync_persistent_deny_read_acls(&sandbox_home, principal, &[], sid.as_ptr()) }
                .expect("revoke managed deny-read ACL");

            let state = load_state(&state_path).expect("load revoked state");
            assert!(!state.principals.contains_key(principal), "{label}");
            let (dacl, security_descriptor) =
                unsafe { fetch_dacl_handle(&protected).expect("read revoked DACL") };
            assert!(
                !unsafe { dacl_has_read_deny_for_sid(dacl, sid.as_ptr()) },
                "{label}"
            );
            unsafe {
                LocalFree(security_descriptor as HLOCAL);
            }
        }
    }

    #[test]
    fn pending_unapplied_ownership_is_dropped_after_interrupted_commit() {
        let temp = tempfile::tempdir().expect("tempdir");
        let sandbox_home = temp.path().join("sandbox-home");
        let protected = temp.path().join("protected");
        std::fs::create_dir_all(&sandbox_home).expect("create sandbox home");
        std::fs::create_dir(&protected).expect("create protected path");
        let principal = "S-1-5-21-1-2-3-4";
        let sid = LocalSid::from_string(principal).expect("test SID");
        assert!(
            unsafe { add_deny_read_ace(&protected, sid.as_ptr()) }
                .expect("seed expected pending deny")
        );
        let expected = unsafe { deny_read_acl_fingerprint(&protected, sid.as_ptr()) }
            .expect("fingerprint expected pending deny");
        unsafe { revoke_deny_read_ace(&protected, sid.as_ptr()).expect("remove unapplied deny") };
        let state_path = sandbox_dir(&sandbox_home).join(DENY_READ_ACL_STATE_FILE);
        let mut state = PersistentDenyReadAclState::default();
        state.pending_principals.insert(
            principal.to_string(),
            vec![ManagedDenyReadAcl {
                path: protected,
                fingerprint: expected,
            }],
        );
        store_state(&state_path, &state).expect("store unapplied pending state");

        unsafe { sync_persistent_deny_read_acls(&sandbox_home, principal, &[], sid.as_ptr()) }
            .expect("drop unapplied pending ownership");

        let recovered = load_state(&state_path).expect("load recovered state");
        assert!(recovered.pending_principals.is_empty());
        assert!(!recovered.principals.contains_key(principal));
    }

    #[test]
    fn changed_pending_acl_fingerprint_fails_closed_and_retains_journal() {
        let temp = tempfile::tempdir().expect("tempdir");
        let sandbox_home = temp.path().join("sandbox-home");
        let protected = temp.path().join("protected");
        std::fs::create_dir_all(&sandbox_home).expect("create sandbox home");
        std::fs::create_dir(&protected).expect("create protected path");
        let principal = "S-1-5-21-1-2-3-4";
        let sid = LocalSid::from_string(principal).expect("test SID");
        assert!(
            unsafe { add_deny_read_ace(&protected, sid.as_ptr()) }
                .expect("seed expected pending deny")
        );
        let expected = unsafe { deny_read_acl_fingerprint(&protected, sid.as_ptr()) }
            .expect("fingerprint expected pending deny");
        assert!(
            unsafe { add_deny_write_ace(&protected, sid.as_ptr()) }
                .expect("change pending ACL fingerprint")
        );
        let state_path = sandbox_dir(&sandbox_home).join(DENY_READ_ACL_STATE_FILE);
        let mut state = PersistentDenyReadAclState::default();
        state.pending_principals.insert(
            principal.to_string(),
            vec![ManagedDenyReadAcl {
                path: protected.clone(),
                fingerprint: expected,
            }],
        );
        store_state(&state_path, &state).expect("store changed pending state");

        let error =
            unsafe { sync_persistent_deny_read_acls(&sandbox_home, principal, &[], sid.as_ptr()) }
                .expect_err("changed pending fingerprint must fail closed");

        assert!(
            error
                .to_string()
                .contains("pending deny-read ACL fingerprint changed"),
            "{error:#}"
        );
        let retained = load_state(&state_path).expect("load retained pending state");
        assert_eq!(
            retained
                .pending_principals
                .get(principal)
                .map(|entries| entries.iter().map(|entry| entry.path.clone()).collect()),
            Some(vec![protected.clone()])
        );
        unsafe {
            revoke_deny_read_ace(&protected, sid.as_ptr()).expect("restore pending read ACE")
        };
    }

    #[test]
    fn already_revoked_stale_ownership_is_removed_after_interrupted_commit() {
        let temp = tempfile::tempdir().expect("tempdir");
        let sandbox_home = temp.path().join("sandbox-home");
        let protected = temp.path().join("protected");
        std::fs::create_dir_all(&sandbox_home).expect("create sandbox home");
        std::fs::create_dir(&protected).expect("create protected path");
        let principal = "S-1-5-21-1-2-3-4";
        let sid = LocalSid::from_string(principal).expect("test SID");
        unsafe {
            sync_persistent_deny_read_acls(
                &sandbox_home,
                principal,
                std::slice::from_ref(&protected),
                sid.as_ptr(),
            )
        }
        .expect("apply managed deny");
        unsafe {
            revoke_deny_read_ace(&protected, sid.as_ptr()).expect("simulate completed revoke")
        };

        unsafe { sync_persistent_deny_read_acls(&sandbox_home, principal, &[], sid.as_ptr()) }
            .expect("recover interrupted revoke commit");

        let state_path = sandbox_dir(&sandbox_home).join(DENY_READ_ACL_STATE_FILE);
        let recovered = load_state(&state_path).expect("load recovered state");
        assert!(!recovered.principals.contains_key(principal));
    }

    #[test]
    fn changed_managed_acl_fingerprint_is_retained_and_revoke_fails_closed() {
        let temp = tempfile::tempdir().expect("tempdir");
        let sandbox_home = temp.path().join("singularity-home");
        let protected = temp.path().join("protected");
        std::fs::create_dir_all(&sandbox_home).expect("create sandbox home");
        std::fs::create_dir(&protected).expect("create protected path");
        let principal = "S-1-5-21-1-2-3-4";
        let sid = LocalSid::from_string(principal).expect("test SID");

        unsafe {
            sync_persistent_deny_read_acls(
                &sandbox_home,
                principal,
                std::slice::from_ref(&protected),
                sid.as_ptr(),
            )
        }
        .expect("apply managed deny-read ACL");
        assert!(
            unsafe { add_deny_write_ace(&protected, sid.as_ptr()) }
                .expect("add concurrent deny-write ACE"),
            "concurrent ACL fixture must change the SID fingerprint"
        );

        let error =
            unsafe { sync_persistent_deny_read_acls(&sandbox_home, principal, &[], sid.as_ptr()) }
                .expect_err("changed ACL fingerprint must block stale revoke");
        assert!(
            error.to_string().contains("ownership fingerprint changed"),
            "{error:#}"
        );
        let state_path = sandbox_dir(&sandbox_home).join(DENY_READ_ACL_STATE_FILE);
        let state = load_state(&state_path).expect("load retained managed state");
        assert_eq!(
            state
                .principals
                .get(principal)
                .map(|entries| entries.iter().map(|entry| entry.path.clone()).collect()),
            Some(vec![protected.clone()])
        );
        let (dacl, security_descriptor) =
            unsafe { fetch_dacl_handle(&protected).expect("read retained DACL") };
        assert!(unsafe { dacl_has_read_deny_for_sid(dacl, sid.as_ptr()) });
        unsafe {
            LocalFree(security_descriptor as HLOCAL);
            revoke_deny_read_ace(&protected, sid.as_ptr()).expect("restore managed read ACE");
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
    fn version_two_managed_paths_migrate_to_unmanaged_ownership() {
        let temp = tempfile::tempdir().expect("tempdir");
        let sandbox_home = temp.path().join("singularity-home");
        let protected = temp.path().join("version-two-protected");
        std::fs::create_dir_all(&protected).expect("create protected path");
        let principal = "S-1-5-21-1-2-3-4";
        let sid = LocalSid::from_string(principal).expect("test SID");
        assert!(
            unsafe { add_deny_read_ace(&protected, sid.as_ptr()) }
                .expect("seed version two exact deny")
        );
        let state_path = sandbox_dir(&sandbox_home).join(DENY_READ_ACL_STATE_FILE);
        std::fs::create_dir_all(state_path.parent().expect("state parent"))
            .expect("create state directory");
        std::fs::write(
            &state_path,
            serde_json::to_vec(&serde_json::json!({
                "version": 2,
                "principals": { principal: [protected.clone()] },
                "legacy_unmanaged_principals": {}
            }))
            .expect("serialize version two state"),
        )
        .expect("write version two state");

        unsafe { sync_persistent_deny_read_acls(&sandbox_home, principal, &[], sid.as_ptr()) }
            .expect("migrate version two state");
        let state = load_state(&state_path).expect("load migrated version two state");
        assert!(!state.principals.contains_key(principal));
        assert_eq!(
            state.legacy_unmanaged_principals.get(principal),
            Some(&vec![protected.clone()])
        );
        let (dacl, security_descriptor) =
            unsafe { fetch_dacl_handle(&protected).expect("read retained version two DACL") };
        assert!(unsafe { dacl_has_read_deny_for_sid(dacl, sid.as_ptr()) });
        unsafe {
            LocalFree(security_descriptor as HLOCAL);
            revoke_deny_read_ace(&protected, sid.as_ptr()).expect("restore version two test ACE");
        }
    }

    #[test]
    fn version_three_fingerprinted_ownership_migrates_without_loss() {
        let temp = tempfile::tempdir().expect("tempdir");
        let sandbox_home = temp.path().join("singularity-home");
        let protected = temp.path().join("version-three-protected");
        let principal = "S-1-5-21-1-2-3-4";
        let state_path = sandbox_dir(&sandbox_home).join(DENY_READ_ACL_STATE_FILE);
        std::fs::create_dir_all(state_path.parent().expect("state parent"))
            .expect("create state directory");
        std::fs::write(
            &state_path,
            serde_json::to_vec(&serde_json::json!({
                "version": 3,
                "principals": {
                    principal: [{
                        "path": protected.clone(),
                        "fingerprint": {
                            "entries": [{"flags": 0, "mask": 1}]
                        }
                    }]
                },
                "legacy_unmanaged_principals": {},
                "active_runner_leases": []
            }))
            .expect("serialize version three state"),
        )
        .expect("write version three state");

        let state = load_state(&state_path).expect("load migrated version three state");

        assert_eq!(state.version, super::DENY_READ_ACL_STATE_VERSION);
        assert_eq!(
            state
                .principals
                .get(principal)
                .map(|entries| entries.iter().map(|entry| entry.path.clone()).collect()),
            Some(vec![protected])
        );
        assert!(state.pending_principals.is_empty());
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
        let active_key = lexical_path_key(&paths[0].path);
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
