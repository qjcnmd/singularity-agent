use crate::path_normalization::normalized_path_text;
use crate::winutil::to_wide;
use anyhow::Result;
use std::error::Error;
use std::ffi::c_void;
use std::fmt;
use std::path::Path;
use std::path::PathBuf;
use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::Foundation::ERROR_INVALID_DATA;
use windows_sys::Win32::Foundation::ERROR_SUCCESS;
use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Foundation::HLOCAL;
use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
use windows_sys::Win32::Foundation::LocalFree;
use windows_sys::Win32::Security::ACCESS_ALLOWED_ACE;
use windows_sys::Win32::Security::ACCESS_DENIED_ACE;
use windows_sys::Win32::Security::ACE_HEADER;
use windows_sys::Win32::Security::ACL;
use windows_sys::Win32::Security::ACL_SIZE_INFORMATION;
use windows_sys::Win32::Security::AclSizeInformation;
use windows_sys::Win32::Security::Authorization::EXPLICIT_ACCESS_W;
use windows_sys::Win32::Security::Authorization::GetSecurityInfo;
use windows_sys::Win32::Security::Authorization::SetEntriesInAclW;
use windows_sys::Win32::Security::Authorization::SetSecurityInfo;
use windows_sys::Win32::Security::Authorization::TRUSTEE_IS_SID;
use windows_sys::Win32::Security::Authorization::TRUSTEE_IS_UNKNOWN;
use windows_sys::Win32::Security::Authorization::TRUSTEE_W;
use windows_sys::Win32::Security::DACL_SECURITY_INFORMATION;
use windows_sys::Win32::Security::EqualSid;
use windows_sys::Win32::Security::GENERIC_MAPPING;
use windows_sys::Win32::Security::GetAce;
use windows_sys::Win32::Security::GetAclInformation;
use windows_sys::Win32::Security::MapGenericMask;
use windows_sys::Win32::Storage::FileSystem::BY_HANDLE_FILE_INFORMATION;
use windows_sys::Win32::Storage::FileSystem::CreateFileW;
use windows_sys::Win32::Storage::FileSystem::DELETE;
use windows_sys::Win32::Storage::FileSystem::FILE_ALL_ACCESS;
use windows_sys::Win32::Storage::FileSystem::FILE_APPEND_DATA;
use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_NORMAL;
use windows_sys::Win32::Storage::FileSystem::FILE_DELETE_CHILD;
use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS;
use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
use windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_EXECUTE;
use windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_READ;
use windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_WRITE;
use windows_sys::Win32::Storage::FileSystem::FILE_NAME_NORMALIZED;
use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_DELETE;
use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;
use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_WRITE;
use windows_sys::Win32::Storage::FileSystem::FILE_WRITE_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::FILE_WRITE_DATA;
use windows_sys::Win32::Storage::FileSystem::FILE_WRITE_EA;
use windows_sys::Win32::Storage::FileSystem::GetFinalPathNameByHandleW;
use windows_sys::Win32::Storage::FileSystem::OPEN_EXISTING;
use windows_sys::Win32::Storage::FileSystem::READ_CONTROL;
use windows_sys::Win32::Storage::FileSystem::WRITE_DAC;
const SE_KERNEL_OBJECT: u32 = 6;
const INHERIT_ONLY_ACE: u8 = 0x08;
const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
const ACCESS_DENIED_ACE_TYPE: u8 = 1;
const GENERIC_READ_MASK: u32 = 0x8000_0000;
const GENERIC_WRITE_MASK: u32 = 0x4000_0000;
const DENY_ACCESS: i32 = 3;
const DENY_READ_MASK: u32 = FILE_GENERIC_READ | GENERIC_READ_MASK;
const DENY_WRITE_MASK: u32 = FILE_GENERIC_WRITE
    | FILE_WRITE_DATA
    | FILE_APPEND_DATA
    | FILE_WRITE_EA
    | FILE_WRITE_ATTRIBUTES
    | GENERIC_WRITE_MASK
    | DELETE
    | FILE_DELETE_CHILD;

/// The Win32 operation that failed while reading or writing a DACL.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AclOperation {
    OpenTarget,
    ReparseTargetUnsupported,
    QueryTargetIdentity,
    TargetIdentityMismatch,
    GetSecurityInfo,
    SetEntriesInAcl,
    SetSecurityInfo,
}

/// Preserves the native error code so callers can fail closed without reducing an ACL failure
/// to a best-effort log message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WindowsAclError {
    pub operation: AclOperation,
    pub code: u32,
}

impl WindowsAclError {
    fn new(operation: AclOperation, code: u32) -> Self {
        Self { operation, code }
    }
}

impl fmt::Display for WindowsAclError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Windows ACL {:?} failed with error {}",
            self.operation, self.code
        )
    }
}

impl Error for WindowsAclError {}

struct AclTarget {
    handle: HANDLE,
    p_dacl: *mut ACL,
    p_sd: *mut c_void,
}

impl Drop for AclTarget {
    fn drop(&mut self) {
        unsafe {
            if !self.p_sd.is_null() {
                LocalFree(self.p_sd as HLOCAL);
            }
            if self.handle != 0 && self.handle != INVALID_HANDLE_VALUE {
                CloseHandle(self.handle);
            }
        }
    }
}

unsafe fn open_acl_target(path: &Path, desired_access: u32, object_type: i32) -> Result<AclTarget> {
    open_acl_target_with_flags(
        path,
        desired_access,
        object_type,
        FILE_FLAG_BACKUP_SEMANTICS,
    )
}

unsafe fn open_acl_target_with_flags(
    path: &Path,
    desired_access: u32,
    object_type: i32,
    flags_and_attributes: u32,
) -> Result<AclTarget> {
    if object_type == 1 && path_contains_reparse_component(path)? {
        return Err(anyhow::Error::new(WindowsAclError::new(
            AclOperation::ReparseTargetUnsupported,
            50, // ERROR_NOT_SUPPORTED
        )));
    }
    let expected_identity = if object_type == 1 {
        Some(dunce::canonicalize(path).map_err(|err| {
            anyhow::Error::new(WindowsAclError::new(
                AclOperation::QueryTargetIdentity,
                ERROR_INVALID_DATA,
            ))
            .context(format!("canonicalize ACL target {}: {err}", path.display()))
        })?)
    } else {
        None
    };
    let wpath = to_wide(path);
    let flags_and_attributes = if object_type == 1 {
        flags_and_attributes | FILE_FLAG_OPEN_REPARSE_POINT
    } else {
        flags_and_attributes
    };
    let handle = CreateFileW(
        wpath.as_ptr(),
        desired_access,
        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
        std::ptr::null_mut(),
        OPEN_EXISTING,
        flags_and_attributes,
        0,
    );
    if handle == 0 || handle == INVALID_HANDLE_VALUE {
        return Err(anyhow::Error::new(WindowsAclError::new(
            AclOperation::OpenTarget,
            GetLastError(),
        )));
    }
    if object_type == 1 {
        let mut info: BY_HANDLE_FILE_INFORMATION = std::mem::zeroed();
        if windows_sys::Win32::Storage::FileSystem::GetFileInformationByHandle(handle, &mut info)
            == 0
        {
            let code = GetLastError();
            CloseHandle(handle);
            return Err(anyhow::Error::new(WindowsAclError::new(
                AclOperation::QueryTargetIdentity,
                code,
            )));
        }
        if info.dwFileAttributes & 0x0000_0400 != 0 {
            CloseHandle(handle);
            return Err(anyhow::Error::new(WindowsAclError::new(
                AclOperation::ReparseTargetUnsupported,
                50, // ERROR_NOT_SUPPORTED
            )));
        }
        if path_contains_reparse_component(path)? {
            CloseHandle(handle);
            return Err(anyhow::Error::new(WindowsAclError::new(
                AclOperation::ReparseTargetUnsupported,
                50, // ERROR_NOT_SUPPORTED
            )));
        }
        if let Err(error) = verify_target_identity_against(
            handle,
            expected_identity
                .as_deref()
                .expect("file ACL target has an expected identity"),
        ) {
            CloseHandle(handle);
            return Err(error);
        }
    }
    let mut p_sd: *mut c_void = std::ptr::null_mut();
    let mut p_dacl: *mut ACL = std::ptr::null_mut();
    let code = GetSecurityInfo(
        handle,
        object_type,
        DACL_SECURITY_INFORMATION,
        std::ptr::null_mut(),
        std::ptr::null_mut(),
        &mut p_dacl,
        std::ptr::null_mut(),
        &mut p_sd,
    );
    if code != ERROR_SUCCESS {
        CloseHandle(handle);
        return Err(anyhow::Error::new(WindowsAclError::new(
            AclOperation::GetSecurityInfo,
            code,
        )));
    }
    Ok(AclTarget {
        handle,
        p_dacl,
        p_sd,
    })
}

unsafe fn final_path_from_handle(handle: HANDLE) -> Result<PathBuf> {
    let mut buffer = vec![0u16; 512];
    loop {
        let length = GetFinalPathNameByHandleW(
            handle,
            buffer.as_mut_ptr(),
            buffer.len() as u32,
            FILE_NAME_NORMALIZED,
        );
        if length == 0 {
            return Err(anyhow::Error::new(WindowsAclError::new(
                AclOperation::QueryTargetIdentity,
                GetLastError(),
            )));
        }
        if (length as usize) < buffer.len() {
            let text = String::from_utf16(&buffer[..length as usize]).map_err(|_| {
                anyhow::Error::new(WindowsAclError::new(
                    AclOperation::QueryTargetIdentity,
                    ERROR_INVALID_DATA,
                ))
            })?;
            return Ok(PathBuf::from(text));
        }
        if buffer.len() >= 32 * 1024 {
            return Err(anyhow::Error::new(WindowsAclError::new(
                AclOperation::QueryTargetIdentity,
                ERROR_INVALID_DATA,
            )));
        }
        buffer.resize(buffer.len() * 2, 0);
    }
}

fn normalized_identity(path: &Path) -> String {
    normalized_path_text(path).to_ascii_lowercase()
}

pub(crate) unsafe fn verify_target_identity_against(handle: HANDLE, expected: &Path) -> Result<()> {
    let actual = final_path_from_handle(handle)?;
    if normalized_identity(expected) != normalized_identity(&actual) {
        return Err(anyhow::Error::new(WindowsAclError::new(
            AclOperation::TargetIdentityMismatch,
            ERROR_INVALID_DATA,
        ))
        .context(format!(
            "ACL target identity changed: expected {}, opened {}",
            expected.display(),
            actual.display()
        )));
    }
    Ok(())
}

pub(crate) fn path_contains_reparse_component(path: &Path) -> Result<bool> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        // A Windows prefix such as `\\?\D:` is not itself a valid filesystem
        // target. Preserve the verbatim form for long-path support, but only
        // query metadata after the disk or UNC root is complete.
        if matches!(component, std::path::Component::Prefix(_)) {
            continue;
        }
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) => {
                if std::os::windows::fs::MetadataExt::file_attributes(&metadata) & 0x0000_0400 != 0
                {
                    return Ok(true);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                let code = error
                    .raw_os_error()
                    .map_or(ERROR_INVALID_DATA, |code| code as u32);
                return Err(anyhow::Error::new(WindowsAclError::new(
                    AclOperation::QueryTargetIdentity,
                    code,
                ))
                .context(format!(
                    "inspect ACL target reparse state {}: {error}",
                    current.display()
                )));
            }
        }
    }
    Ok(false)
}

unsafe fn set_target_dacl(target: &AclTarget, p_dacl: *mut ACL, object_type: i32) -> Result<()> {
    let code = SetSecurityInfo(
        target.handle,
        object_type,
        DACL_SECURITY_INFORMATION,
        std::ptr::null_mut(),
        std::ptr::null_mut(),
        p_dacl,
        std::ptr::null_mut(),
    );
    if code != ERROR_SUCCESS {
        return Err(anyhow::Error::new(WindowsAclError::new(
            AclOperation::SetSecurityInfo,
            code,
        )));
    }
    Ok(())
}

/// Replaces a file-object DACL through a stable handle rather than a path-based setter.
///
/// # Safety
/// Caller must pass a valid DACL pointer and an existing non-reparse path.
pub unsafe fn set_dacl_for_path(path: &Path, p_dacl: *mut ACL) -> Result<()> {
    let target = open_acl_target(path, READ_CONTROL | WRITE_DAC, 1)?;
    set_target_dacl(&target, p_dacl, 1)
}

fn is_missing_target_error(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<WindowsAclError>()
        .is_some_and(|error| {
            matches!(error.operation, AclOperation::OpenTarget) && matches!(error.code, 2 | 3 | 267)
        })
}

/// Fetch DACL via handle-based query; caller must LocalFree the returned SD.
///
/// # Safety
/// Caller must free the returned security descriptor with `LocalFree` and pass an existing path.
pub unsafe fn fetch_dacl_handle(path: &Path) -> Result<(*mut ACL, *mut c_void)> {
    let mut target = open_acl_target(path, READ_CONTROL, 1)?;
    let p_dacl = target.p_dacl;
    let p_sd = target.p_sd;
    let handle = target.handle;
    target.p_sd = std::ptr::null_mut();
    target.handle = 0;
    CloseHandle(handle);
    Ok((p_dacl, p_sd))
}

/// Fast mask-based check: does an ACE for provided SIDs grant the desired mask? Skips inherit-only.
/// When `require_all_bits` is true, all bits in `desired_mask` must be present; otherwise any bit suffices.
pub unsafe fn dacl_mask_allows(
    p_dacl: *mut ACL,
    psids: &[*mut c_void],
    desired_mask: u32,
    require_all_bits: bool,
) -> bool {
    if p_dacl.is_null() {
        return false;
    }
    let mut info: ACL_SIZE_INFORMATION = std::mem::zeroed();
    let ok = GetAclInformation(
        p_dacl as *const ACL,
        &mut info as *mut _ as *mut c_void,
        std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
        AclSizeInformation,
    );
    if ok == 0 {
        return false;
    }
    let mapping = GENERIC_MAPPING {
        GenericRead: FILE_GENERIC_READ,
        GenericWrite: FILE_GENERIC_WRITE,
        GenericExecute: FILE_GENERIC_EXECUTE,
        GenericAll: FILE_ALL_ACCESS,
    };
    for i in 0..(info.AceCount as usize) {
        let mut p_ace: *mut c_void = std::ptr::null_mut();
        if GetAce(p_dacl as *const ACL, i as u32, &mut p_ace) == 0 {
            continue;
        }
        let hdr = &*(p_ace as *const ACE_HEADER);
        if hdr.AceType != ACCESS_ALLOWED_ACE_TYPE {
            continue; // not ACCESS_ALLOWED
        }
        if (hdr.AceFlags & INHERIT_ONLY_ACE) != 0 {
            continue;
        }
        let base = p_ace as usize;
        let sid_ptr =
            (base + std::mem::size_of::<ACE_HEADER>() + std::mem::size_of::<u32>()) as *mut c_void;
        let mut matched = false;
        for sid in psids {
            if EqualSid(sid_ptr, *sid) != 0 {
                matched = true;
                break;
            }
        }
        if !matched {
            continue;
        }
        let ace = &*(p_ace as *const ACCESS_ALLOWED_ACE);
        let mut mask = ace.Mask;
        MapGenericMask(&mut mask, &mapping);
        if (require_all_bits && (mask & desired_mask) == desired_mask)
            || (!require_all_bits && (mask & desired_mask) != 0)
        {
            return true;
        }
    }
    false
}

/// Path-based wrapper around the mask check (single DACL fetch).
pub fn path_mask_allows(
    path: &Path,
    psids: &[*mut c_void],
    desired_mask: u32,
    require_all_bits: bool,
) -> Result<bool> {
    unsafe {
        let (p_dacl, sd) = fetch_dacl_handle(path)?;
        let has = dacl_mask_allows(p_dacl, psids, desired_mask, require_all_bits);
        if !sd.is_null() {
            LocalFree(sd as HLOCAL);
        }
        Ok(has)
    }
}

pub unsafe fn dacl_has_write_allow_for_sid(p_dacl: *mut ACL, psid: *mut c_void) -> bool {
    if p_dacl.is_null() {
        return false;
    }
    let mut info: ACL_SIZE_INFORMATION = std::mem::zeroed();
    let ok = GetAclInformation(
        p_dacl as *const ACL,
        &mut info as *mut _ as *mut c_void,
        std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
        AclSizeInformation,
    );
    if ok == 0 {
        return false;
    }
    let count = info.AceCount as usize;
    for i in 0..count {
        let mut p_ace: *mut c_void = std::ptr::null_mut();
        if GetAce(p_dacl as *const ACL, i as u32, &mut p_ace) == 0 {
            continue;
        }
        let hdr = &*(p_ace as *const ACE_HEADER);
        if hdr.AceType != ACCESS_ALLOWED_ACE_TYPE {
            continue; // ACCESS_ALLOWED_ACE_TYPE
        }
        // Ignore ACEs that are inherit-only (do not apply to the current object)
        if (hdr.AceFlags & INHERIT_ONLY_ACE) != 0 {
            continue;
        }
        let ace = &*(p_ace as *const ACCESS_ALLOWED_ACE);
        let mask = ace.Mask;
        let base = p_ace as usize;
        let sid_ptr =
            (base + std::mem::size_of::<ACE_HEADER>() + std::mem::size_of::<u32>()) as *mut c_void;
        let eq = EqualSid(sid_ptr, psid);
        if eq != 0 && (mask & FILE_GENERIC_WRITE) != 0 {
            return true;
        }
    }
    false
}

pub unsafe fn dacl_has_write_deny_for_sid(p_dacl: *mut ACL, psid: *mut c_void) -> bool {
    dacl_has_deny_mask_for_sid(p_dacl, psid, DENY_WRITE_MASK)
}

pub unsafe fn dacl_has_read_deny_for_sid(p_dacl: *mut ACL, psid: *mut c_void) -> bool {
    dacl_has_deny_mask_for_sid(p_dacl, psid, DENY_READ_MASK)
}

/// Returns true only when one applicable deny ACE covers the complete effective mask. Generic
/// rights are mapped before comparison so stale or split partial ACEs cannot suppress repair.
unsafe fn dacl_has_deny_mask_for_sid(
    p_dacl: *mut ACL,
    psid: *mut c_void,
    required_mask: u32,
) -> bool {
    if p_dacl.is_null() {
        return false;
    }
    let mut info: ACL_SIZE_INFORMATION = std::mem::zeroed();
    let ok = GetAclInformation(
        p_dacl as *const ACL,
        &mut info as *mut _ as *mut c_void,
        std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
        AclSizeInformation,
    );
    if ok == 0 {
        return false;
    }
    let required_mask = effective_file_access_mask(required_mask);
    for i in 0..info.AceCount {
        let mut p_ace: *mut c_void = std::ptr::null_mut();
        if GetAce(p_dacl as *const ACL, i, &mut p_ace) == 0 {
            continue;
        }
        let hdr = &*(p_ace as *const ACE_HEADER);
        if hdr.AceType != ACCESS_DENIED_ACE_TYPE {
            continue; // ACCESS_DENIED_ACE_TYPE
        }
        if (hdr.AceFlags & INHERIT_ONLY_ACE) != 0 {
            continue;
        }
        let ace = &*(p_ace as *const ACCESS_DENIED_ACE);
        let base = p_ace as usize;
        let sid_ptr =
            (base + std::mem::size_of::<ACE_HEADER>() + std::mem::size_of::<u32>()) as *mut c_void;
        if EqualSid(sid_ptr, psid) != 0
            && (effective_file_access_mask(ace.Mask) & required_mask) == required_mask
        {
            return true;
        }
    }
    false
}

fn file_generic_mapping() -> GENERIC_MAPPING {
    GENERIC_MAPPING {
        GenericRead: FILE_GENERIC_READ,
        GenericWrite: FILE_GENERIC_WRITE,
        GenericExecute: FILE_GENERIC_EXECUTE,
        GenericAll: FILE_ALL_ACCESS,
    }
}

unsafe fn effective_file_access_mask(mask: u32) -> u32 {
    let mut effective = mask;
    let mapping = file_generic_mapping();
    MapGenericMask(&mut effective, &mapping);
    effective
}

// Grant DELETE on each inheriting descendant instead of FILE_DELETE_CHILD on
// its parent. A parent delete-child grant would bypass a direct deny-write ACE
// on protected children such as `.git` or an explicit read-only subpath.
const WRITE_ALLOW_MASK: u32 =
    FILE_GENERIC_READ | FILE_GENERIC_WRITE | FILE_GENERIC_EXECUTE | DELETE;

unsafe fn ensure_allow_mask_aces_with_inheritance_impl(
    path: &Path,
    sids: &[*mut c_void],
    allow_mask: u32,
    disallow_mask: u32,
    inheritance: u32,
) -> Result<bool> {
    let target = open_acl_target(path, READ_CONTROL | WRITE_DAC, 1)?;
    let mut entries: Vec<EXPLICIT_ACCESS_W> = Vec::new();
    for sid in sids {
        if dacl_mask_allows(
            target.p_dacl,
            &[*sid],
            allow_mask,
            /*require_all_bits*/ true,
        ) && !dacl_mask_allows(
            target.p_dacl,
            &[*sid],
            disallow_mask,
            /*require_all_bits*/ false,
        ) {
            continue;
        }
        entries.push(EXPLICIT_ACCESS_W {
            grfAccessPermissions: allow_mask,
            grfAccessMode: 2, // SET_ACCESS
            grfInheritance: inheritance,
            Trustee: TRUSTEE_W {
                pMultipleTrustee: std::ptr::null_mut(),
                MultipleTrusteeOperation: 0,
                TrusteeForm: TRUSTEE_IS_SID,
                TrusteeType: TRUSTEE_IS_UNKNOWN,
                ptstrName: *sid as *mut u16,
            },
        });
    }
    let mut added = false;
    if !entries.is_empty() {
        let mut p_new_dacl: *mut ACL = std::ptr::null_mut();
        let code2 = SetEntriesInAclW(
            entries.len() as u32,
            entries.as_ptr(),
            target.p_dacl,
            &mut p_new_dacl,
        );
        if code2 != ERROR_SUCCESS {
            return Err(anyhow::Error::new(WindowsAclError::new(
                AclOperation::SetEntriesInAcl,
                code2,
            )));
        }
        let set_result = set_target_dacl(&target, p_new_dacl, 1);
        if !p_new_dacl.is_null() {
            LocalFree(p_new_dacl as HLOCAL);
        }
        set_result?;
        added = true;
    }
    Ok(added)
}

/// Ensure all provided SIDs have an allow ACE with the requested mask on the path.
/// Returns true if any ACE was added.
///
/// # Safety
/// Caller must pass valid SID pointers and an existing path; free the returned security descriptor with `LocalFree`.
pub unsafe fn ensure_allow_mask_aces_with_inheritance(
    path: &Path,
    sids: &[*mut c_void],
    allow_mask: u32,
    inheritance: u32,
) -> Result<bool> {
    ensure_allow_mask_aces_with_inheritance_impl(
        path,
        sids,
        allow_mask,
        /*disallow_mask*/ 0,
        inheritance,
    )
}

/// Ensure all provided SIDs have an allow ACE with the requested mask on the path.
/// Returns true if any ACE was added.
///
/// # Safety
/// Caller must pass valid SID pointers and an existing path; free the returned security descriptor with `LocalFree`.
pub unsafe fn ensure_allow_mask_aces(
    path: &Path,
    sids: &[*mut c_void],
    allow_mask: u32,
) -> Result<bool> {
    ensure_allow_mask_aces_with_inheritance(
        path,
        sids,
        allow_mask,
        CONTAINER_INHERIT_ACE | OBJECT_INHERIT_ACE,
    )
}

/// Ensure all provided SIDs have a write-capable allow ACE on the path.
/// Returns true if any ACE was added.
///
/// # Safety
/// Caller must pass valid SID pointers and an existing path; free the returned security descriptor with `LocalFree`.
pub unsafe fn ensure_allow_write_aces(path: &Path, sids: &[*mut c_void]) -> Result<bool> {
    ensure_allow_mask_aces_with_inheritance_impl(
        path,
        sids,
        WRITE_ALLOW_MASK,
        FILE_DELETE_CHILD,
        CONTAINER_INHERIT_ACE | OBJECT_INHERIT_ACE,
    )
}

/// Adds an allow ACE granting read/write/execute to the given SID on the target path.
///
/// # Safety
/// Caller must ensure `psid` points to a valid SID and `path` refers to an existing file or directory.
pub unsafe fn add_allow_ace(path: &Path, psid: *mut c_void) -> Result<bool> {
    let target = open_acl_target(path, READ_CONTROL | WRITE_DAC, 1)?;
    // Already has write? Skip costly DACL rewrite.
    if dacl_has_write_allow_for_sid(target.p_dacl, psid) {
        return Ok(false);
    }
    // Always ensure write is present: if an allow ACE exists without write, add one with write+RX.
    let trustee = TRUSTEE_W {
        pMultipleTrustee: std::ptr::null_mut(),
        MultipleTrusteeOperation: 0,
        TrusteeForm: TRUSTEE_IS_SID,
        TrusteeType: TRUSTEE_IS_UNKNOWN,
        ptstrName: psid as *mut u16,
    };
    let mut explicit: EXPLICIT_ACCESS_W = std::mem::zeroed();
    explicit.grfAccessPermissions = FILE_GENERIC_READ | FILE_GENERIC_WRITE | FILE_GENERIC_EXECUTE;
    explicit.grfAccessMode = 2; // SET_ACCESS
    explicit.grfInheritance = CONTAINER_INHERIT_ACE | OBJECT_INHERIT_ACE;
    explicit.Trustee = trustee;
    let mut p_new_dacl: *mut ACL = std::ptr::null_mut();
    let code2 = SetEntriesInAclW(1, &explicit, target.p_dacl, &mut p_new_dacl);
    if code2 != ERROR_SUCCESS {
        return Err(anyhow::Error::new(WindowsAclError::new(
            AclOperation::SetEntriesInAcl,
            code2,
        )));
    }
    let set_result = set_target_dacl(&target, p_new_dacl, 1);
    if !p_new_dacl.is_null() {
        LocalFree(p_new_dacl as HLOCAL);
    }
    set_result?;
    Ok(true)
}

/// Adds a deny ACE to prevent write/append/delete for the given SID on the target path.
///
/// # Safety
/// Caller must ensure `psid` points to a valid SID and `path` refers to an existing file or directory.
pub unsafe fn add_deny_write_ace(path: &Path, psid: *mut c_void) -> Result<bool> {
    add_deny_ace(path, psid, DenyAceKind::Write)
}

#[derive(Clone, Copy)]
enum DenyAceKind {
    Read,
    Write,
}

impl DenyAceKind {
    fn mask(self) -> u32 {
        match self {
            Self::Read => DENY_READ_MASK,
            Self::Write => DENY_WRITE_MASK,
        }
    }

    unsafe fn already_present(self, p_dacl: *mut ACL, psid: *mut c_void) -> bool {
        match self {
            Self::Read => dacl_has_read_deny_for_sid(p_dacl, psid),
            Self::Write => dacl_has_write_deny_for_sid(p_dacl, psid),
        }
    }
}

unsafe fn add_deny_ace(path: &Path, psid: *mut c_void, kind: DenyAceKind) -> Result<bool> {
    let target = open_acl_target(path, READ_CONTROL | WRITE_DAC, 1)?;
    if !kind.already_present(target.p_dacl, psid) {
        let trustee = TRUSTEE_W {
            pMultipleTrustee: std::ptr::null_mut(),
            MultipleTrusteeOperation: 0,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_UNKNOWN,
            ptstrName: psid as *mut u16,
        };
        let mut explicit: EXPLICIT_ACCESS_W = std::mem::zeroed();
        explicit.grfAccessPermissions = kind.mask();
        explicit.grfAccessMode = DENY_ACCESS;
        explicit.grfInheritance = CONTAINER_INHERIT_ACE | OBJECT_INHERIT_ACE;
        explicit.Trustee = trustee;
        let mut p_new_dacl: *mut ACL = std::ptr::null_mut();
        let code2 = SetEntriesInAclW(1, &explicit, target.p_dacl, &mut p_new_dacl);
        if code2 != ERROR_SUCCESS {
            return Err(anyhow::Error::new(WindowsAclError::new(
                AclOperation::SetEntriesInAcl,
                code2,
            )));
        }
        let set_result = set_target_dacl(&target, p_new_dacl, 1);
        if !p_new_dacl.is_null() {
            LocalFree(p_new_dacl as HLOCAL);
        }
        set_result?;
        return Ok(true);
    }
    Ok(false)
}

/// Adds a deny ACE to prevent reads for the given SID on the target path.
///
/// `SetEntriesInAclW` places newly-created deny ACEs before allow ACEs, which
/// keeps the resulting DACL in the order Windows expects for denies to win.
/// The ACE is inheritable so a deny applied to a materialized directory also
/// covers files and directories later created underneath it.
///
/// # Safety
/// Caller must ensure `psid` points to a valid SID and `path` refers to an existing file or directory.
pub unsafe fn add_deny_read_ace(path: &Path, psid: *mut c_void) -> Result<bool> {
    add_deny_ace(path, psid, DenyAceKind::Read)
}

pub unsafe fn revoke_ace(path: &Path, psid: *mut c_void) -> Result<()> {
    let target = match open_acl_target(path, READ_CONTROL | WRITE_DAC, 1) {
        Ok(target) => target,
        Err(error) if is_missing_target_error(&error) => return Ok(()),
        Err(error) => return Err(error),
    };
    let trustee = TRUSTEE_W {
        pMultipleTrustee: std::ptr::null_mut(),
        MultipleTrusteeOperation: 0,
        TrusteeForm: TRUSTEE_IS_SID,
        TrusteeType: TRUSTEE_IS_UNKNOWN,
        ptstrName: psid as *mut u16,
    };
    let mut explicit: EXPLICIT_ACCESS_W = std::mem::zeroed();
    explicit.grfAccessPermissions = 0;
    explicit.grfAccessMode = 4; // REVOKE_ACCESS
    explicit.grfInheritance = CONTAINER_INHERIT_ACE | OBJECT_INHERIT_ACE;
    explicit.Trustee = trustee;
    let mut p_new_dacl: *mut ACL = std::ptr::null_mut();
    let code2 = SetEntriesInAclW(1, &explicit, target.p_dacl, &mut p_new_dacl);
    if code2 != ERROR_SUCCESS {
        return Err(anyhow::Error::new(WindowsAclError::new(
            AclOperation::SetEntriesInAcl,
            code2,
        )));
    }
    let set_result = set_target_dacl(&target, p_new_dacl, 1);
    if !p_new_dacl.is_null() {
        LocalFree(p_new_dacl as HLOCAL);
    }
    set_result
}

/// Best-effort grant of RX to the null device for stdout/stderr redirection compatibility.
///
/// # Safety
/// Caller must ensure `psid` is a valid SID pointer.
pub unsafe fn allow_null_device(psid: *mut c_void) {
    let handle = CreateFileW(
        to_wide(r"\\.\NUL").as_ptr(),
        READ_CONTROL | WRITE_DAC,
        FILE_SHARE_READ | FILE_SHARE_WRITE,
        std::ptr::null_mut(),
        OPEN_EXISTING,
        FILE_ATTRIBUTE_NORMAL,
        0,
    );
    if handle == 0 || handle == INVALID_HANDLE_VALUE {
        return;
    }
    let mut security_descriptor: *mut c_void = std::ptr::null_mut();
    let mut dacl: *mut ACL = std::ptr::null_mut();
    let code = GetSecurityInfo(
        handle,
        SE_KERNEL_OBJECT as i32,
        DACL_SECURITY_INFORMATION,
        std::ptr::null_mut(),
        std::ptr::null_mut(),
        &mut dacl,
        std::ptr::null_mut(),
        &mut security_descriptor,
    );
    if code != ERROR_SUCCESS {
        CloseHandle(handle);
        return;
    }
    let trustee = TRUSTEE_W {
        pMultipleTrustee: std::ptr::null_mut(),
        MultipleTrusteeOperation: 0,
        TrusteeForm: TRUSTEE_IS_SID,
        TrusteeType: TRUSTEE_IS_UNKNOWN,
        ptstrName: psid as *mut u16,
    };
    let mut explicit: EXPLICIT_ACCESS_W = std::mem::zeroed();
    explicit.grfAccessPermissions = FILE_GENERIC_READ | FILE_GENERIC_WRITE | FILE_GENERIC_EXECUTE;
    explicit.grfAccessMode = 2; // SET_ACCESS
    explicit.grfInheritance = 0;
    explicit.Trustee = trustee;
    let mut p_new_dacl: *mut ACL = std::ptr::null_mut();
    let set_entries = SetEntriesInAclW(1, &explicit, dacl, &mut p_new_dacl);
    if set_entries == ERROR_SUCCESS {
        let _ = SetSecurityInfo(
            handle,
            SE_KERNEL_OBJECT as i32,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            p_new_dacl,
            std::ptr::null_mut(),
        );
    }
    if !p_new_dacl.is_null() {
        LocalFree(p_new_dacl as HLOCAL);
    }
    if !security_descriptor.is_null() {
        LocalFree(security_descriptor as HLOCAL);
    }
    CloseHandle(handle);
}
const CONTAINER_INHERIT_ACE: u32 = 0x2;
const OBJECT_INHERIT_ACE: u32 = 0x1;

#[cfg(test)]
mod tests {
    use super::AclOperation;
    use super::AclTarget;
    use super::DENY_ACCESS;
    use super::ERROR_SUCCESS;
    use super::SetEntriesInAclW;
    use super::TRUSTEE_IS_SID;
    use super::TRUSTEE_IS_UNKNOWN;
    use super::TRUSTEE_W;
    use super::WindowsAclError;
    use super::add_deny_read_ace;
    use super::add_deny_write_ace;
    use super::dacl_has_read_deny_for_sid;
    use super::dacl_has_write_deny_for_sid;
    use super::fetch_dacl_handle;
    use super::open_acl_target;
    use super::path_contains_reparse_component;
    use super::set_target_dacl;
    use crate::token::LocalSid;
    use std::ffi::c_void;
    use std::fs::OpenOptions;
    use std::os::windows::process::CommandExt;
    use std::path::PathBuf;
    use tempfile::TempDir;
    use windows_sys::Win32::Foundation::HLOCAL;
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Storage::FileSystem::DELETE;
    use windows_sys::Win32::Storage::FileSystem::FILE_READ_DATA;
    use windows_sys::Win32::Storage::FileSystem::READ_CONTROL;
    use windows_sys::Win32::Storage::FileSystem::WRITE_DAC;

    unsafe fn seed_partial_deny(target: &AclTarget, psid: *mut c_void, mask: u32) {
        let trustee = TRUSTEE_W {
            pMultipleTrustee: std::ptr::null_mut(),
            MultipleTrusteeOperation: 0,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_UNKNOWN,
            ptstrName: psid as *mut u16,
        };
        let explicit = super::EXPLICIT_ACCESS_W {
            grfAccessPermissions: mask,
            grfAccessMode: DENY_ACCESS,
            grfInheritance: 0,
            Trustee: trustee,
        };
        let mut new_dacl = std::ptr::null_mut();
        assert_eq!(
            SetEntriesInAclW(1, &explicit, target.p_dacl, &mut new_dacl),
            ERROR_SUCCESS,
            "seed partial deny ACE"
        );
        let result = set_target_dacl(target, new_dacl, 1);
        if !new_dacl.is_null() {
            LocalFree(new_dacl as HLOCAL);
        }
        result.expect("install partial deny ACE");
    }

    unsafe fn fetch_has_deny(path: &std::path::Path, sid: *mut c_void, write: bool) -> bool {
        let (p_dacl, p_sd) = fetch_dacl_handle(path).expect("fetch test DACL");
        let has = if write {
            dacl_has_write_deny_for_sid(p_dacl, sid)
        } else {
            dacl_has_read_deny_for_sid(p_dacl, sid)
        };
        LocalFree(p_sd as HLOCAL);
        has
    }

    #[test]
    fn partial_write_deny_is_completed_and_blocks_write() {
        let tmp = TempDir::new().expect("tempdir");
        let path = tmp.path().join("write-target.txt");
        std::fs::write(&path, b"payload").expect("write fixture");
        let sid = LocalSid::from_string("S-1-5-21-1111111111-2222222222-3333333333-4444")
            .expect("test capability SID");
        let world_sid = LocalSid::from_string("S-1-1-0").expect("world SID");
        let target = unsafe { open_acl_target(&path, READ_CONTROL | WRITE_DAC, 1) }
            .expect("open ACL target");

        unsafe { seed_partial_deny(&target, sid.as_ptr(), DELETE) };
        assert!(!unsafe { fetch_has_deny(&path, sid.as_ptr(), true) });
        assert!(unsafe { add_deny_write_ace(&path, sid.as_ptr()) }.expect("complete write deny"));
        assert!(unsafe { fetch_has_deny(&path, sid.as_ptr(), true) });
        assert!(
            unsafe { add_deny_write_ace(&path, world_sid.as_ptr()) }
                .expect("install observable write deny")
        );
        let write_blocked = OpenOptions::new().write(true).open(&path).is_err();

        unsafe { set_target_dacl(&target, target.p_dacl, 1).expect("restore original DACL") };
        assert!(write_blocked, "complete deny ACE must block writes");
    }

    #[test]
    fn partial_read_deny_is_completed_and_blocks_read() {
        let tmp = TempDir::new().expect("tempdir");
        let path = tmp.path().join("read-target.txt");
        std::fs::write(&path, b"payload").expect("write fixture");
        let sid = LocalSid::from_string("S-1-5-21-1111111111-2222222222-3333333333-4444")
            .expect("test capability SID");
        let world_sid = LocalSid::from_string("S-1-1-0").expect("world SID");
        let target = unsafe { open_acl_target(&path, READ_CONTROL | WRITE_DAC, 1) }
            .expect("open ACL target");

        unsafe { seed_partial_deny(&target, sid.as_ptr(), FILE_READ_DATA) };
        assert!(!unsafe { fetch_has_deny(&path, sid.as_ptr(), false) });
        assert!(unsafe { add_deny_read_ace(&path, sid.as_ptr()) }.expect("complete read deny"));
        assert!(unsafe { fetch_has_deny(&path, sid.as_ptr(), false) });
        assert!(
            unsafe { add_deny_read_ace(&path, world_sid.as_ptr()) }
                .expect("install observable read deny")
        );
        let read_blocked = std::fs::read(&path).is_err();

        unsafe { set_target_dacl(&target, target.p_dacl, 1).expect("restore original DACL") };
        assert!(read_blocked, "complete deny ACE must block reads");
    }

    #[test]
    fn reparse_acl_target_returns_typed_unsupported_error() {
        let tmp = TempDir::new().expect("tempdir");
        let target = tmp.path().join("target");
        let alias = tmp.path().join("alias");
        std::fs::create_dir(&target).expect("create target");
        let link = format!("\"{}\"", alias.display());
        let target_arg = format!("\"{}\"", target.display());
        let junction_created = std::process::Command::new("cmd.exe")
            .raw_arg("/c")
            .raw_arg("mklink")
            .raw_arg("/J")
            .raw_arg(&link)
            .raw_arg(&target_arg)
            .output()
            .is_ok_and(|output| output.status.success() && alias.exists());
        assert!(junction_created, "junction fixture must be available");

        let error = unsafe { fetch_dacl_handle(&alias) }.expect_err("reparse target rejected");
        let typed = error
            .downcast_ref::<WindowsAclError>()
            .expect("typed Windows ACL error");
        assert_eq!(typed.operation, AclOperation::ReparseTargetUnsupported);
        assert_eq!(typed.code, 50);
    }

    #[test]
    fn verbatim_disk_prefix_is_not_probed_as_a_standalone_path() {
        let tmp = TempDir::new().expect("tempdir");
        let ordinary = tmp.path().join("ordinary.txt");
        std::fs::write(&ordinary, b"payload").expect("write fixture");
        let verbatim = PathBuf::from(format!(r"\\?\{}", ordinary.display()));

        assert!(
            !path_contains_reparse_component(&verbatim).expect("inspect verbatim path"),
            "ordinary file must not be treated as a reparse target"
        );
    }
}
