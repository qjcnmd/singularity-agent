use crate::path_safety::ensure_case_insensitive_directory;
use crate::path_safety::open_existing_acl_target;
use crate::winutil::to_wide;
use anyhow::Result;
use serde::Deserialize;
use serde::Serialize;
use std::error::Error;
use std::ffi::c_void;
use std::fmt;
use std::os::windows::io::{AsRawHandle, IntoRawHandle};
use std::path::Path;
#[cfg(test)]
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
use windows_sys::Win32::Security::DeleteAce;
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
use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_DIRECTORY;
use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_NORMAL;
use windows_sys::Win32::Storage::FileSystem::FILE_DELETE_CHILD;
use windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_EXECUTE;
use windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_READ;
use windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_WRITE;
use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;
use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_WRITE;
use windows_sys::Win32::Storage::FileSystem::FILE_WRITE_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::FILE_WRITE_DATA;
use windows_sys::Win32::Storage::FileSystem::FILE_WRITE_EA;
use windows_sys::Win32::Storage::FileSystem::OPEN_EXISTING;
use windows_sys::Win32::Storage::FileSystem::READ_CONTROL;
use windows_sys::Win32::Storage::FileSystem::WRITE_DAC;
const SE_KERNEL_OBJECT: u32 = 6;
const INHERIT_ONLY_ACE: u8 = 0x08;
const NO_PROPAGATE_INHERIT_ACE: u8 = 0x04;
const INHERITED_ACE: u8 = 0x10;
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

/// Sorted deny-ACE state for one SID on one ACL target.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct DenyReadAclFingerprint {
    entries: Vec<DenyAceFingerprintEntry>,
}

impl DenyReadAclFingerprint {
    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
struct DenyAceFingerprintEntry {
    flags: u8,
    mask: u32,
}

/// The Win32 operation that failed while reading or writing a DACL.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AclOperation {
    OpenTarget,
    ReparseTargetUnsupported,
    QueryTargetIdentity,
    GetSecurityInfo,
    GetAclInformation,
    GetAce,
    DeleteAce,
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
    is_directory: bool,
    owns_handle: bool,
}

impl Drop for AclTarget {
    fn drop(&mut self) {
        unsafe {
            if !self.p_sd.is_null() {
                LocalFree(self.p_sd as HLOCAL);
            }
            if self.owns_handle && self.handle != 0 && self.handle != INVALID_HANDLE_VALUE {
                CloseHandle(self.handle);
            }
        }
    }
}

unsafe fn open_acl_target(path: &Path, desired_access: u32, object_type: i32) -> Result<AclTarget> {
    if object_type != 1 {
        return Err(anyhow::Error::new(WindowsAclError::new(
            AclOperation::OpenTarget,
            ERROR_INVALID_DATA,
        )));
    }
    let file = match open_existing_acl_target(path, desired_access) {
        Ok(file) => file,
        Err(error) => {
            let native_code = error.chain().find_map(|cause| {
                cause
                    .downcast_ref::<std::io::Error>()
                    .and_then(std::io::Error::raw_os_error)
                    .map(|code| code as u32)
            });
            if let Some(code) = native_code {
                return Err(anyhow::Error::new(WindowsAclError::new(
                    AclOperation::OpenTarget,
                    code,
                ))
                .context(error));
            }
            return Err(error);
        }
    };
    let handle = file.as_raw_handle() as HANDLE;
    // SAFETY: `handle` is owned by `file` and remains valid until it is transferred below;
    // Windows initializes the plain output structure through the valid mutable pointer.
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    let info_ok = unsafe {
        windows_sys::Win32::Storage::FileSystem::GetFileInformationByHandle(handle, &mut info)
    };
    if info_ok == 0 {
        let code = unsafe { GetLastError() };
        return Err(anyhow::Error::new(WindowsAclError::new(
            AclOperation::QueryTargetIdentity,
            code,
        )));
    }
    if info.dwFileAttributes & 0x0000_0400 != 0 {
        return Err(anyhow::Error::new(WindowsAclError::new(
            AclOperation::ReparseTargetUnsupported,
            50, // ERROR_NOT_SUPPORTED
        )));
    }
    let is_directory = info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0;
    if is_directory {
        ensure_case_insensitive_directory(&file, path)?;
    }
    let handle = file.into_raw_handle() as HANDLE;
    let mut p_sd: *mut c_void = std::ptr::null_mut();
    let mut p_dacl: *mut ACL = std::ptr::null_mut();
    // SAFETY: `handle` is a live file handle and the output pointers refer to local storage;
    // Windows allocates the returned security descriptor for the matching `LocalFree` drop.
    let code = unsafe {
        GetSecurityInfo(
            handle,
            object_type,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut p_dacl,
            std::ptr::null_mut(),
            &mut p_sd,
        )
    };
    if code != ERROR_SUCCESS {
        unsafe {
            CloseHandle(handle);
        }
        return Err(anyhow::Error::new(WindowsAclError::new(
            AclOperation::GetSecurityInfo,
            code,
        )));
    }
    Ok(AclTarget {
        handle,
        p_dacl,
        p_sd,
        is_directory,
        owns_handle: true,
    })
}

/// Borrows an already pinned directory handle for one ACL operation.
///
/// The caller retains handle ownership and must have opened it with the access required by the
/// requested operation.
unsafe fn borrow_acl_directory(handle: HANDLE) -> Result<AclTarget> {
    // SAFETY: `handle` is a caller-owned directory handle, and the Win32 output structure is
    // initialized through a valid mutable pointer.
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    if unsafe {
        windows_sys::Win32::Storage::FileSystem::GetFileInformationByHandle(handle, &mut info)
    } == 0
    {
        return Err(anyhow::Error::new(WindowsAclError::new(
            AclOperation::QueryTargetIdentity,
            unsafe { GetLastError() },
        )));
    }
    if info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY == 0
        || info.dwFileAttributes & 0x0000_0400 != 0
    {
        return Err(anyhow::Error::new(WindowsAclError::new(
            AclOperation::ReparseTargetUnsupported,
            50, // ERROR_NOT_SUPPORTED
        )));
    }
    let mut p_sd: *mut c_void = std::ptr::null_mut();
    let mut p_dacl: *mut ACL = std::ptr::null_mut();
    let code = unsafe {
        GetSecurityInfo(
            handle,
            1,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut p_dacl,
            std::ptr::null_mut(),
            &mut p_sd,
        )
    };
    if code != ERROR_SUCCESS {
        return Err(anyhow::Error::new(WindowsAclError::new(
            AclOperation::GetSecurityInfo,
            code,
        )));
    }
    Ok(AclTarget {
        handle,
        p_dacl,
        p_sd,
        is_directory: true,
        owns_handle: false,
    })
}

#[cfg(test)]
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
    // SAFETY: the caller keeps `target.handle` and `p_dacl` valid for this synchronous Win32
    // call; the target DACL pointer is owned by the surrounding ACL operation.
    let code = unsafe {
        SetSecurityInfo(
            target.handle,
            object_type,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            p_dacl,
            std::ptr::null_mut(),
        )
    };
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
    // SAFETY: the public safety contract supplies a valid DACL and existing non-reparse path;
    // the target handle and DACL remain valid for the synchronous replacement.
    let target = unsafe { open_acl_target(path, READ_CONTROL | WRITE_DAC, 1) }?;
    unsafe { set_target_dacl(&target, p_dacl, 1) }
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
    // SAFETY: the public safety contract supplies an existing path and transfers the returned
    // security descriptor ownership to the caller.
    let mut target = unsafe { open_acl_target(path, READ_CONTROL, 1) }?;
    let p_dacl = target.p_dacl;
    let p_sd = target.p_sd;
    let handle = target.handle;
    target.p_sd = std::ptr::null_mut();
    target.handle = 0;
    unsafe {
        CloseHandle(handle);
    }
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
    // SAFETY: the caller guarantees a live DACL and valid SID pointers. `GetAce` is checked
    // before each ACE is interpreted, and the returned ACE layout is the Win32 ACL contract.
    unsafe {
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
            let sid_ptr = (base + std::mem::size_of::<ACE_HEADER>() + std::mem::size_of::<u32>())
                as *mut c_void;
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
    // SAFETY: the caller guarantees a live DACL and SID pointer. Each ACE is obtained through
    // the checked Win32 accessor before its header, mask, and SID are read.
    unsafe {
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
            let sid_ptr = (base + std::mem::size_of::<ACE_HEADER>() + std::mem::size_of::<u32>())
                as *mut c_void;
            let eq = EqualSid(sid_ptr, psid);
            if eq != 0 && (mask & FILE_GENERIC_WRITE) != 0 {
                return true;
            }
        }
        false
    }
}

#[cfg(test)]
pub unsafe fn dacl_has_write_deny_for_sid(p_dacl: *mut ACL, psid: *mut c_void) -> bool {
    unsafe { dacl_has_deny_mask_for_sid(p_dacl, psid, DENY_WRITE_MASK, false) }
        .expect("inspect write deny DACL")
}

#[cfg(test)]
pub unsafe fn dacl_has_read_deny_for_sid(p_dacl: *mut ACL, psid: *mut c_void) -> bool {
    unsafe { dacl_has_deny_mask_for_sid(p_dacl, psid, DENY_READ_MASK, false) }
        .expect("inspect read deny DACL")
}

unsafe fn deny_aces_for_sid(
    p_dacl: *mut ACL,
    psid: *mut c_void,
) -> Result<Vec<DenyAceFingerprintEntry>> {
    if p_dacl.is_null() {
        return Ok(Vec::new());
    }
    // SAFETY: the caller guarantees a live DACL and SID pointer. Every ACE is read only after
    // `GetAce` succeeds, and Win32 supplies the ACL-owned memory for the duration of the call.
    let mut entries = unsafe {
        let mut info: ACL_SIZE_INFORMATION = std::mem::zeroed();
        if GetAclInformation(
            p_dacl as *const ACL,
            &mut info as *mut _ as *mut c_void,
            std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
            AclSizeInformation,
        ) == 0
        {
            return Err(anyhow::Error::new(WindowsAclError::new(
                AclOperation::GetAclInformation,
                GetLastError(),
            )));
        }
        let mut entries = Vec::new();
        for index in 0..info.AceCount {
            let mut p_ace = std::ptr::null_mut();
            if GetAce(p_dacl as *const ACL, index, &mut p_ace) == 0 {
                return Err(anyhow::Error::new(WindowsAclError::new(
                    AclOperation::GetAce,
                    GetLastError(),
                )));
            }
            let header = &*(p_ace as *const ACE_HEADER);
            if header.AceType != ACCESS_DENIED_ACE_TYPE {
                continue;
            }
            let sid_ptr = (p_ace as usize
                + std::mem::size_of::<ACE_HEADER>()
                + std::mem::size_of::<u32>()) as *mut c_void;
            if EqualSid(sid_ptr, psid) != 0 {
                let ace = &*(p_ace as *const ACCESS_DENIED_ACE);
                entries.push(DenyAceFingerprintEntry {
                    flags: header.AceFlags,
                    mask: ace.Mask,
                });
            }
        }
        entries
    };
    entries.sort();
    Ok(entries)
}

/// Returns true only when the DACL covers the complete effective mask on the target and, when
/// requested for a directory, propagates it to both child files and directories. Win32 may encode
/// those two responsibilities as separate explicit ACEs.
#[cfg(test)]
unsafe fn dacl_has_deny_mask_for_sid(
    p_dacl: *mut ACL,
    psid: *mut c_void,
    required_mask: u32,
    require_descendant_inheritance: bool,
) -> Result<bool> {
    let entries = unsafe { deny_aces_for_sid(p_dacl, psid) }?;
    Ok(deny_entries_cover_mask(
        &entries,
        required_mask,
        require_descendant_inheritance,
    ))
}

fn deny_entries_cover_mask(
    entries: &[DenyAceFingerprintEntry],
    required_mask: u32,
    require_descendant_inheritance: bool,
) -> bool {
    let required_mask = effective_file_access_mask(required_mask);
    let mut protects_target = false;
    let mut protects_descendants = false;
    for entry in entries {
        if (effective_file_access_mask(entry.mask) & required_mask) == required_mask {
            protects_target |= entry.flags & INHERIT_ONLY_ACE == 0;
            protects_descendants |= deny_ace_protects_descendants(entry.flags);
        }
    }
    protects_target && (!require_descendant_inheritance || protects_descendants)
}

/// Captures every deny ACE for `psid` on the current target DACL.
pub(crate) unsafe fn deny_read_acl_fingerprint(
    path: &Path,
    psid: *mut c_void,
) -> Result<DenyReadAclFingerprint> {
    let target = unsafe { open_acl_target(path, READ_CONTROL, 1) }?;
    Ok(DenyReadAclFingerprint {
        entries: unsafe { deny_aces_for_sid(target.p_dacl, psid) }?,
    })
}

fn deny_ace_protects_descendants(flags: u8) -> bool {
    let required = (CONTAINER_INHERIT_ACE | OBJECT_INHERIT_ACE) as u8;
    flags & required == required && flags & NO_PROPAGATE_INHERIT_ACE == 0
}

fn is_runtime_managed_deny_ace(flags: u8) -> bool {
    let required = (CONTAINER_INHERIT_ACE | OBJECT_INHERIT_ACE) as u8;
    let inheritance_flags = required | NO_PROPAGATE_INHERIT_ACE | INHERIT_ONLY_ACE | INHERITED_ACE;
    let shape = flags & inheritance_flags;
    shape == 0 || shape == required | INHERIT_ONLY_ACE
}

fn file_generic_mapping() -> GENERIC_MAPPING {
    GENERIC_MAPPING {
        GenericRead: FILE_GENERIC_READ,
        GenericWrite: FILE_GENERIC_WRITE,
        GenericExecute: FILE_GENERIC_EXECUTE,
        GenericAll: FILE_ALL_ACCESS,
    }
}

fn effective_file_access_mask(mask: u32) -> u32 {
    let mut effective = mask;
    let mapping = file_generic_mapping();
    unsafe {
        MapGenericMask(&mut effective, &mapping);
    }
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
    // SAFETY: the caller supplies valid SID pointers and an existing non-reparse path; the
    // target handle and DACL remain live for the complete ACL update.
    let target = unsafe { open_acl_target(path, READ_CONTROL | WRITE_DAC, 1) }?;
    let mut entries: Vec<EXPLICIT_ACCESS_W> = Vec::new();
    for sid in sids {
        let allows = unsafe {
            dacl_mask_allows(
                target.p_dacl,
                &[*sid],
                allow_mask,
                /*require_all_bits*/ true,
            )
        };
        let disallowed = unsafe {
            dacl_mask_allows(
                target.p_dacl,
                &[*sid],
                disallow_mask,
                /*require_all_bits*/ false,
            )
        };
        if allows && !disallowed {
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
        let code2 = unsafe {
            SetEntriesInAclW(
                entries.len() as u32,
                entries.as_ptr(),
                target.p_dacl,
                &mut p_new_dacl,
            )
        };
        if code2 != ERROR_SUCCESS {
            return Err(anyhow::Error::new(WindowsAclError::new(
                AclOperation::SetEntriesInAcl,
                code2,
            )));
        }
        let set_result = unsafe { set_target_dacl(&target, p_new_dacl, 1) };
        if !p_new_dacl.is_null() {
            unsafe {
                LocalFree(p_new_dacl as HLOCAL);
            }
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
    unsafe {
        ensure_allow_mask_aces_with_inheritance_impl(
            path,
            sids,
            allow_mask,
            /*disallow_mask*/ 0,
            inheritance,
        )
    }
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
    unsafe {
        ensure_allow_mask_aces_with_inheritance(
            path,
            sids,
            allow_mask,
            CONTAINER_INHERIT_ACE | OBJECT_INHERIT_ACE,
        )
    }
}

/// Ensure all provided SIDs have a write-capable allow ACE on the path.
/// Returns true if any ACE was added.
///
/// # Safety
/// Caller must pass valid SID pointers and an existing path; free the returned security descriptor with `LocalFree`.
pub unsafe fn ensure_allow_write_aces(path: &Path, sids: &[*mut c_void]) -> Result<bool> {
    unsafe {
        ensure_allow_mask_aces_with_inheritance_impl(
            path,
            sids,
            WRITE_ALLOW_MASK,
            FILE_DELETE_CHILD,
            CONTAINER_INHERIT_ACE | OBJECT_INHERIT_ACE,
        )
    }
}

/// Adds an allow ACE granting read/write/execute to the given SID on the target path.
///
/// # Safety
/// Caller must ensure `psid` points to a valid SID and `path` refers to an existing file or directory.
pub unsafe fn add_allow_ace(path: &Path, psid: *mut c_void) -> Result<bool> {
    // SAFETY: the caller supplies a valid SID pointer and an existing non-reparse path.
    let target = unsafe { open_acl_target(path, READ_CONTROL | WRITE_DAC, 1) }?;
    // Already has write? Skip costly DACL rewrite.
    if unsafe { dacl_has_write_allow_for_sid(target.p_dacl, psid) } {
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
    let mut explicit: EXPLICIT_ACCESS_W = unsafe { std::mem::zeroed() };
    explicit.grfAccessPermissions = FILE_GENERIC_READ | FILE_GENERIC_WRITE | FILE_GENERIC_EXECUTE;
    explicit.grfAccessMode = 2; // SET_ACCESS
    explicit.grfInheritance = CONTAINER_INHERIT_ACE | OBJECT_INHERIT_ACE;
    explicit.Trustee = trustee;
    let mut p_new_dacl: *mut ACL = std::ptr::null_mut();
    let code2 = unsafe { SetEntriesInAclW(1, &explicit, target.p_dacl, &mut p_new_dacl) };
    if code2 != ERROR_SUCCESS {
        return Err(anyhow::Error::new(WindowsAclError::new(
            AclOperation::SetEntriesInAcl,
            code2,
        )));
    }
    let set_result = unsafe { set_target_dacl(&target, p_new_dacl, 1) };
    if !p_new_dacl.is_null() {
        unsafe {
            LocalFree(p_new_dacl as HLOCAL);
        }
    }
    set_result?;
    Ok(true)
}

/// Adds a deny ACE to prevent write/append/delete for the given SID on the target path.
///
/// # Safety
/// Caller must ensure `psid` points to a valid SID and `path` refers to an existing file or directory.
pub unsafe fn add_deny_write_ace(path: &Path, psid: *mut c_void) -> Result<bool> {
    Ok(unsafe { add_deny_ace(path, psid, DenyAceKind::Write) }?.added)
}

pub(crate) struct DenyAceAddResult {
    pub(crate) added: bool,
    pub(crate) runtime_owned: bool,
    pub(crate) fingerprint: Option<DenyReadAclFingerprint>,
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

    fn already_present(self, entries: &[DenyAceFingerprintEntry], is_directory: bool) -> bool {
        deny_entries_cover_mask(entries, self.mask(), is_directory)
    }
}

fn runtime_owned_read_fingerprint(is_directory: bool) -> DenyReadAclFingerprint {
    let mut entries = vec![DenyAceFingerprintEntry {
        flags: 0,
        mask: effective_file_access_mask(DENY_READ_MASK),
    }];
    if is_directory {
        entries.push(DenyAceFingerprintEntry {
            flags: (CONTAINER_INHERIT_ACE | OBJECT_INHERIT_ACE | u32::from(INHERIT_ONLY_ACE)) as u8,
            mask: DENY_READ_MASK,
        });
    }
    entries.sort();
    DenyReadAclFingerprint { entries }
}

type BeforeDenyReadSet<'a> = Option<&'a mut dyn FnMut(&DenyReadAclFingerprint) -> Result<()>>;

unsafe fn add_deny_ace(
    path: &Path,
    psid: *mut c_void,
    kind: DenyAceKind,
) -> Result<DenyAceAddResult> {
    // SAFETY: the caller supplies a valid SID pointer and an existing non-reparse path.
    let target = unsafe { open_acl_target(path, READ_CONTROL | WRITE_DAC, 1) }?;
    unsafe { add_deny_ace_to_target(target, psid, kind, None) }
}

unsafe fn add_deny_ace_to_handle(
    handle: HANDLE,
    psid: *mut c_void,
    kind: DenyAceKind,
) -> Result<DenyAceAddResult> {
    let target = unsafe { borrow_acl_directory(handle) }?;
    unsafe { add_deny_ace_to_target(target, psid, kind, None) }
}

unsafe fn add_deny_ace_to_target(
    target: AclTarget,
    psid: *mut c_void,
    kind: DenyAceKind,
    mut before_set: BeforeDenyReadSet<'_>,
) -> Result<DenyAceAddResult> {
    let existing_entries = unsafe { deny_aces_for_sid(target.p_dacl, psid) }?;
    let had_any_deny = !existing_entries.is_empty();
    if !kind.already_present(&existing_entries, target.is_directory) {
        let trustee = TRUSTEE_W {
            pMultipleTrustee: std::ptr::null_mut(),
            MultipleTrusteeOperation: 0,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_UNKNOWN,
            ptstrName: psid as *mut u16,
        };
        let mut explicit: EXPLICIT_ACCESS_W = unsafe { std::mem::zeroed() };
        explicit.grfAccessPermissions = kind.mask();
        explicit.grfAccessMode = DENY_ACCESS;
        explicit.grfInheritance = CONTAINER_INHERIT_ACE | OBJECT_INHERIT_ACE;
        explicit.Trustee = trustee;
        let mut p_new_dacl: *mut ACL = std::ptr::null_mut();
        let code2 = unsafe { SetEntriesInAclW(1, &explicit, target.p_dacl, &mut p_new_dacl) };
        if code2 != ERROR_SUCCESS {
            return Err(anyhow::Error::new(WindowsAclError::new(
                AclOperation::SetEntriesInAcl,
                code2,
            )));
        }
        let fingerprint = (!had_any_deny && matches!(kind, DenyAceKind::Read))
            .then(|| runtime_owned_read_fingerprint(target.is_directory));
        let before_set_result = match (before_set.as_mut(), fingerprint.as_ref()) {
            (Some(before_set), Some(fingerprint)) => before_set(fingerprint),
            _ => Ok(()),
        };
        if let Err(error) = before_set_result {
            if !p_new_dacl.is_null() {
                unsafe {
                    LocalFree(p_new_dacl as HLOCAL);
                }
            }
            return Err(error);
        }
        let set_result = unsafe { set_target_dacl(&target, p_new_dacl, 1) };
        if !p_new_dacl.is_null() {
            unsafe {
                LocalFree(p_new_dacl as HLOCAL);
            }
        }
        set_result?;
        return Ok(DenyAceAddResult {
            added: true,
            runtime_owned: !had_any_deny,
            fingerprint,
        });
    }
    Ok(DenyAceAddResult {
        added: false,
        runtime_owned: false,
        fingerprint: None,
    })
}

/// Adds a deny-write ACE through an already pinned directory handle.
///
/// # Safety
/// Caller must ensure `handle` is a valid directory handle with `READ_CONTROL | WRITE_DAC` and
/// `psid` points to a valid SID.
pub(crate) unsafe fn add_deny_write_ace_to_handle(
    handle: HANDLE,
    psid: *mut c_void,
) -> Result<bool> {
    Ok(unsafe { add_deny_ace_to_handle(handle, psid, DenyAceKind::Write) }?.added)
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
    Ok(unsafe { add_deny_ace(path, psid, DenyAceKind::Read) }?.added)
}

/// Adds a deny-read ACE and reports whether this runtime exclusively owns the resulting repair.
///
/// A repair applied alongside any pre-existing deny ACE for the same SID remains enforced but is
/// conservatively not claimed as revocable runtime state.
/// Adds a deny-read ACE after persisting its expected ownership fingerprint.
pub(crate) unsafe fn add_deny_read_ace_with_ownership_before_set(
    path: &Path,
    psid: *mut c_void,
    before_set: &mut dyn FnMut(&DenyReadAclFingerprint) -> Result<()>,
) -> Result<DenyAceAddResult> {
    let target = unsafe { open_acl_target(path, READ_CONTROL | WRITE_DAC, 1) }?;
    unsafe { add_deny_ace_to_target(target, psid, DenyAceKind::Read, Some(before_set)) }
}

/// Adds a deny-read ACE through a pinned handle after persisting its expected fingerprint.
///
/// # Safety
/// Caller must ensure `handle` is a valid directory handle with `READ_CONTROL | WRITE_DAC` and
/// `psid` points to a valid SID.
pub(crate) unsafe fn add_deny_read_ace_with_ownership_to_handle_before_set(
    handle: HANDLE,
    psid: *mut c_void,
    before_set: &mut dyn FnMut(&DenyReadAclFingerprint) -> Result<()>,
) -> Result<DenyAceAddResult> {
    let target = unsafe { borrow_acl_directory(handle) }?;
    unsafe { add_deny_ace_to_target(target, psid, DenyAceKind::Read, Some(before_set)) }
}

/// Removes the exact explicit deny-read ACE pair installed for a sandbox principal.
///
/// Win32 `REVOKE_ACCESS` removes allow/audit entries but does not reliably remove deny ACEs.
/// Rebuild the existing ACL in memory and delete only the exact current-object and inherit-only
/// propagation entries owned by this runtime. A combined deny entry is rejected rather than
/// partially weakening another boundary.
#[cfg(test)]
pub(crate) unsafe fn revoke_deny_read_ace(path: &Path, psid: *mut c_void) -> Result<()> {
    unsafe { revoke_deny_read_ace_impl(path, psid, None) }
}

/// Revokes runtime-shaped read denies only when the complete SID fingerprint is unchanged.
pub(crate) unsafe fn revoke_deny_read_ace_with_fingerprint(
    path: &Path,
    psid: *mut c_void,
    expected: &DenyReadAclFingerprint,
) -> Result<()> {
    unsafe { revoke_deny_read_ace_impl(path, psid, Some(expected)) }
}

unsafe fn revoke_deny_read_ace_impl(
    path: &Path,
    psid: *mut c_void,
    expected: Option<&DenyReadAclFingerprint>,
) -> Result<()> {
    let target = match unsafe { open_acl_target(path, READ_CONTROL | WRITE_DAC, 1) } {
        Ok(target) => target,
        Err(error) if is_missing_target_error(&error) => return Ok(()),
        Err(error) => return Err(error),
    };
    if target.p_dacl.is_null() {
        if expected.is_some_and(|fingerprint| !fingerprint.entries.is_empty()) {
            anyhow::bail!(
                "refusing to revoke deny-read ACL after ownership fingerprint changed for {}",
                path.display()
            );
        }
        return Ok(());
    }
    if let Some(expected) = expected {
        let current = DenyReadAclFingerprint {
            entries: unsafe { deny_aces_for_sid(target.p_dacl, psid) }?,
        };
        if &current != expected {
            anyhow::bail!(
                "refusing to revoke deny-read ACL after ownership fingerprint changed for {}",
                path.display()
            );
        }
    }
    // SAFETY: `target.p_dacl` is the ACL returned for the live target handle. The copied ACL is
    // sized from Win32's metadata, each ACE lookup is checked, and `psid` is caller-validated.
    unsafe {
        let mut info: ACL_SIZE_INFORMATION = std::mem::zeroed();
        if GetAclInformation(
            target.p_dacl as *const ACL,
            &mut info as *mut _ as *mut c_void,
            std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
            AclSizeInformation,
        ) == 0
        {
            return Err(anyhow::Error::new(WindowsAclError::new(
                AclOperation::GetAclInformation,
                GetLastError(),
            )));
        }
        let acl_size = info.AclBytesInUse.saturating_add(info.AclBytesFree) as usize;
        let word_count = acl_size.div_ceil(std::mem::size_of::<u32>());
        let mut acl_words = vec![0u32; word_count];
        std::ptr::copy_nonoverlapping(
            target.p_dacl as *const u8,
            acl_words.as_mut_ptr() as *mut u8,
            acl_size,
        );
        let copied_acl = acl_words.as_mut_ptr() as *mut ACL;
        let mut changed = false;
        for index in (0..info.AceCount).rev() {
            let mut p_ace: *mut c_void = std::ptr::null_mut();
            if GetAce(copied_acl as *const ACL, index, &mut p_ace) == 0 {
                return Err(anyhow::Error::new(WindowsAclError::new(
                    AclOperation::GetAce,
                    GetLastError(),
                )));
            }
            let header = &*(p_ace as *const ACE_HEADER);
            if header.AceType != ACCESS_DENIED_ACE_TYPE {
                continue;
            }
            let ace = &*(p_ace as *const ACCESS_DENIED_ACE);
            let sid_ptr = (p_ace as usize
                + std::mem::size_of::<ACE_HEADER>()
                + std::mem::size_of::<u32>()) as *mut c_void;
            if EqualSid(sid_ptr, psid) == 0 {
                continue;
            }
            if !is_runtime_managed_deny_ace(header.AceFlags) {
                continue;
            }
            let effective_mask = effective_file_access_mask(ace.Mask);
            let effective_read_mask = effective_file_access_mask(DENY_READ_MASK);
            if effective_mask != effective_read_mask {
                if (effective_mask & effective_read_mask) == effective_read_mask {
                    anyhow::bail!(
                        "refusing to partially revoke a combined deny ACE for {}",
                        path.display()
                    );
                }
                continue;
            }
            if DeleteAce(copied_acl, index) == 0 {
                return Err(anyhow::Error::new(WindowsAclError::new(
                    AclOperation::DeleteAce,
                    GetLastError(),
                )));
            }
            changed = true;
        }
        if changed {
            set_target_dacl(&target, copied_acl, 1)?;
        }
        Ok(())
    }
}

/// Best-effort grant of RX to the null device for stdout/stderr redirection compatibility.
///
/// # Safety
/// Caller must ensure `psid` is a valid SID pointer.
pub unsafe fn allow_null_device(psid: *mut c_void) {
    // SAFETY: the caller guarantees a valid SID; the NUL handle and Win32 ACL buffers are used
    // only during this synchronous best-effort compatibility grant.
    unsafe {
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
        explicit.grfAccessPermissions =
            FILE_GENERIC_READ | FILE_GENERIC_WRITE | FILE_GENERIC_EXECUTE;
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
    use super::add_allow_ace;
    use super::add_deny_read_ace;
    use super::add_deny_write_ace;
    use super::dacl_has_read_deny_for_sid;
    use super::dacl_has_write_deny_for_sid;
    use super::deny_aces_for_sid;
    use super::fetch_dacl_handle;
    use super::open_acl_target;
    use super::path_contains_reparse_component;
    use super::revoke_deny_read_ace;
    use super::set_target_dacl;
    use crate::path_safety::CaseSensitivityTestOutcome;
    use crate::path_safety::ProtectedMetadataError;
    use crate::path_safety::override_case_sensitivity_for_test;
    use crate::token::LocalSid;
    use std::ffi::c_void;
    use std::fs::OpenOptions;
    use std::os::windows::process::CommandExt;
    use std::path::PathBuf;
    use tempfile::TempDir;
    use windows_sys::Win32::Foundation::HLOCAL;
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::ACL;
    use windows_sys::Win32::Security::InitializeAcl;
    use windows_sys::Win32::Storage::FileSystem::DELETE;
    use windows_sys::Win32::Storage::FileSystem::FILE_READ_DATA;
    use windows_sys::Win32::Storage::FileSystem::READ_CONTROL;
    use windows_sys::Win32::Storage::FileSystem::WRITE_DAC;

    unsafe fn seed_deny(target: &AclTarget, psid: *mut c_void, mask: u32, inheritance: u32) {
        // SAFETY: the test fixture owns a live target and SID; all returned ACL allocations are
        // released after the replacement attempt.
        unsafe {
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
                grfInheritance: inheritance,
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
    }

    unsafe fn seed_partial_deny(target: &AclTarget, psid: *mut c_void, mask: u32) {
        unsafe { seed_deny(target, psid, mask, 0) };
    }

    unsafe fn fetch_has_deny(path: &std::path::Path, sid: *mut c_void, write: bool) -> bool {
        // SAFETY: the test path and SID remain valid while the fetched DACL is inspected; the
        // returned security descriptor is released before returning.
        unsafe {
            let (p_dacl, p_sd) = fetch_dacl_handle(path).expect("fetch test DACL");
            let has = if write {
                dacl_has_write_deny_for_sid(p_dacl, sid)
            } else {
                dacl_has_read_deny_for_sid(p_dacl, sid)
            };
            LocalFree(p_sd as HLOCAL);
            has
        }
    }

    #[test]
    fn malformed_acl_query_returns_typed_error_instead_of_absence() {
        let sid = LocalSid::from_string("S-1-5-21-1111111111-2222222222-3333333333-4444")
            .expect("test SID");
        let mut storage = [0_u32; 16];
        let acl = storage.as_mut_ptr() as *mut ACL;
        assert_ne!(
            unsafe { InitializeAcl(acl, std::mem::size_of_val(&storage) as u32, 2) },
            0,
            "initialize malformed ACL fixture"
        );
        unsafe {
            (*acl).AclRevision = u8::MAX;
            (*acl).AclSize = 0;
        }

        let error = unsafe { deny_aces_for_sid(acl, sid.as_ptr()) }
            .expect_err("ACL query must fail closed");
        let typed = error
            .downcast_ref::<WindowsAclError>()
            .expect("ACL query error must preserve its Win32 operation");
        assert!(
            matches!(
                typed.operation,
                AclOperation::GetAclInformation | AclOperation::GetAce
            ),
            "unexpected ACL query operation: {:?}",
            typed.operation
        );
    }

    #[test]
    fn common_acl_open_rejects_a_case_sensitive_parent() {
        let tmp = TempDir::new().expect("tempdir");
        let parent = tmp.path().join("case-sensitive-parent");
        let target = parent.join("target.txt");
        std::fs::create_dir(&parent).expect("create parent");
        let _case_sensitive =
            override_case_sensitivity_for_test(&parent, CaseSensitivityTestOutcome::CaseSensitive);
        std::fs::write(&target, "fixture").expect("write target");
        let sid = LocalSid::from_string("S-1-5-21-1111111111-2222222222-3333333333-4444")
            .expect("test capability SID");

        let error = unsafe { add_allow_ace(&target, sid.as_ptr()) }
            .expect_err("all path-based ACL entry points must share the path-safety guard");

        assert_eq!(
            error.downcast_ref::<ProtectedMetadataError>(),
            Some(&ProtectedMetadataError::CaseSensitiveDirectoryUnsupported { path: parent })
        );
    }

    unsafe fn fetch_deny_flags(path: &std::path::Path, sid: *mut c_void) -> Vec<u8> {
        // SAFETY: the fetched ACL and SID are valid for the duration of this test inspection;
        // the security descriptor allocation is released before returning.
        unsafe {
            let (p_dacl, p_sd) = fetch_dacl_handle(path).expect("fetch test DACL");
            let mut info: super::ACL_SIZE_INFORMATION = std::mem::zeroed();
            assert_ne!(
                super::GetAclInformation(
                    p_dacl as *const super::ACL,
                    &mut info as *mut _ as *mut c_void,
                    std::mem::size_of::<super::ACL_SIZE_INFORMATION>() as u32,
                    super::AclSizeInformation,
                ),
                0
            );
            let mut flags = Vec::new();
            for index in 0..info.AceCount {
                let mut p_ace = std::ptr::null_mut();
                assert_ne!(super::GetAce(p_dacl, index, &mut p_ace), 0);
                let header = &*(p_ace as *const super::ACE_HEADER);
                if header.AceType != super::ACCESS_DENIED_ACE_TYPE {
                    continue;
                }
                let sid_ptr = (p_ace as usize
                    + std::mem::size_of::<super::ACE_HEADER>()
                    + std::mem::size_of::<u32>()) as *mut c_void;
                if super::EqualSid(sid_ptr, sid) != 0 {
                    flags.push(header.AceFlags);
                }
            }
            super::LocalFree(p_sd as super::HLOCAL);
            flags
        }
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
    fn revoke_read_deny_rejects_a_combined_deny_without_weakening_it() {
        let tmp = TempDir::new().expect("tempdir");
        let path = tmp.path().join("combined-target");
        std::fs::create_dir(&path).expect("create fixture");
        let sid = LocalSid::from_string("S-1-5-21-1111111111-2222222222-3333333333-4444")
            .expect("test capability SID");
        let target = unsafe { open_acl_target(&path, READ_CONTROL | WRITE_DAC, 1) }
            .expect("open ACL target");

        unsafe {
            seed_deny(
                &target,
                sid.as_ptr(),
                super::DENY_READ_MASK | super::FILE_WRITE_DATA,
                super::CONTAINER_INHERIT_ACE | super::OBJECT_INHERIT_ACE,
            )
        };
        let error = unsafe { revoke_deny_read_ace(&path, sid.as_ptr()) }
            .expect_err("combined deny must fail closed");
        assert!(error.to_string().contains("combined deny ACE"));
        assert!(unsafe { fetch_has_deny(&path, sid.as_ptr(), false) });

        unsafe { set_target_dacl(&target, target.p_dacl, 1).expect("restore original DACL") };
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
    fn non_inheritable_full_read_deny_is_completed_for_descendants() {
        let tmp = TempDir::new().expect("tempdir");
        let path = tmp.path().join("read-target");
        std::fs::create_dir(&path).expect("create fixture");
        let sid = LocalSid::from_string("S-1-5-21-1111111111-2222222222-3333333333-4444")
            .expect("test capability SID");
        let target = unsafe { open_acl_target(&path, READ_CONTROL | WRITE_DAC, 1) }
            .expect("open ACL target");

        unsafe { seed_deny(&target, sid.as_ptr(), super::DENY_READ_MASK, 0) };
        assert!(
            unsafe { add_deny_read_ace(&path, sid.as_ptr()) }
                .expect("complete inheritable read deny"),
            "deny flags were {:?}",
            unsafe { fetch_deny_flags(&path, sid.as_ptr()) }
        );
        let deny_flags = unsafe { fetch_deny_flags(&path, sid.as_ptr()) };
        assert!(
            deny_flags
                .iter()
                .copied()
                .any(super::is_runtime_managed_deny_ace),
            "runtime deny flags were {deny_flags:?}"
        );
        let child = path.join("child.txt");
        std::fs::write(&child, b"payload").expect("create inheriting child");
        assert!(
            unsafe { fetch_has_deny(&child, sid.as_ptr(), false) },
            "new descendants must inherit the complete deny-read ACE"
        );

        unsafe { set_target_dacl(&target, target.p_dacl, 1).expect("restore original DACL") };
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
        assert_eq!(
            error.downcast_ref::<ProtectedMetadataError>(),
            Some(&ProtectedMetadataError::ReparseTargetUnsupported { path: alias })
        );
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
