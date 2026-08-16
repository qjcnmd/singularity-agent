//! Windows owner-only ACL primitive.
//!
//! This is the single repository implementation for the model/store security
//! layers. It pins one object by handle, verifies the current-user owner and a
//! protected single-ACE DACL, and applies the same contract when repair is safe.

use std::ffi::c_void;
use std::fs::File;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::path::Path;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{CloseHandle, ERROR_SUCCESS, HANDLE, HLOCAL, LocalFree};
use windows_sys::Win32::Security::Authorization::{
    EXPLICIT_ACCESS_W, GetSecurityInfo, SE_FILE_OBJECT, SET_ACCESS, SetEntriesInAclW,
    SetSecurityInfo, TRUSTEE_IS_SID, TRUSTEE_IS_UNKNOWN, TRUSTEE_W,
};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, ACL_SIZE_INFORMATION, AclSizeInformation,
    DACL_SECURITY_INFORMATION, EqualSid, GENERIC_MAPPING, GetAce, GetAclInformation, GetLengthSid,
    GetSecurityDescriptorControl, GetSecurityDescriptorDacl, GetTokenInformation,
    OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, SE_DACL_PROTECTED,
    TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows_sys::Win32::Storage::FileSystem::{
    CREATE_NEW, CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_GENERIC_EXECUTE, FILE_GENERIC_READ, FILE_GENERIC_WRITE,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING, READ_CONTROL, WRITE_DAC,
    WRITE_OWNER,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
const MAX_TOKEN_INFORMATION_BYTES: usize = 64 * 1024;

/// Verify that the exact object pinned by `file` has the owner-only contract.
pub fn ensure_owner_only_handle(file: &File) -> io::Result<()> {
    let current_sid = current_user_sid()?;
    let handle = file.as_raw_handle() as HANDLE;
    let mut owner = null_mut();
    let mut dacl = null_mut();
    let mut descriptor = null_mut();
    let status = unsafe {
        GetSecurityInfo(
            handle,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            null_mut(),
            &mut dacl,
            null_mut(),
            &mut descriptor,
        )
    };
    if status != ERROR_SUCCESS || descriptor.is_null() {
        if !descriptor.is_null() {
            unsafe { LocalFree(descriptor as HLOCAL) };
        }
        return Err(io::Error::other("owner-only ACL could not be checked"));
    }
    let result = inspect_descriptor(descriptor, owner, dacl, &current_sid);
    unsafe { LocalFree(descriptor as HLOCAL) };
    result
}

/// Apply owner + protected owner-only DACL through the handle, then verify it.
///
/// The caller must open the handle with `WRITE_OWNER` and `WRITE_DAC`; the
/// function itself never silently retries with another handle.
pub fn set_owner_only_handle(file: &File) -> io::Result<()> {
    apply_owner_only_security(file, true)?;
    ensure_owner_only_handle(file)
}

/// Apply only the protected owner-only DACL; owner repair is done by the caller
/// with a handle opened for owner access.
pub fn set_owner_only_dacl_handle(file: &File) -> io::Result<()> {
    apply_owner_only_security(file, false)
}

/// Create a new file already restricted to the owner-only contract.
pub fn create_owner_only_file(path: &Path) -> io::Result<File> {
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            FILE_GENERIC_READ | FILE_GENERIC_WRITE | READ_CONTROL | WRITE_DAC | WRITE_OWNER,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            null(),
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT,
            0,
        )
    };
    if handle == -1 || handle == 0 {
        return Err(io::Error::last_os_error());
    }
    let file = unsafe { File::from_raw_handle(handle as _) };
    set_owner_only_handle(&file)?;
    Ok(file)
}

/// Verify a file path, repairing an inherited/incorrect DACL when the current
/// process can open the same object with `READ_CONTROL|WRITE_DAC|WRITE_OWNER`.
pub fn ensure_owner_only_file(path: &Path) -> io::Result<()> {
    let verify = File::open(path)?;
    if ensure_owner_only_handle(&verify).is_ok() {
        return Ok(());
    }
    let repair = open_owner_repair_path(path, false).map_err(|error| {
        io::Error::other(format!(
            "open owner repair handle for {}: {error}",
            path.display()
        ))
    })?;
    set_owner_only_handle(&repair).map_err(|error| {
        io::Error::other(format!("set owner-only ACL on {}: {error}", path.display()))
    })
}

/// Verify a directory path, repairing when the process can open the directory
/// with the required security access.
pub fn ensure_owner_only_dir(path: &Path) -> io::Result<()> {
    let dir = open_owner_repair_path(path, true).map_err(|error| {
        io::Error::other(format!(
            "open owner repair handle for {}: {error}",
            path.display()
        ))
    })?;
    if ensure_owner_only_handle(&dir).is_ok() {
        return Ok(());
    }
    set_owner_only_handle(&dir).map_err(|error| {
        io::Error::other(format!("set owner-only ACL on {}: {error}", path.display()))
    })
}

fn open_owner_repair_path(path: &Path, directory: bool) -> io::Result<File> {
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut flags = FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT;
    if directory {
        flags = FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT;
    }
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            READ_CONTROL | WRITE_DAC | WRITE_OWNER,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            null(),
            OPEN_EXISTING,
            flags,
            0,
        )
    };
    if handle == -1 || handle == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_handle(handle as _) })
}

fn apply_owner_only_security(file: &File, set_owner: bool) -> io::Result<()> {
    let mut sid = current_user_sid()?;
    let owner_sid = sid.as_mut_ptr() as *mut c_void;
    let trustee = TRUSTEE_W {
        pMultipleTrustee: null_mut(),
        MultipleTrusteeOperation: 0,
        TrusteeForm: TRUSTEE_IS_SID,
        TrusteeType: TRUSTEE_IS_UNKNOWN,
        ptstrName: owner_sid as *mut u16,
    };
    let entry = EXPLICIT_ACCESS_W {
        grfAccessPermissions: FILE_GENERIC_READ
            | FILE_GENERIC_WRITE
            | READ_CONTROL
            | WRITE_DAC
            | WRITE_OWNER,
        grfAccessMode: SET_ACCESS,
        grfInheritance: 0,
        Trustee: trustee,
    };
    let mut dacl: *mut ACL = null_mut();
    let status = unsafe { SetEntriesInAclW(1, &entry, null(), &mut dacl) };
    if status != ERROR_SUCCESS || dacl.is_null() {
        if !dacl.is_null() {
            unsafe { LocalFree(dacl as HLOCAL) };
        }
        return Err(io::Error::other("owner-only ACL could not be created"));
    }
    let handle = file.as_raw_handle() as HANDLE;
    let mut security_information = DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION;
    let owner = if set_owner {
        security_information |= OWNER_SECURITY_INFORMATION;
        owner_sid
    } else {
        null_mut()
    };
    let status = unsafe {
        SetSecurityInfo(
            handle,
            SE_FILE_OBJECT,
            security_information,
            owner,
            null_mut(),
            dacl,
            null_mut(),
        )
    };
    unsafe { LocalFree(dacl as HLOCAL) };
    if status != ERROR_SUCCESS {
        return Err(io::Error::other("owner-only ACL could not be applied"));
    }
    Ok(())
}

fn inspect_descriptor(
    descriptor: *mut c_void,
    owner: *mut c_void,
    dacl: *mut ACL,
    current_sid: &[u8],
) -> io::Result<()> {
    if owner.is_null() || current_sid.is_empty() {
        return Err(io::Error::other("path is not owner-only"));
    }
    let current_sid = current_sid.as_ptr() as *mut c_void;
    if unsafe { EqualSid(owner, current_sid) } == 0 || dacl.is_null() {
        return Err(io::Error::other("path is not owner-only"));
    }
    let mut control = 0u16;
    let mut revision = 0u32;
    if unsafe { GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) } == 0
        || control & SE_DACL_PROTECTED == 0
    {
        return Err(io::Error::other("path DACL is not protected"));
    }
    let mut dacl_present = 0;
    let mut descriptor_dacl = null_mut();
    let mut dacl_defaulted = 0;
    if unsafe {
        GetSecurityDescriptorDacl(
            descriptor,
            &mut dacl_present,
            &mut descriptor_dacl,
            &mut dacl_defaulted,
        )
    } == 0
        || dacl_present == 0
        || descriptor_dacl.is_null()
        || descriptor_dacl != dacl
    {
        return Err(io::Error::other("path DACL is not present"));
    }
    let mut info: ACL_SIZE_INFORMATION = unsafe { std::mem::zeroed() };
    if unsafe {
        GetAclInformation(
            dacl,
            &mut info as *mut _ as *mut c_void,
            std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
            AclSizeInformation,
        )
    } == 0
        || info.AceCount != 1
    {
        return Err(io::Error::other("path DACL must contain exactly one ACE"));
    }
    let mut ace = null_mut();
    if unsafe { GetAce(dacl, 0, &mut ace) } == 0 || ace.is_null() {
        return Err(io::Error::other("path DACL has no ACE"));
    }
    let header = unsafe { &*(ace as *const ACE_HEADER) };
    if header.AceType != ACCESS_ALLOWED_ACE_TYPE || header.AceFlags != 0 {
        return Err(io::Error::other(
            "path DACL ACE is not a non-inherited allow ACE",
        ));
    }
    let allowed = unsafe { &*(ace as *const ACCESS_ALLOWED_ACE) };
    let sid = &allowed.SidStart as *const u32 as *mut c_void;
    if unsafe { EqualSid(sid, current_sid) } == 0 {
        return Err(io::Error::other("path DACL ACE is not the current user"));
    }
    let mut mask = allowed.Mask;
    let mapping = GENERIC_MAPPING {
        GenericRead: FILE_GENERIC_READ,
        GenericWrite: FILE_GENERIC_WRITE,
        GenericExecute: FILE_GENERIC_EXECUTE,
        GenericAll: FILE_GENERIC_READ | FILE_GENERIC_WRITE | FILE_GENERIC_EXECUTE,
    };
    unsafe { windows_sys::Win32::Security::MapGenericMask(&mut mask, &mapping) };
    if (mask & (FILE_GENERIC_READ | FILE_GENERIC_WRITE)) != (FILE_GENERIC_READ | FILE_GENERIC_WRITE)
    {
        return Err(io::Error::other(
            "path DACL ACE does not grant the required read/write access",
        ));
    }
    Ok(())
}

fn current_user_sid() -> io::Result<Vec<u8>> {
    let mut token: HANDLE = 0;
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::other("owner-only ACL could not be checked"));
    }
    let result = current_user_sid_from_token(token);
    unsafe { CloseHandle(token) };
    result
}

fn current_user_sid_from_token(token: HANDLE) -> io::Result<Vec<u8>> {
    let mut length = 0;
    let _ = unsafe { GetTokenInformation(token, TokenUser, null_mut(), 0, &mut length) };
    let length = usize::try_from(length)
        .map_err(|_| io::Error::other("owner-only ACL could not be checked"))?;
    if length == 0 || length > MAX_TOKEN_INFORMATION_BYTES {
        return Err(io::Error::other("owner-only ACL could not be checked"));
    }
    let mut buffer = vec![0u8; length];
    let mut return_length = length as u32;
    if unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            buffer.as_mut_ptr() as *mut c_void,
            length as u32,
            &mut return_length,
        )
    } == 0
    {
        return Err(io::Error::other("owner-only ACL could not be checked"));
    }
    let token_user = unsafe { std::ptr::read_unaligned(buffer.as_ptr() as *const TOKEN_USER) };
    if token_user.User.Sid.is_null() {
        return Err(io::Error::other("owner-only ACL could not be checked"));
    }
    let sid_length = unsafe { GetLengthSid(token_user.User.Sid) } as usize;
    if sid_length == 0 {
        return Err(io::Error::other("owner-only ACL could not be checked"));
    }
    Ok(
        unsafe {
            std::slice::from_raw_parts(token_user.User.Sid as *const u8, sid_length).to_vec()
        },
    )
}
