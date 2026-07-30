mod firewall;

use anyhow::Context;
use anyhow::Result;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::Deserialize;
use serde::Serialize;
use singularity_windows_sandbox::AclOperation;
use singularity_windows_sandbox::ReadAclMutexError;
use singularity_windows_sandbox::ReadAclMutexState;
use singularity_windows_sandbox::SETUP_VERSION;
use singularity_windows_sandbox::SetupErrorCode;
use singularity_windows_sandbox::SetupErrorReport;
use singularity_windows_sandbox::SetupFailure;
use singularity_windows_sandbox::TrustedWorkspaceSetupPin;
use singularity_windows_sandbox::WindowsAclError;
use singularity_windows_sandbox::acquire_read_acl_mutex;
use singularity_windows_sandbox::add_deny_write_ace;
use singularity_windows_sandbox::add_deny_write_ace_to_handle;
use singularity_windows_sandbox::canonicalize_path;
use singularity_windows_sandbox::convert_string_sid_to_sid;
use singularity_windows_sandbox::duplicate_setup_root_handle;
use singularity_windows_sandbox::ensure_allow_mask_aces_with_inheritance;
use singularity_windows_sandbox::ensure_allow_mask_aces_with_inheritance_to_handle;
use singularity_windows_sandbox::ensure_allow_write_aces;
use singularity_windows_sandbox::ensure_allow_write_aces_to_handle;
use singularity_windows_sandbox::ensure_case_insensitive_acl_path;
use singularity_windows_sandbox::ensure_missing_protected_path_materialized;
use singularity_windows_sandbox::existing_public_certificate_only_pem;
use singularity_windows_sandbox::extract_setup_failure;
use singularity_windows_sandbox::handle_mask_allows;
use singularity_windows_sandbox::hide_newly_created_users;
use singularity_windows_sandbox::install_wfp_filters;
use singularity_windows_sandbox::is_command_cwd_root;
use singularity_windows_sandbox::log_note;
use singularity_windows_sandbox::log_writer;
use singularity_windows_sandbox::open_pinned_workspace_path;
use singularity_windows_sandbox::path_mask_allows;
use singularity_windows_sandbox::plan_deny_read_acl_paths;
use singularity_windows_sandbox::probe_read_acl_mutex;
use singularity_windows_sandbox::product_identity::SANDBOX_HOME_ENV;
use singularity_windows_sandbox::revoke_deny_write_ace_to_handle;
use singularity_windows_sandbox::sandbox_bin_dir;
use singularity_windows_sandbox::sandbox_dir;
use singularity_windows_sandbox::sandbox_secrets_dir;
use singularity_windows_sandbox::set_dacl_for_path;
use singularity_windows_sandbox::string_from_sid_bytes;
use singularity_windows_sandbox::sync_persistent_deny_read_acls_with_pinned_root;
use singularity_windows_sandbox::to_wide;
use singularity_windows_sandbox::workspace_write_cap_sid_for_root;
use singularity_windows_sandbox::workspace_write_root_overlaps_path;
use singularity_windows_sandbox::write_setup_error_report;
use std::collections::HashSet;
use std::ffi::OsStr;
use std::ffi::c_void;
use std::fs::File;
use std::io::Write;
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;
use std::sync::mpsc;
use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::Foundation::HLOCAL;
use windows_sys::Win32::Foundation::LocalFree;
use windows_sys::Win32::Security::ACL;
use windows_sys::Win32::Security::Authorization::ConvertStringSidToSidW;
use windows_sys::Win32::Security::Authorization::EXPLICIT_ACCESS_W;
use windows_sys::Win32::Security::Authorization::GRANT_ACCESS;
use windows_sys::Win32::Security::Authorization::SetEntriesInAclW;
use windows_sys::Win32::Security::Authorization::TRUSTEE_IS_SID;
use windows_sys::Win32::Security::Authorization::TRUSTEE_W;
use windows_sys::Win32::Security::CONTAINER_INHERIT_ACE;
use windows_sys::Win32::Security::OBJECT_INHERIT_ACE;
use windows_sys::Win32::Storage::FileSystem::DELETE;
use windows_sys::Win32::Storage::FileSystem::FILE_DELETE_CHILD;
use windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_EXECUTE;
use windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_READ;
use windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_WRITE;

const DENY_ACCESS: i32 = 3;
const WRITE_ROOT_ALLOW_MASK: u32 =
    FILE_GENERIC_READ | FILE_GENERIC_WRITE | FILE_GENERIC_EXECUTE | DELETE;

fn acl_error_priority(error: &anyhow::Error) -> u8 {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<WindowsAclError>())
        .map_or(0, |error| {
            if error.code == 5
                && matches!(
                    error.operation,
                    AclOperation::OpenTarget
                        | AclOperation::GetSecurityInfo
                        | AclOperation::SetSecurityInfo
                )
            {
                2
            } else {
                1
            }
        })
}

fn retain_preferred_acl_error(retained: &mut Option<anyhow::Error>, candidate: anyhow::Error) {
    if acl_error_priority(&candidate) > retained.as_ref().map_or(0, acl_error_priority) {
        *retained = Some(candidate);
    }
}

fn acl_open_failure(error: anyhow::Error) -> anyhow::Error {
    let Some(code) = error
        .chain()
        .find_map(|cause| cause.downcast_ref::<std::io::Error>())
        .and_then(std::io::Error::raw_os_error)
        .map(|code| code as u32)
    else {
        return error;
    };
    anyhow::Error::new(
        SetupFailure::new(
            SetupErrorCode::HelperAclRefreshFailed,
            "trusted ACL target open failed",
        )
        .with_acl_error(WindowsAclError {
            operation: AclOperation::OpenTarget,
            code,
        }),
    )
    .context(error)
}

mod sandbox_users;
mod setup_runtime_bin;
use sandbox_users::commit_setup_marker;
use sandbox_users::prepare_setup_marker;
use sandbox_users::provision_sandbox_users;
use sandbox_users::resolve_sandbox_users_group_sid;
use sandbox_users::resolve_sid;
use sandbox_users::sid_bytes_to_psid;

#[derive(Debug, Clone, Deserialize, Serialize)]
struct Payload {
    version: u32,
    offline_username: String,
    online_username: String,
    sandbox_home: PathBuf,
    command_cwd: PathBuf,
    read_roots: Vec<PathBuf>,
    write_roots: Vec<PathBuf>,
    #[serde(default)]
    deny_read_paths: Vec<PathBuf>,
    #[serde(default)]
    deny_write_paths: Vec<PathBuf>,
    #[serde(default)]
    revoke_deny_write_paths: Vec<PathBuf>,
    proxy_ports: Vec<u16>,
    #[serde(default)]
    allow_local_binding: bool,
    #[serde(default)]
    real_user: String,
    #[serde(default)]
    mode: SetupMode,
    #[serde(default)]
    refresh_only: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    trusted_workspace_root: Option<TrustedWorkspaceSetupPin>,
}

struct PinnedWorkspaceRoot {
    path: PathBuf,
    handle: File,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
enum SetupMode {
    #[default]
    Full,
    ProvisionOnly,
    ReadAclsOnly,
}

/// Owns one SID allocated by the Win32 SID conversion helpers across all early returns.
struct LocalSidGuard(*mut c_void);

impl LocalSidGuard {
    fn as_ptr(&self) -> *mut c_void {
        self.0
    }
}

impl Drop for LocalSidGuard {
    fn drop(&mut self) {
        unsafe {
            if !self.0.is_null() {
                LocalFree(self.0 as HLOCAL);
            }
        }
    }
}

fn log_line(log: &mut dyn Write, msg: &str) -> Result<()> {
    let ts = chrono::Utc::now().to_rfc3339();
    writeln!(log, "[{ts}] {msg}").map_err(|err| {
        anyhow::Error::new(SetupFailure::new(
            SetupErrorCode::HelperLogFailed,
            format!("failed to write setup log line: {err}"),
        ))
    })?;
    Ok(())
}

fn workspace_write_cap_sids_for_path(
    sandbox_home: &Path,
    command_cwd: &Path,
    write_roots: &[PathBuf],
    path: &Path,
) -> Result<Vec<String>> {
    let mut sid_strs = Vec::new();
    for root in write_roots {
        if workspace_write_root_overlaps_path(root, path) {
            sid_strs.push(workspace_write_cap_sid_for_root(
                sandbox_home,
                command_cwd,
                root,
            )?);
        }
    }
    if sid_strs.is_empty() {
        if write_roots.is_empty() {
            sid_strs.push(workspace_write_cap_sid_for_root(
                sandbox_home,
                command_cwd,
                command_cwd,
            )?);
        } else {
            for root in write_roots {
                sid_strs.push(workspace_write_cap_sid_for_root(
                    sandbox_home,
                    command_cwd,
                    root,
                )?);
            }
        }
    }
    sid_strs.sort();
    sid_strs.dedup();
    Ok(sid_strs)
}

fn write_root_needs_refresh(
    root: &Path,
    psid: *mut c_void,
    pinned_workspace_root: Option<&PinnedWorkspaceRoot>,
) -> Result<bool> {
    if !path_mask_allows_for_path(
        root,
        &[psid],
        WRITE_ROOT_ALLOW_MASK,
        /*require_all_bits*/ true,
        pinned_workspace_root,
    )? {
        return Ok(true);
    }
    path_mask_allows_for_path(
        root,
        &[psid],
        FILE_DELETE_CHILD,
        /*require_all_bits*/ false,
        pinned_workspace_root,
    )
}

fn spawn_read_acl_helper(payload: &Payload, _log: &mut dyn Write) -> Result<()> {
    let mut read_payload = payload.clone();
    read_payload.mode = SetupMode::ReadAclsOnly;
    read_payload.refresh_only = true;
    let payload_json = serde_json::to_vec(&read_payload)?;
    let payload_b64 = BASE64.encode(payload_json);
    let exe = std::env::current_exe().context("locate setup helper")?;
    Command::new(&exe)
        .arg(payload_b64)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(0x08000000) // CREATE_NO_WINDOW
        .spawn()
        .context("spawn read ACL helper")?;
    Ok(())
}

struct ReadAclSubjects<'a> {
    sandbox_group_psid: *mut c_void,
    rx_psids: &'a [*mut c_void],
}

#[allow(clippy::too_many_arguments)]
fn apply_read_acls(
    read_roots: &[PathBuf],
    subjects: &ReadAclSubjects<'_>,
    log: &mut dyn Write,
    refresh_errors: &mut Vec<String>,
    access_mask: u32,
    access_label: &str,
    inheritance: u32,
    pinned_workspace_root: Option<&PinnedWorkspaceRoot>,
) -> Result<()> {
    for root in read_roots {
        if !root.exists() {
            log_line(
                log,
                &format!("{access_label} root {} missing; skipping", root.display()),
            )?;
            continue;
        }
        let builtin_has = read_mask_allows_or_log(
            root,
            subjects.rx_psids,
            /*label*/ None,
            access_mask,
            access_label,
            refresh_errors,
            log,
            pinned_workspace_root,
        )?;
        if builtin_has {
            continue;
        }
        let sandbox_has = read_mask_allows_or_log(
            root,
            &[subjects.sandbox_group_psid],
            Some("sandbox_group"),
            access_mask,
            access_label,
            refresh_errors,
            log,
            pinned_workspace_root,
        )?;
        if sandbox_has {
            continue;
        }
        log_line(
            log,
            &format!(
                "granting {access_label} ACE to {} for sandbox users",
                root.display()
            ),
        )?;
        let result = unsafe {
            ensure_allow_mask_aces_with_inheritance_for_path(
                root,
                &[subjects.sandbox_group_psid],
                access_mask,
                inheritance,
                pinned_workspace_root,
            )
        };
        if let Err(err) = result {
            refresh_errors.push(format!(
                "grant {access_label} ACE failed on {} for sandbox_group: {err}",
                root.display()
            ));
            log_line(
                log,
                &format!(
                    "grant {access_label} ACE failed on {} for sandbox_group: {err}",
                    root.display()
                ),
            )?;
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn read_mask_allows_or_log(
    root: &Path,
    psids: &[*mut c_void],
    label: Option<&str>,
    read_mask: u32,
    access_label: &str,
    refresh_errors: &mut Vec<String>,
    log: &mut dyn Write,
    pinned_workspace_root: Option<&PinnedWorkspaceRoot>,
) -> Result<bool> {
    match path_mask_allows_for_path(
        root,
        psids,
        read_mask,
        /*require_all_bits*/ true,
        pinned_workspace_root,
    ) {
        Ok(has) => Ok(has),
        Err(e) => {
            let label_suffix = label
                .map(|value| format!(" for {value}"))
                .unwrap_or_default();
            refresh_errors.push(format!(
                "{access_label} mask check failed on {}{}: {}",
                root.display(),
                label_suffix,
                e
            ));
            log_line(
                log,
                &format!(
                    "{access_label} mask check failed on {}{}: {}; continuing",
                    root.display(),
                    label_suffix,
                    e
                ),
            )?;
            Ok(false)
        }
    }
}

fn path_mask_allows_for_path(
    path: &Path,
    psids: &[*mut c_void],
    desired_mask: u32,
    require_all_bits: bool,
    pinned_workspace_root: Option<&PinnedWorkspaceRoot>,
) -> Result<bool> {
    if let Some(pinned) = pinned_workspace_root
        && let Some(handle) = open_pinned_workspace_path(
            &pinned.handle,
            &pinned.path,
            path,
            windows_sys::Win32::Storage::FileSystem::READ_CONTROL,
        )?
    {
        return unsafe {
            handle_mask_allows(
                handle.as_raw_handle() as _,
                psids,
                desired_mask,
                require_all_bits,
            )
        };
    }
    path_mask_allows(path, psids, desired_mask, require_all_bits)
}

unsafe fn ensure_allow_mask_aces_with_inheritance_for_path(
    path: &Path,
    psids: &[*mut c_void],
    allow_mask: u32,
    inheritance: u32,
    pinned_workspace_root: Option<&PinnedWorkspaceRoot>,
) -> Result<bool> {
    if let Some(pinned) = pinned_workspace_root
        && let Some(handle) = open_pinned_workspace_path(
            &pinned.handle,
            &pinned.path,
            path,
            windows_sys::Win32::Storage::FileSystem::READ_CONTROL
                | windows_sys::Win32::Storage::FileSystem::WRITE_DAC,
        )?
    {
        return unsafe {
            ensure_allow_mask_aces_with_inheritance_to_handle(
                handle.as_raw_handle() as _,
                psids,
                allow_mask,
                inheritance,
            )
        };
    }
    unsafe { ensure_allow_mask_aces_with_inheritance(path, psids, allow_mask, inheritance) }
}

unsafe fn ensure_allow_write_aces_for_path(
    path: &Path,
    psids: &[*mut c_void],
    pinned_workspace_root: Option<&PinnedWorkspaceRoot>,
) -> Result<bool> {
    if let Some(pinned) = pinned_workspace_root
        && let Some(handle) = open_pinned_workspace_path(
            &pinned.handle,
            &pinned.path,
            path,
            windows_sys::Win32::Storage::FileSystem::READ_CONTROL
                | windows_sys::Win32::Storage::FileSystem::WRITE_DAC,
        )?
    {
        return unsafe { ensure_allow_write_aces_to_handle(handle.as_raw_handle() as _, psids) };
    }
    unsafe { ensure_allow_write_aces(path, psids) }
}

unsafe fn add_deny_write_ace_for_path(
    path: &Path,
    psid: *mut c_void,
    pinned_workspace_root: Option<&PinnedWorkspaceRoot>,
) -> Result<bool> {
    if let Some(pinned) = pinned_workspace_root
        && let Some(handle) = open_pinned_workspace_path(
            &pinned.handle,
            &pinned.path,
            path,
            windows_sys::Win32::Storage::FileSystem::READ_CONTROL
                | windows_sys::Win32::Storage::FileSystem::WRITE_DAC,
        )?
    {
        return unsafe { add_deny_write_ace_to_handle(handle.as_raw_handle() as _, psid) };
    }
    unsafe { add_deny_write_ace(path, psid) }
}

fn lock_sandbox_dir(
    dir: &Path,
    real_user: &str,
    sandbox_group_sid: &[u8],
    sandbox_group_access_mode: i32,
    sandbox_group_mask: u32,
    real_user_mask: u32,
    _log: &mut dyn Write,
) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let system_sid = resolve_sid("SYSTEM")?;
    let admins_sid = resolve_sid("Administrators")?;
    let real_sid = resolve_sid(real_user)?;
    let entries = [
        (
            sandbox_group_sid.to_vec(),
            sandbox_group_mask,
            sandbox_group_access_mode,
        ),
        (
            system_sid,
            FILE_GENERIC_READ | FILE_GENERIC_WRITE | FILE_GENERIC_EXECUTE | DELETE,
            GRANT_ACCESS,
        ),
        (
            admins_sid,
            FILE_GENERIC_READ | FILE_GENERIC_WRITE | FILE_GENERIC_EXECUTE | DELETE,
            GRANT_ACCESS,
        ),
        (real_sid, real_user_mask, GRANT_ACCESS),
    ];
    unsafe {
        let mut eas: Vec<EXPLICIT_ACCESS_W> = Vec::new();
        let mut sids: Vec<*mut c_void> = Vec::new();
        for (sid_bytes, mask, access_mode) in entries.iter().map(|(s, m, a)| (s, *m, *a)) {
            let sid_str = string_from_sid_bytes(sid_bytes).map_err(anyhow::Error::msg)?;
            let sid_w = to_wide(OsStr::new(&sid_str));
            let mut psid: *mut c_void = std::ptr::null_mut();
            if ConvertStringSidToSidW(sid_w.as_ptr(), &mut psid) == 0 {
                return Err(anyhow::anyhow!(
                    "ConvertStringSidToSidW failed: {}",
                    GetLastError()
                ));
            }
            sids.push(psid);
            eas.push(EXPLICIT_ACCESS_W {
                grfAccessPermissions: mask,
                grfAccessMode: access_mode,
                grfInheritance: OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE,
                Trustee: TRUSTEE_W {
                    pMultipleTrustee: std::ptr::null_mut(),
                    MultipleTrusteeOperation: 0,
                    TrusteeForm: TRUSTEE_IS_SID,
                    TrusteeType: TRUSTEE_IS_SID,
                    ptstrName: psid as *mut u16,
                },
            });
        }
        let mut new_dacl: *mut ACL = std::ptr::null_mut();
        let set = SetEntriesInAclW(
            eas.len() as u32,
            eas.as_ptr(),
            std::ptr::null_mut(),
            &mut new_dacl,
        );
        if set != 0 {
            return Err(anyhow::Error::new(WindowsAclError {
                operation: AclOperation::SetEntriesInAcl,
                code: set,
            }));
        }
        let set_result = set_dacl_for_path(dir, new_dacl);
        if !new_dacl.is_null() {
            LocalFree(new_dacl as HLOCAL);
        }
        for sid in sids {
            if !sid.is_null() {
                LocalFree(sid as HLOCAL);
            }
        }
        set_result?;
    }
    Ok(())
}

pub fn main() -> Result<()> {
    let ret = real_main();
    if let Err(e) = &ret {
        // Best-effort: log unexpected top-level errors.
        if let Ok(sandbox_home) = std::env::var(SANDBOX_HOME_ENV) {
            let sbx_dir = sandbox_dir(Path::new(&sandbox_home));
            let _ = std::fs::create_dir_all(&sbx_dir);
            if let Some(mut f) = log_writer(&sbx_dir) {
                let _ = writeln!(
                    f,
                    "[{}] top-level error: {}",
                    chrono::Utc::now().to_rfc3339(),
                    e
                );
            }
        }
    }
    ret
}

fn real_main() -> Result<()> {
    let mut args = std::env::args().collect::<Vec<_>>();
    if args.len() != 2 {
        return Err(anyhow::Error::new(SetupFailure::new(
            SetupErrorCode::HelperRequestArgsFailed,
            "expected payload argument",
        )));
    }
    let payload_b64 = args.remove(1);
    let payload_json = BASE64.decode(payload_b64).map_err(|err| {
        anyhow::Error::new(SetupFailure::new(
            SetupErrorCode::HelperRequestArgsFailed,
            format!("failed to decode payload b64: {err}"),
        ))
    })?;
    let payload: Payload = serde_json::from_slice(&payload_json).map_err(|err| {
        anyhow::Error::new(SetupFailure::new(
            SetupErrorCode::HelperRequestArgsFailed,
            format!("failed to parse payload json: {err}"),
        ))
    })?;
    if payload.version != SETUP_VERSION {
        return Err(anyhow::Error::new(SetupFailure::new(
            SetupErrorCode::HelperRequestArgsFailed,
            format!(
                "setup version mismatch: expected {SETUP_VERSION}, got {}",
                payload.version
            ),
        )));
    }
    let sbx_dir = sandbox_dir(&payload.sandbox_home);
    std::fs::create_dir_all(&sbx_dir).map_err(|err| {
        anyhow::Error::new(SetupFailure::new(
            SetupErrorCode::HelperSandboxDirCreateFailed,
            format!("failed to create sandbox dir {}: {err}", sbx_dir.display()),
        ))
    })?;
    let mut log = log_writer(&sbx_dir).ok_or_else(|| {
        anyhow::Error::new(SetupFailure::new(
            SetupErrorCode::HelperLogFailed,
            format!("open log in {} failed", sbx_dir.display()),
        ))
    })?;
    let result = (|| {
        let pinned_workspace_root = payload
            .trusted_workspace_root
            .as_ref()
            .map(|pin| {
                duplicate_setup_root_handle(pin).map(|handle| PinnedWorkspaceRoot {
                    path: pin.root_path.clone(),
                    handle,
                })
            })
            .transpose()
            .map_err(|error| {
                anyhow::Error::new(SetupFailure::new(
                    SetupErrorCode::HelperRequestArgsFailed,
                    format!("duplicate trusted workspace root handle failed: {error}"),
                ))
            })?;
        validate_payload_acl_paths(&payload, pinned_workspace_root.as_ref()).map_err(|error| {
            let native_code = error.chain().find_map(|cause| {
                cause
                    .downcast_ref::<std::io::Error>()
                    .and_then(std::io::Error::raw_os_error)
                    .map(|code| code as u32)
            });
            match native_code {
                Some(code) => anyhow::Error::new(
                    SetupFailure::new(
                        SetupErrorCode::HelperAclRefreshFailed,
                        "ACL path validation failed",
                    )
                    .with_acl_error(WindowsAclError {
                        operation: AclOperation::OpenTarget,
                        code,
                    }),
                )
                .context(error),
                None => error,
            }
        })?;
        run_setup(&payload, &mut log, &sbx_dir, pinned_workspace_root.as_ref())
    })();
    if let Err(err) = &result {
        let _ = log_line(&mut log, &format!("setup error: {err:?}"));
        log_note(&format!("setup error: {err:?}"), Some(sbx_dir.as_path()));
        let failure = classify_setup_failure(err);
        let report = SetupErrorReport {
            code: failure.code,
            message: failure.message,
            acl_operation: failure.acl_operation,
            windows_error_code: failure.windows_error_code,
        };
        if let Err(write_err) = write_setup_error_report(&payload.sandbox_home, &report) {
            let _ = log_line(
                &mut log,
                &format!("setup error report write failed: {write_err}"),
            );
            log_note(
                &format!("setup error report write failed: {write_err}"),
                Some(sbx_dir.as_path()),
            );
        }
    }
    result
}

fn classify_setup_failure(error: &anyhow::Error) -> SetupFailure {
    if let Some(failure) = extract_setup_failure(error) {
        return failure.clone();
    }
    if let Some(acl_error) = error
        .chain()
        .find_map(|cause| cause.downcast_ref::<WindowsAclError>())
        .copied()
    {
        return SetupFailure::new(
            SetupErrorCode::HelperAclRefreshFailed,
            "ACL path validation failed",
        )
        .with_acl_error(acl_error);
    }
    let mut failure = SetupFailure::new(SetupErrorCode::HelperUnknownError, error.to_string());
    failure.windows_error_code = error
        .chain()
        .find_map(|cause| cause.downcast_ref::<std::io::Error>())
        .and_then(std::io::Error::raw_os_error)
        .map(|code| code as u32);
    failure
}

fn run_setup(
    payload: &Payload,
    log: &mut dyn Write,
    sbx_dir: &Path,
    pinned_workspace_root: Option<&PinnedWorkspaceRoot>,
) -> Result<()> {
    let writes_setup_marker = !payload.refresh_only && payload.mode != SetupMode::ReadAclsOnly;
    if writes_setup_marker {
        prepare_setup_marker(&payload.sandbox_home, &payload.real_user)?;
    }
    match payload.mode {
        SetupMode::ReadAclsOnly => run_read_acl_only(payload, log, pinned_workspace_root),
        SetupMode::ProvisionOnly => run_provision_only(payload, log, sbx_dir),
        SetupMode::Full => run_setup_full(payload, log, sbx_dir, pinned_workspace_root),
    }?;
    if writes_setup_marker {
        commit_setup_marker(
            &payload.sandbox_home,
            &payload.offline_username,
            &payload.online_username,
            &payload.proxy_ports,
            payload.allow_local_binding,
        )?;
    }
    Ok(())
}

fn validate_payload_acl_paths(
    payload: &Payload,
    pinned_workspace_root: Option<&PinnedWorkspaceRoot>,
) -> Result<()> {
    ensure_case_insensitive_acl_path(&payload.sandbox_home)?;
    match payload.mode {
        SetupMode::ProvisionOnly => {}
        SetupMode::ReadAclsOnly => {
            for path in &payload.read_roots {
                validate_payload_acl_path(path, pinned_workspace_root, true)?;
            }
        }
        SetupMode::Full => {
            drop(plan_deny_read_acl_paths(&payload.deny_read_paths)?);
            validate_payload_acl_path(&payload.command_cwd, pinned_workspace_root, false)?;
            for path in &payload.read_roots {
                validate_payload_acl_path(path, pinned_workspace_root, true)?;
            }
            for path in &payload.write_roots {
                validate_payload_acl_path(path, pinned_workspace_root, true)?;
            }
            for path in &payload.deny_write_paths {
                validate_payload_acl_path(path, pinned_workspace_root, true)?;
            }
            for path in &payload.revoke_deny_write_paths {
                validate_payload_acl_path(path, pinned_workspace_root, true)?;
            }
        }
    }
    Ok(())
}

fn validate_payload_acl_path(
    path: &Path,
    pinned_workspace_root: Option<&PinnedWorkspaceRoot>,
    allow_missing_leaf: bool,
) -> Result<()> {
    if let Some(pinned) = pinned_workspace_root {
        match open_pinned_workspace_path(&pinned.handle, &pinned.path, path, 0) {
            Ok(Some(_)) => return Ok(()),
            Ok(None) => {}
            Err(error) if allow_missing_leaf && is_missing_path_error(&error) => {
                if let Some(parent) = path.parent()
                    && open_pinned_workspace_path(&pinned.handle, &pinned.path, parent, 0)?
                        .is_some()
                {
                    return Ok(());
                }
                return Err(error);
            }
            Err(error) => return Err(error),
        }
    }
    ensure_case_insensitive_acl_path(path)
}

fn is_missing_path_error(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io_error| io_error.kind() == std::io::ErrorKind::NotFound)
    })
}

fn run_read_acl_only(
    payload: &Payload,
    log: &mut dyn Write,
    pinned_workspace_root: Option<&PinnedWorkspaceRoot>,
) -> Result<()> {
    let _read_acl_guard = match acquire_read_acl_mutex()? {
        Some(guard) => guard,
        None => {
            log_line(log, "read ACL helper already running; skipping")?;
            return Ok(());
        }
    };
    log_line(log, "read-acl-only mode: applying read ACLs")?;
    let sandbox_group_sid = resolve_sandbox_users_group_sid()?;
    let sandbox_group_psid = LocalSidGuard(sid_bytes_to_psid(&sandbox_group_sid)?);
    let mut refresh_errors: Vec<String> = Vec::new();
    if !payload.read_roots.is_empty() {
        let users_sid = resolve_sid("Users")?;
        let users_psid = LocalSidGuard(sid_bytes_to_psid(&users_sid)?);
        let auth_sid = resolve_sid("Authenticated Users")?;
        let auth_psid = LocalSidGuard(sid_bytes_to_psid(&auth_sid)?);
        let everyone_sid = resolve_sid("Everyone")?;
        let everyone_psid = LocalSidGuard(sid_bytes_to_psid(&everyone_sid)?);
        let rx_psids = vec![
            users_psid.as_ptr(),
            auth_psid.as_ptr(),
            everyone_psid.as_ptr(),
        ];
        let subjects = ReadAclSubjects {
            sandbox_group_psid: sandbox_group_psid.as_ptr(),
            rx_psids: &rx_psids,
        };
        apply_read_acls(
            &payload.read_roots,
            &subjects,
            log,
            &mut refresh_errors,
            FILE_GENERIC_READ | FILE_GENERIC_EXECUTE,
            "read",
            OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE,
            pinned_workspace_root,
        )?;
    }
    if !refresh_errors.is_empty() {
        log_line(
            log,
            &format!("read ACL run completed with errors: {refresh_errors:?}"),
        )?;
        anyhow::bail!("read ACL run had errors");
    }
    log_line(log, "read ACL run completed")?;
    Ok(())
}

fn revoke_deny_write_paths(
    payload: &Payload,
    log: &mut dyn Write,
    pinned_workspace_root: Option<&PinnedWorkspaceRoot>,
) -> Result<()> {
    if payload.revoke_deny_write_paths.is_empty() {
        return Ok(());
    }
    let pinned_workspace_root = pinned_workspace_root
        .ok_or_else(|| anyhow::anyhow!("trusted deny-write revoke requires a pinned workspace"))?;
    let mut seen = HashSet::new();
    for path in &payload.revoke_deny_write_paths {
        if !seen.insert(path.clone()) {
            continue;
        }
        let deny_sid_strs = workspace_write_cap_sids_for_path(
            &payload.sandbox_home,
            &payload.command_cwd,
            &payload.write_roots,
            path,
        )?;
        let handle = open_pinned_workspace_path(
            &pinned_workspace_root.handle,
            &pinned_workspace_root.path,
            path,
            windows_sys::Win32::Storage::FileSystem::READ_CONTROL
                | windows_sys::Win32::Storage::FileSystem::WRITE_DAC,
        )
        .map_err(acl_open_failure)?
        .ok_or_else(|| anyhow::anyhow!("trusted deny-write path escaped pinned workspace"))?;
        for deny_sid_str in deny_sid_strs {
            let deny_psid = unsafe {
                convert_string_sid_to_sid(&deny_sid_str)
                    .ok_or_else(|| anyhow::anyhow!("convert deny capability SID failed"))?
            };
            let result =
                unsafe { revoke_deny_write_ace_to_handle(handle.as_raw_handle() as _, deny_psid) };
            unsafe {
                LocalFree(deny_psid as HLOCAL);
            }
            if let Err(error) = result {
                if let Some(acl_error) = error
                    .chain()
                    .find_map(|cause| cause.downcast_ref::<WindowsAclError>())
                    .copied()
                {
                    return Err(anyhow::Error::new(
                        SetupFailure::new(
                            SetupErrorCode::HelperAclRefreshFailed,
                            "deny-write ACL cleanup failed",
                        )
                        .with_acl_error(acl_error),
                    ));
                }
                return Err(anyhow::anyhow!(
                    "revoke deny-write ACE on {} for {} failed: {error}",
                    path.display(),
                    deny_sid_str
                ));
            }
        }
        log_line(
            log,
            &format!(
                "removed stale runtime deny-write ACEs from {}",
                path.display()
            ),
        )?;
    }
    Ok(())
}

fn provision_and_hide_sandbox_users(
    payload: &Payload,
    log: &mut dyn Write,
    sbx_dir: &Path,
) -> Result<()> {
    let provision_result = provision_sandbox_users(
        &payload.sandbox_home,
        &payload.offline_username,
        &payload.online_username,
        log,
    );
    if let Err(err) = provision_result {
        if extract_setup_failure(&err).is_some() {
            return Err(err);
        }
        return Err(anyhow::Error::new(SetupFailure::new(
            SetupErrorCode::HelperUserProvisionFailed,
            format!("provision sandbox users failed: {err}"),
        )));
    }
    let users = vec![
        payload.offline_username.clone(),
        payload.online_username.clone(),
    ];
    hide_newly_created_users(&users, sbx_dir);
    Ok(())
}

fn configure_offline_sandbox_network(
    payload: &Payload,
    offline_sid_str: &str,
    log: &mut dyn Write,
) -> Result<()> {
    let proxy_allowlist_result = firewall::ensure_offline_proxy_allowlist(
        offline_sid_str,
        &payload.proxy_ports,
        payload.allow_local_binding,
        log,
    );
    if let Err(err) = proxy_allowlist_result {
        if extract_setup_failure(&err).is_some() {
            return Err(err);
        }
        return Err(anyhow::Error::new(SetupFailure::new(
            SetupErrorCode::HelperFirewallRuleCreateOrAddFailed,
            format!("ensure offline proxy allowlist failed: {err}"),
        )));
    }
    let firewall_result = firewall::ensure_offline_outbound_block(offline_sid_str, log);
    if let Err(err) = firewall_result {
        if extract_setup_failure(&err).is_some() {
            return Err(err);
        }
        return Err(anyhow::Error::new(SetupFailure::new(
            SetupErrorCode::HelperFirewallRuleCreateOrAddFailed,
            format!("ensure offline outbound block failed: {err}"),
        )));
    }
    install_wfp_filters(
        &payload.offline_username,
        &payload.real_user,
        &payload.proxy_ports,
        payload.allow_local_binding,
        |message| {
            let _ = log_line(log, message);
        },
    )?;
    Ok(())
}

fn lock_persistent_sandbox_dirs(
    payload: &Payload,
    sandbox_group_sid: &[u8],
    log: &mut dyn Write,
) -> Result<()> {
    lock_sandbox_dir(
        &sandbox_dir(&payload.sandbox_home),
        &payload.real_user,
        sandbox_group_sid,
        GRANT_ACCESS,
        FILE_GENERIC_READ | FILE_GENERIC_WRITE | FILE_GENERIC_EXECUTE | DELETE,
        FILE_GENERIC_READ | FILE_GENERIC_WRITE | FILE_GENERIC_EXECUTE,
        log,
    )
    .map_err(|err| {
        anyhow::Error::new(SetupFailure::new(
            SetupErrorCode::HelperSandboxLockFailed,
            format!(
                "lock sandbox dir {} failed: {err}",
                sandbox_dir(&payload.sandbox_home).display()
            ),
        ))
    })?;
    lock_sandbox_dir(
        &sandbox_secrets_dir(&payload.sandbox_home),
        &payload.real_user,
        sandbox_group_sid,
        DENY_ACCESS,
        FILE_GENERIC_READ | FILE_GENERIC_WRITE | FILE_GENERIC_EXECUTE | DELETE,
        FILE_GENERIC_READ | FILE_GENERIC_WRITE | FILE_GENERIC_EXECUTE,
        log,
    )
    .map_err(|err| {
        anyhow::Error::new(SetupFailure::new(
            SetupErrorCode::HelperSandboxLockFailed,
            format!(
                "lock sandbox secrets dir {} failed: {err}",
                sandbox_secrets_dir(&payload.sandbox_home).display()
            ),
        ))
    })?;
    let legacy_users = sandbox_dir(&payload.sandbox_home).join("sandbox_users.json");
    if legacy_users.exists() {
        let _ = std::fs::remove_file(&legacy_users);
    }
    Ok(())
}

fn lock_sandbox_bin_dir(
    payload: &Payload,
    sandbox_group_sid: &[u8],
    log: &mut dyn Write,
) -> Result<()> {
    lock_sandbox_dir(
        &sandbox_bin_dir(&payload.sandbox_home),
        &payload.real_user,
        sandbox_group_sid,
        GRANT_ACCESS,
        FILE_GENERIC_READ | FILE_GENERIC_EXECUTE,
        FILE_GENERIC_READ | FILE_GENERIC_WRITE | FILE_GENERIC_EXECUTE | DELETE,
        log,
    )
    .map_err(|err| {
        anyhow::Error::new(SetupFailure::new(
            SetupErrorCode::HelperSandboxLockFailed,
            format!(
                "lock sandbox bin dir {} failed: {err}",
                sandbox_bin_dir(&payload.sandbox_home).display()
            ),
        ))
    })
}

fn run_provision_only(payload: &Payload, log: &mut dyn Write, sbx_dir: &Path) -> Result<()> {
    provision_and_hide_sandbox_users(payload, log, sbx_dir)?;
    let offline_sid = resolve_sid(&payload.offline_username).map_err(|err| {
        anyhow::Error::new(SetupFailure::new(
            SetupErrorCode::HelperSidResolveFailed,
            format!(
                "resolve SID for offline user {} failed: {err}",
                payload.offline_username
            ),
        ))
    })?;
    let offline_sid_str = string_from_sid_bytes(&offline_sid).map_err(anyhow::Error::msg)?;

    let sandbox_group_sid = resolve_sandbox_users_group_sid().map_err(|err| {
        anyhow::Error::new(SetupFailure::new(
            SetupErrorCode::HelperSidResolveFailed,
            format!("resolve sandbox users group SID failed: {err}"),
        ))
    })?;

    configure_offline_sandbox_network(payload, &offline_sid_str, log)?;

    lock_sandbox_bin_dir(payload, &sandbox_group_sid, log)?;
    lock_persistent_sandbox_dirs(payload, &sandbox_group_sid, log)?;
    log_note("setup provisioning binary completed", Some(sbx_dir));
    Ok(())
}

fn run_setup_full(
    payload: &Payload,
    log: &mut dyn Write,
    sbx_dir: &Path,
    pinned_workspace_root: Option<&PinnedWorkspaceRoot>,
) -> Result<()> {
    let refresh_only = payload.refresh_only;
    if !refresh_only {
        provision_and_hide_sandbox_users(payload, log, sbx_dir)?;
    }
    let offline_sid = resolve_sid(&payload.offline_username).map_err(|err| {
        anyhow::Error::new(SetupFailure::new(
            SetupErrorCode::HelperSidResolveFailed,
            format!(
                "resolve SID for offline user {} failed: {err}",
                payload.offline_username
            ),
        ))
    })?;
    let offline_sid_str = string_from_sid_bytes(&offline_sid).map_err(anyhow::Error::msg)?;

    let sandbox_group_sid = resolve_sandbox_users_group_sid().map_err(|err| {
        anyhow::Error::new(SetupFailure::new(
            SetupErrorCode::HelperSidResolveFailed,
            format!("resolve sandbox users group SID failed: {err}"),
        ))
    })?;
    let sandbox_group_psid_guard =
        LocalSidGuard(sid_bytes_to_psid(&sandbox_group_sid).map_err(|err| {
            anyhow::Error::new(SetupFailure::new(
                SetupErrorCode::HelperSidResolveFailed,
                format!("convert sandbox users group SID to PSID failed: {err}"),
            ))
        })?);
    let sandbox_group_psid = sandbox_group_psid_guard.as_ptr();
    let sandbox_group_sid_str =
        string_from_sid_bytes(&sandbox_group_sid).map_err(anyhow::Error::msg)?;

    let mut refresh_errors: Vec<String> = Vec::new();
    let mut preferred_refresh_error = None;
    if !refresh_only {
        configure_offline_sandbox_network(payload, &offline_sid_str, log)?;
    }

    // Trusted preparation skips protected-path deny installation; revoke only the current,
    // already-existing runtime-owned deny-write ACEs before the normal refresh grants run.
    revoke_deny_write_paths(payload, log, pinned_workspace_root)?;

    // Codex uses the dedicated Sandbox Users group as the authoritative read principal.
    // Apply deny-read ACLs to that same principal before any child starts. The product caller
    // holds the execution mutex across setup and the complete Job Object lifetime; this helper's
    // state mutex then serializes the ownership-aware current-set reconciliation itself.
    let applied_deny_read_paths = unsafe {
        sync_persistent_deny_read_acls_with_pinned_root(
            &payload.sandbox_home,
            &sandbox_group_sid_str,
            &payload.deny_read_paths,
            sandbox_group_psid,
            pinned_workspace_root.map(|pinned| (&pinned.handle, pinned.path.as_path())),
        )
    }
    .map_err(|error| {
        let failure = error
            .chain()
            .find_map(|cause| cause.downcast_ref::<WindowsAclError>())
            .copied()
            .map_or_else(
                || {
                    SetupFailure::new(
                        SetupErrorCode::HelperDenyReadAclFailed,
                        "deny-read ACL reconciliation failed",
                    )
                },
                |acl_error| {
                    SetupFailure::new(
                        SetupErrorCode::HelperDenyReadAclFailed,
                        "deny-read ACL reconciliation failed",
                    )
                    .with_acl_error(acl_error)
                },
            );
        anyhow::Error::new(failure)
    })?;
    if !applied_deny_read_paths.is_empty() {
        log_line(
            log,
            &format!("applied {} deny-read ACLs", applied_deny_read_paths.len()),
        )?;
    }

    if payload.read_roots.is_empty() {
        log_line(log, "no read roots to grant; skipping read ACL helper")?;
    } else {
        let mutex_state = probe_read_acl_mutex().map_err(|error| {
            let failure = error
                .chain()
                .find_map(|cause| cause.downcast_ref::<ReadAclMutexError>())
                .copied()
                .map_or_else(
                    || {
                        SetupFailure::new(
                            SetupErrorCode::HelperReadAclMutexProbeFailed,
                            "read ACL mutex probe failed",
                        )
                    },
                    |mutex_error| {
                        SetupFailure::new(
                            SetupErrorCode::HelperReadAclMutexProbeFailed,
                            "read ACL mutex probe failed",
                        )
                        .with_mutex_error(mutex_error)
                    },
                );
            anyhow::Error::new(failure)
        })?;
        match mutex_state {
            ReadAclMutexState::Present => {
                log_line(log, "read ACL helper already running; skipping spawn")?;
            }
            ReadAclMutexState::Absent => {
                spawn_read_acl_helper(payload, log).map_err(|err| {
                    anyhow::Error::new(SetupFailure::new(
                        SetupErrorCode::HelperReadAclHelperSpawnFailed,
                        format!("spawn read ACL helper failed: {err}"),
                    ))
                })?;
            }
        }
    }

    if refresh_only {
        setup_runtime_bin::ensure_singularity_runtime_paths_readable(
            sandbox_group_psid,
            &mut refresh_errors,
            log,
        )?;
    }

    let mut grant_tasks: Vec<(PathBuf, String)> = Vec::new();

    let mut seen_deny_paths: HashSet<PathBuf> = HashSet::new();
    let mut seen_write_roots: HashSet<PathBuf> = HashSet::new();
    let canonical_command_cwd = canonicalize_path(&payload.command_cwd);

    for root in &payload.write_roots {
        if !seen_write_roots.insert(root.clone()) {
            continue;
        }
        if !root.exists() {
            log_line(
                log,
                &format!("write root {} missing; skipping", root.display()),
            )?;
            continue;
        }
        let mut need_grant = false;
        let is_command_cwd = is_command_cwd_root(root, &canonical_command_cwd);
        let cap_label = if is_command_cwd {
            "workspace_cap"
        } else {
            "root_cap"
        };
        let root_cap_sid_str =
            workspace_write_cap_sid_for_root(&payload.sandbox_home, &payload.command_cwd, root)?;
        let root_cap_psid = unsafe {
            convert_string_sid_to_sid(&root_cap_sid_str)
                .ok_or_else(|| anyhow::anyhow!("convert write root capability SID failed"))?
        };
        for (label, psid) in [
            ("sandbox_group", sandbox_group_psid),
            (cap_label, root_cap_psid),
        ] {
            let needs_refresh = match write_root_needs_refresh(root, psid, pinned_workspace_root) {
                Ok(needs_refresh) => needs_refresh,
                Err(e) => {
                    let message = format!(
                        "write ACE check failed on {} for {label}: {}",
                        root.display(),
                        e
                    );
                    log_line(
                        log,
                        &format!(
                            "write ACE check failed on {} for {label}: {}; continuing",
                            root.display(),
                            e
                        ),
                    )?;
                    retain_preferred_acl_error(
                        &mut preferred_refresh_error,
                        e.context(format!("check write ACE on {} for {label}", root.display())),
                    );
                    refresh_errors.push(message);
                    true
                }
            };
            if needs_refresh {
                need_grant = true;
            }
        }
        unsafe {
            LocalFree(root_cap_psid as HLOCAL);
        }
        if need_grant {
            log_line(
                log,
                &format!(
                    "granting write ACE to {} for sandbox group and capability SID",
                    root.display()
                ),
            )?;
            grant_tasks.push((root.clone(), root_cap_sid_str));
        }
    }

    let (tx, rx) = mpsc::channel::<(PathBuf, Result<bool>)>();
    std::thread::scope(|scope| {
        for (root, root_cap_sid_str) in grant_tasks {
            let sid_strings = vec![sandbox_group_sid_str.clone(), root_cap_sid_str];
            let tx = tx.clone();
            scope.spawn(move || {
                // Convert SID strings to psids locally in this thread.
                let mut psids: Vec<*mut c_void> = Vec::new();
                for sid_str in &sid_strings {
                    if let Some(psid) = unsafe { convert_string_sid_to_sid(sid_str) } {
                        psids.push(psid);
                    } else {
                        let _ = tx.send((root.clone(), Err(anyhow::anyhow!("convert SID failed"))));
                        return;
                    }
                }

                let res = unsafe {
                    ensure_allow_write_aces_for_path(&root, &psids, pinned_workspace_root)
                };

                for psid in psids {
                    unsafe {
                        LocalFree(psid as HLOCAL);
                    }
                }
                let _ = tx.send((root, res));
            });
        }
        drop(tx);
        for (root, res) in rx {
            match res {
                Ok(_) => {}
                Err(e) => {
                    let message = format!("write ACE failed on {}: {}", root.display(), e);
                    if log_line(
                        log,
                        &format!("write ACE grant failed on {}: {}", root.display(), e),
                    )
                    .is_err()
                    {
                        // ignore log errors inside scoped thread
                    }
                    retain_preferred_acl_error(
                        &mut preferred_refresh_error,
                        e.context(format!("grant write ACE on {}", root.display())),
                    );
                    refresh_errors.push(message);
                }
            }
        }
    });

    for path in &payload.deny_write_paths {
        if !seen_deny_paths.insert(path.clone()) {
            continue;
        }
        if existing_public_certificate_only_pem(path)? {
            continue;
        }
        // Deny ACEs attach to filesystem objects. Materialize only missing carveouts without
        // following a reparse point in any ancestor so a child cannot create the path later.
        let mut materialized = match std::fs::symlink_metadata(path) {
            Ok(_) => None,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Some(ensure_missing_protected_path_materialized(path)?)
            }
            Err(error) => return Err(error.into()),
        };

        let deny_sid_strs = workspace_write_cap_sids_for_path(
            &payload.sandbox_home,
            &payload.command_cwd,
            &payload.write_roots,
            path,
        )?;
        for deny_sid_str in deny_sid_strs {
            let deny_psid = unsafe {
                convert_string_sid_to_sid(&deny_sid_str)
                    .ok_or_else(|| anyhow::anyhow!("convert deny capability SID failed"))?
            };

            let result = match &materialized {
                Some(materialized) => unsafe { materialized.add_deny_write_ace(deny_psid) },
                None => unsafe {
                    add_deny_write_ace_for_path(path, deny_psid, pinned_workspace_root)
                },
            };
            match result {
                Ok(true) => {
                    log_line(
                        log,
                        &format!("applied deny ACE to protect {}", path.display()),
                    )?;
                }
                Ok(false) => {}
                Err(err) => {
                    let message = format!("deny ACE failed on {}: {err}", path.display());
                    if let Some(materialized) = materialized.take()
                        && let Err(cleanup) = materialized.cleanup_if_empty()
                    {
                        refresh_errors.push(format!(
                            "deny ACE sentinel cleanup failed on {}: {cleanup}",
                            path.display()
                        ));
                    }
                    log_line(
                        log,
                        &format!("deny ACE failed on {}: {err}", path.display()),
                    )?;
                    retain_preferred_acl_error(
                        &mut preferred_refresh_error,
                        err.context(format!("apply deny-write ACE to {}", path.display())),
                    );
                    refresh_errors.push(message);
                }
            }
            unsafe {
                LocalFree(deny_psid as HLOCAL);
            }
        }
    }

    lock_sandbox_bin_dir(payload, &sandbox_group_sid, log)?;

    if refresh_only {
        log_line(
            log,
            &format!(
                "setup refresh: processed {} write roots (read roots delegated); errors={:?}",
                payload.write_roots.len(),
                refresh_errors
            ),
        )?;
    }
    if !refresh_only {
        lock_persistent_sandbox_dirs(payload, &sandbox_group_sid, log)?;
    }

    if !refresh_errors.is_empty() {
        log_line(
            log,
            &format!("setup refresh completed with errors: {refresh_errors:?}"),
        )?;
        if let Some(error) = preferred_refresh_error {
            if let Some(acl_error) = error
                .chain()
                .find_map(|cause| cause.downcast_ref::<WindowsAclError>())
                .copied()
            {
                return Err(anyhow::Error::new(
                    SetupFailure::new(SetupErrorCode::HelperAclRefreshFailed, "ACL refresh failed")
                        .with_acl_error(acl_error),
                ));
            }
            return Err(error.context("setup refresh failed"));
        }
        anyhow::bail!("setup refresh had errors");
    }
    log_note("setup binary completed", Some(sbx_dir));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::Payload;
    use super::PinnedWorkspaceRoot;
    use super::SETUP_VERSION;
    use super::WRITE_ROOT_ALLOW_MASK;
    use super::acl_open_failure;
    use super::classify_setup_failure;
    use super::convert_string_sid_to_sid;
    use super::validate_payload_acl_path;
    use super::workspace_write_cap_sids_for_path;
    use super::write_root_needs_refresh;
    use singularity_windows_sandbox::TrustedWorkspaceLease;
    use singularity_windows_sandbox::ensure_allow_mask_aces;
    use singularity_windows_sandbox::ensure_allow_write_aces;
    use singularity_windows_sandbox::load_or_create_cap_sids;
    use singularity_windows_sandbox::workspace_write_cap_sid_for_root;

    use pretty_assertions::assert_eq;
    use serde_json::json;
    use std::fs;
    use windows_sys::Win32::Foundation::HLOCAL;
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Storage::FileSystem::FILE_DELETE_CHILD;

    #[test]
    fn trusted_acl_open_access_denied_requests_elevated_authority() {
        let error = acl_open_failure(anyhow::Error::new(std::io::Error::from_raw_os_error(5)));
        let failure = singularity_windows_sandbox::extract_setup_failure(&error)
            .expect("typed setup failure");

        assert_eq!(
            failure.code,
            singularity_windows_sandbox::SetupErrorCode::HelperAclRefreshFailed
        );
        assert_eq!(
            failure.acl_operation,
            Some(singularity_windows_sandbox::AclOperation::OpenTarget)
        );
        assert_eq!(failure.windows_error_code, Some(5));
    }

    #[test]
    fn unknown_setup_failure_preserves_the_win32_error_code() {
        let error = anyhow::Error::new(std::io::Error::from_raw_os_error(5))
            .context("open pinned workspace component");

        let failure = classify_setup_failure(&error);

        assert_eq!(
            failure.code,
            singularity_windows_sandbox::SetupErrorCode::HelperUnknownError
        );
        assert_eq!(failure.windows_error_code, Some(5));
        assert_eq!(failure.message, "open pinned workspace component");
    }

    fn payload_json() -> serde_json::Value {
        json!({
            "version": SETUP_VERSION,
            "offline_username": singularity_windows_sandbox::product_identity::OFFLINE_ACCOUNT_NAME,
            "online_username": singularity_windows_sandbox::product_identity::ONLINE_ACCOUNT_NAME,
            "sandbox_home": "C:\\singularity-home",
            "command_cwd": "C:\\workspace",
            "read_roots": [],
            "write_roots": [],
            "proxy_ports": [],
            "real_user": "User",
        })
    }

    #[test]
    fn payload_accepts_provision_only_mode() {
        let mut payload = payload_json();
        payload["mode"] = json!("provision-only");
        let payload: Payload = serde_json::from_value(payload).expect("payload");

        assert_eq!(payload.mode, super::SetupMode::ProvisionOnly);
    }

    #[test]
    fn pinned_payload_validation_preserves_missing_leaf_semantics() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join("workspace");
        let missing = root.join("future-root");
        fs::create_dir(&root).expect("workspace");
        let lease = TrustedWorkspaceLease::acquire(&root).expect("trusted workspace lease");
        let pinned = PinnedWorkspaceRoot {
            path: root,
            handle: lease
                .duplicate_root_handle()
                .expect("duplicate root handle"),
        };

        validate_payload_acl_path(&missing, Some(&pinned), true)
            .expect("missing read/write leaf is validated through pinned parent");
        assert!(
            validate_payload_acl_path(&missing, Some(&pinned), false).is_err(),
            "command cwd must remain required"
        );
    }

    #[test]
    fn write_root_refresh_replaces_stale_delete_child_grant() {
        let temp = tempfile::tempdir().expect("tempdir");
        let sandbox_home = temp.path().join("singularity-home");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&sandbox_home).expect("create singularity home");
        fs::create_dir_all(&workspace).expect("create workspace");

        let sid = workspace_write_cap_sid_for_root(&sandbox_home, &workspace, &workspace)
            .expect("workspace sid");
        let psid = unsafe { convert_string_sid_to_sid(&sid).expect("convert workspace sid") };
        let stale_write_mask = WRITE_ROOT_ALLOW_MASK | FILE_DELETE_CHILD;
        let seeded = unsafe { ensure_allow_mask_aces(&workspace, &[psid], stale_write_mask) }
            .expect("seed stale write ACE");
        let needs_refresh_before =
            write_root_needs_refresh(&workspace, psid, None).expect("check stale write ACE");
        let replaced = unsafe { ensure_allow_write_aces(&workspace, &[psid]) }
            .expect("replace stale write ACE");
        let needs_refresh_after =
            write_root_needs_refresh(&workspace, psid, None).expect("check refreshed write ACE");
        unsafe {
            LocalFree(psid as HLOCAL);
        }

        assert_eq!(
            (seeded, needs_refresh_before, replaced, needs_refresh_after),
            (true, true, true, false)
        );
    }

    #[test]
    fn deny_path_under_active_root_uses_only_matching_root_sid() {
        let temp = tempfile::tempdir().expect("tempdir");
        let sandbox_home = temp.path().join("singularity-home");
        let workspace = temp.path().join("workspace");
        let active_root = temp.path().join("active-root");
        let stale_root = temp.path().join("stale-root");
        let deny_path = active_root.join("protected");
        fs::create_dir_all(&sandbox_home).expect("create singularity home");
        fs::create_dir_all(&workspace).expect("create workspace");
        fs::create_dir_all(&active_root).expect("create active root");
        fs::create_dir_all(&stale_root).expect("create stale root");
        fs::create_dir_all(&deny_path).expect("create deny path");

        let stale_sid = workspace_write_cap_sid_for_root(&sandbox_home, &workspace, &stale_root)
            .expect("stale sid");
        let active_sid = workspace_write_cap_sid_for_root(&sandbox_home, &workspace, &active_root)
            .expect("active sid");
        let workspace_sid = workspace_write_cap_sid_for_root(&sandbox_home, &workspace, &workspace)
            .expect("workspace sid");
        let caps = load_or_create_cap_sids(&sandbox_home).expect("load caps");

        let deny_sids = workspace_write_cap_sids_for_path(
            &sandbox_home,
            &workspace,
            &[workspace.clone(), active_root],
            &deny_path,
        )
        .expect("deny sids");

        assert_eq!(deny_sids, vec![active_sid]);
        assert!(!deny_sids.contains(&workspace_sid));
        assert!(!deny_sids.contains(&stale_sid));
        assert!(!deny_sids.contains(&caps.workspace));
    }

    #[test]
    fn deny_path_outside_active_roots_falls_back_to_all_active_root_sids() {
        let temp = tempfile::tempdir().expect("tempdir");
        let sandbox_home = temp.path().join("singularity-home");
        let workspace = temp.path().join("workspace");
        let active_root = temp.path().join("active-root");
        let stale_root = temp.path().join("stale-root");
        let deny_path = temp.path().join("outside-deny");
        fs::create_dir_all(&sandbox_home).expect("create singularity home");
        fs::create_dir_all(&workspace).expect("create workspace");
        fs::create_dir_all(&active_root).expect("create active root");
        fs::create_dir_all(&stale_root).expect("create stale root");
        fs::create_dir_all(&deny_path).expect("create deny path");

        let stale_sid = workspace_write_cap_sid_for_root(&sandbox_home, &workspace, &stale_root)
            .expect("stale sid");
        let active_sid = workspace_write_cap_sid_for_root(&sandbox_home, &workspace, &active_root)
            .expect("active sid");
        let workspace_sid = workspace_write_cap_sid_for_root(&sandbox_home, &workspace, &workspace)
            .expect("workspace sid");
        let caps = load_or_create_cap_sids(&sandbox_home).expect("load caps");

        let deny_sids = workspace_write_cap_sids_for_path(
            &sandbox_home,
            &workspace,
            &[workspace.clone(), active_root],
            &deny_path,
        )
        .expect("deny sids");

        assert_eq!(deny_sids.len(), 2);
        assert!(deny_sids.contains(&workspace_sid));
        assert!(deny_sids.contains(&active_sid));
        assert!(!deny_sids.contains(&stale_sid));
        assert!(!deny_sids.contains(&caps.workspace));
    }

    #[test]
    fn deny_path_includes_nested_active_root_sid() {
        let temp = tempfile::tempdir().expect("tempdir");
        let sandbox_home = temp.path().join("singularity-home");
        let workspace = temp.path().join("workspace");
        let protected_dir = workspace.join(".singularity");
        let nested_root = protected_dir.join("nested-root");
        fs::create_dir_all(&sandbox_home).expect("create singularity home");
        fs::create_dir_all(&workspace).expect("create workspace");
        fs::create_dir_all(&nested_root).expect("create nested root");

        let workspace_sid = workspace_write_cap_sid_for_root(&sandbox_home, &workspace, &workspace)
            .expect("workspace sid");
        let nested_sid = workspace_write_cap_sid_for_root(&sandbox_home, &workspace, &nested_root)
            .expect("nested sid");

        let deny_sids = workspace_write_cap_sids_for_path(
            &sandbox_home,
            &workspace,
            &[workspace.clone(), nested_root],
            &protected_dir,
        )
        .expect("deny sids");

        let mut expected = vec![workspace_sid, nested_sid];
        expected.sort();
        assert_eq!(deny_sids, expected);
    }
}
