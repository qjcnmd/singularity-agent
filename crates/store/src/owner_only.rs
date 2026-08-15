//! Owner-only permission enforcement for session directories and index/backup files.
//!
//! Unix uses mode 0700/0600; Windows applies and verifies a one-ACE owner DACL.

use super::*;

pub fn ensure_owner_only_dir(path: &Path) -> StoreResult<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let metadata = std::fs::symlink_metadata(path).map_err(|error| {
            StoreError::InvalidState(format!("cannot inspect owner-only dir {}: {error}", path.display()))
        })?;
        if !metadata.is_dir() {
            return Err(StoreError::InvalidState(format!(
                "owner-only path is not a directory: {}",
                path.display()
            )));
        }
        if metadata.permissions().mode() & 0o077 != 0 {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).map_err(
                |error| {
                    StoreError::InvalidState(format!(
                        "cannot restrict owner-only dir {}: {error}",
                        path.display()
                    ))
                },
            )?;
        }
        Ok(())
    }
    #[cfg(windows)]
    {
        windows_owner_only::ensure_owner_only_dir(path)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        Ok(())
    }
}

pub fn ensure_owner_only_file(path: &Path) -> StoreResult<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let metadata = std::fs::symlink_metadata(path).map_err(|error| {
            StoreError::InvalidState(format!("cannot inspect owner-only file {}: {error}", path.display()))
        })?;
        if !metadata.is_file() {
            return Err(StoreError::InvalidState(format!(
                "owner-only path is not a regular file: {}",
                path.display()
            )));
        }
        if metadata.permissions().mode() & 0o077 != 0 {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(
                |error| {
                    StoreError::InvalidState(format!(
                        "cannot restrict owner-only file {}: {error}",
                        path.display()
                    ))
                },
            )?;
        }
        Ok(())
    }
    #[cfg(windows)]
    {
        windows_owner_only::ensure_owner_only_file(path)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        Ok(())
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
mod windows_owner_only {
    use std::ffi::c_void;
    use std::fs::{File, OpenOptions};
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use std::path::Path;
    use std::ptr::{null, null_mut};

    use super::StoreError;
    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_SUCCESS, HANDLE, HLOCAL, LocalFree};
    use windows_sys::Win32::Security::Authorization::{
        EXPLICIT_ACCESS_W, GetSecurityInfo, SE_FILE_OBJECT, SET_ACCESS, SetEntriesInAclW,
        SetSecurityInfo, TRUSTEE_IS_SID, TRUSTEE_IS_UNKNOWN, TRUSTEE_W,
    };
    use windows_sys::Win32::Security::{
        ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, ACL_SIZE_INFORMATION, AclSizeInformation,
        DACL_SECURITY_INFORMATION, EqualSid, GENERIC_MAPPING, GetAce, GetAclInformation,
        GetLengthSid, GetSecurityDescriptorDacl, GetTokenInformation, OWNER_SECURITY_INFORMATION,
        PROTECTED_DACL_SECURITY_INFORMATION, TOKEN_QUERY, TOKEN_USER, TokenUser,
    };
    use windows_sys::Win32::Storage_FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_GENERIC_EXECUTE, FILE_GENERIC_READ, FILE_GENERIC_WRITE,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, READ_CONTROL, WRITE_DAC,
        WRITE_OWNER,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
    const MAX_TOKEN_INFORMATION_BYTES: usize = 64 * 1024;

    pub(super) fn ensure_owner_only_file(path: &Path) -> Result<(), StoreError> {
        let file = File::open(path).map_err(invalid_state)?;
        if ensure_handle(&file).is_ok() {
            return Ok(());
        }
        let repair = OpenOptions::new()
            .read(true)
            .write(true)
            .access_mode(FILE_GENERIC_READ | FILE_GENERIC_WRITE | READ_CONTROL | WRITE_DAC)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .open(path)
            .map_err(invalid_state)?;
        set_owner_only_handle(&repair)?;
        ensure_handle(&repair)
    }

    pub(super) fn ensure_owner_only_dir(path: &Path) -> Result<(), StoreError> {
        let dir = OpenOptions::new()
            .read(true)
            .write(true)
            .access_mode(FILE_GENERIC_READ | FILE_GENERIC_WRITE | READ_CONTROL | WRITE_DAC)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)
            .map_err(invalid_state)?;
        if ensure_handle(&dir).is_ok() {
            return Ok(());
        }
        set_owner_only_handle(&dir)?;
        ensure_handle(&dir)
    }

    fn set_owner_only_handle(file: &File) -> Result<(), StoreError> {
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
            return Err(invalid_state("owner-only ACL could not be created"));
        }
        let handle = file.as_raw_handle() as HANDLE;
        let security_information =
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION | OWNER_SECURITY_INFORMATION;
        let status = unsafe {
            SetSecurityInfo(
                handle,
                SE_FILE_OBJECT,
                security_information,
                owner_sid,
                null_mut(),
                dacl,
                null_mut(),
            )
        };
        unsafe { LocalFree(dacl as HLOCAL) };
        if status != ERROR_SUCCESS {
            return Err(invalid_state("owner-only ACL could not be applied"));
        }
        Ok(())
    }

    fn ensure_handle(file: &File) -> Result<(), StoreError> {
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
            return Err(invalid_state("owner-only ACL could not be checked"));
        }
        let result = inspect_descriptor(descriptor, owner, dacl, &current_sid);
        unsafe { LocalFree(descriptor as HLOCAL) };
        result
    }

    fn inspect_descriptor(
        descriptor: *mut c_void,
        owner: *mut c_void,
        dacl: *mut ACL,
        current_sid: &[u8],
    ) -> Result<(), StoreError> {
        if owner.is_null() || current_sid.is_empty() {
            return Err(invalid_state("path is not owner-only"));
        }
        let current_sid = current_sid.as_ptr() as *mut c_void;
        if unsafe { EqualSid(owner, current_sid) } == 0 || dacl.is_null() {
            return Err(invalid_state("path is not owner-only"));
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
            return Err(invalid_state("path is not owner-only"));
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
            return Err(invalid_state("path is not owner-only"));
        }
        let mut ace = null_mut();
        if unsafe { GetAce(dacl, 0, &mut ace) } == 0 || ace.is_null() {
            return Err(invalid_state("path is not owner-only"));
        }
        let header = unsafe { &*(ace as *const ACE_HEADER) };
        if header.AceType != ACCESS_ALLOWED_ACE_TYPE || header.AceFlags != 0 {
            return Err(invalid_state("path is not owner-only"));
        }
        let allowed = unsafe { &*(ace as *const ACCESS_ALLOWED_ACE) };
        let sid = &allowed.SidStart as *const u32 as *mut c_void;
        if unsafe { EqualSid(sid, current_sid) } == 0 {
            return Err(invalid_state("path is not owner-only"));
        }
        let mut mask = allowed.Mask;
        let mapping = GENERIC_MAPPING {
            GenericRead: FILE_GENERIC_READ,
            GenericWrite: FILE_GENERIC_WRITE,
            GenericExecute: FILE_GENERIC_EXECUTE,
            GenericAll: FILE_GENERIC_READ | FILE_GENERIC_WRITE | FILE_GENERIC_EXECUTE,
        };
        unsafe { windows_sys::Win32::Security::MapGenericMask(&mut mask, &mapping) };
        if (mask & (FILE_GENERIC_READ | FILE_GENERIC_WRITE))
            != (FILE_GENERIC_READ | FILE_GENERIC_WRITE)
        {
            return Err(invalid_state("path is not owner-only"));
        }
        Ok(())
    }

    fn current_user_sid() -> Result<Vec<u8>, StoreError> {
        let mut token: HANDLE = 0;
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            return Err(invalid_state("owner-only ACL could not be checked"));
        }
        let result = current_user_sid_from_token(token);
        unsafe { CloseHandle(token) };
        result
    }

    fn current_user_sid_from_token(token: HANDLE) -> Result<Vec<u8>, StoreError> {
        let mut length = 0;
        let _ = unsafe { GetTokenInformation(token, TokenUser, null_mut(), 0, &mut length) };
        let length = usize::try_from(length)
            .map_err(|_| invalid_state("owner-only ACL could not be checked"))?;
        if length == 0 || length > MAX_TOKEN_INFORMATION_BYTES {
            return Err(invalid_state("owner-only ACL could not be checked"));
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
            return Err(invalid_state("owner-only ACL could not be checked"));
        }
        let token_user = unsafe { std::ptr::read_unaligned(buffer.as_ptr() as *const TOKEN_USER) };
        if token_user.User.Sid.is_null() {
            return Err(invalid_state("owner-only ACL could not be checked"));
        }
        let sid_length = unsafe { GetLengthSid(token_user.User.Sid) } as usize;
        if sid_length == 0 {
            return Err(invalid_state("owner-only ACL could not be checked"));
        }
        Ok(unsafe {
            std::slice::from_raw_parts(token_user.User.Sid as *const u8, sid_length).to_vec()
        })
    }

    fn invalid_state(error: impl std::fmt::Display) -> StoreError {
        StoreError::InvalidState(error.to_string())
    }
}
