use crate::token::current_user_sid_bytes;
use crate::winutil::string_from_sid_bytes;
use crate::winutil::to_wide;
use anyhow::Result;
use std::fmt;
use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::Foundation::ERROR_ALREADY_EXISTS;
use windows_sys::Win32::Foundation::ERROR_FILE_NOT_FOUND;
use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Foundation::HLOCAL;
use windows_sys::Win32::Foundation::LocalFree;
use windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
use windows_sys::Win32::Security::Authorization::SDDL_REVISION_1;
use windows_sys::Win32::Security::PSECURITY_DESCRIPTOR;
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::SYNCHRONIZE;
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::System::Threading::OpenMutexW;
use windows_sys::Win32::System::Threading::ReleaseMutex;

use crate::product_identity::READ_ACL_MUTEX_NAME;

/// The native operation that failed while observing the read-ACL coordination mutex.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadAclMutexOperation {
    Probe,
    Create,
}

/// Preserves the native error code for read-ACL coordination failures.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReadAclMutexError {
    pub operation: ReadAclMutexOperation,
    pub code: u32,
}

impl ReadAclMutexError {
    fn new(operation: ReadAclMutexOperation, code: u32) -> Self {
        Self { operation, code }
    }
}

impl fmt::Display for ReadAclMutexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "read ACL mutex {:?} failed with Windows error {}",
            self.operation, self.code
        )
    }
}

impl std::error::Error for ReadAclMutexError {}

/// The only state a caller may infer from a mutex probe.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadAclMutexState {
    Absent,
    Present,
}

/// Owns a read-ACL mutex that this process acquired.
pub struct ReadAclMutexGuard {
    handle: HANDLE,
}

impl Drop for ReadAclMutexGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = ReleaseMutex(self.handle);
            CloseHandle(self.handle);
        }
    }
}

/// Probes only the synchronize right needed to observe the named mutex.
pub fn probe_read_acl_mutex() -> Result<ReadAclMutexState> {
    probe_named_mutex(READ_ACL_MUTEX_NAME)
}

fn probe_named_mutex(name: &str) -> Result<ReadAclMutexState> {
    let name = to_wide(name);
    let handle = unsafe { OpenMutexW(SYNCHRONIZE, 0, name.as_ptr()) };
    let error = unsafe { GetLastError() };
    classify_open_mutex_result(handle, error)
}

fn classify_open_mutex_result(handle: HANDLE, error: u32) -> Result<ReadAclMutexState> {
    if handle != 0 {
        unsafe {
            CloseHandle(handle);
        }
        return Ok(ReadAclMutexState::Present);
    }
    if error == ERROR_FILE_NOT_FOUND {
        return Ok(ReadAclMutexState::Absent);
    }
    Err(anyhow::Error::new(ReadAclMutexError::new(
        ReadAclMutexOperation::Probe,
        error,
    )))
}

/// Creates the coordination mutex with a minimal read-only probe grant.
///
/// The creating user retains full control, while every process can only obtain `SYNCHRONIZE`.
/// This lets the restricted runner distinguish an active helper from an absent helper without
/// granting it mutex modification or full-object access.
pub fn acquire_read_acl_mutex() -> Result<Option<ReadAclMutexGuard>> {
    acquire_named_read_acl_mutex(READ_ACL_MUTEX_NAME)
}

fn acquire_named_read_acl_mutex(name: &str) -> Result<Option<ReadAclMutexGuard>> {
    let current_sid =
        string_from_sid_bytes(&current_user_sid_bytes()?).map_err(anyhow::Error::msg)?;
    let security_descriptor_sddl = to_wide(format!(
        "D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;{current_sid})(A;;0x00100000;;;WD)"
    ));
    let mut security_descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    let converted = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            security_descriptor_sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut security_descriptor,
            std::ptr::null_mut(),
        )
    };
    if converted == 0 {
        return Err(anyhow::Error::new(ReadAclMutexError::new(
            ReadAclMutexOperation::Create,
            unsafe { GetLastError() },
        )));
    }

    let security_attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: security_descriptor,
        bInheritHandle: 0,
    };
    let name = to_wide(name);
    let handle = unsafe { CreateMutexW(&security_attributes, 1, name.as_ptr()) };
    let error = unsafe { GetLastError() };
    unsafe {
        LocalFree(security_descriptor as HLOCAL);
    }
    if handle == 0 {
        return Err(anyhow::Error::new(ReadAclMutexError::new(
            ReadAclMutexOperation::Create,
            error,
        )));
    }
    if error == ERROR_ALREADY_EXISTS {
        unsafe {
            CloseHandle(handle);
        }
        return Ok(None);
    }
    Ok(Some(ReadAclMutexGuard { handle }))
}

#[cfg(test)]
mod tests {
    use super::ReadAclMutexError;
    use super::ReadAclMutexOperation;
    use super::ReadAclMutexState;
    use super::acquire_named_read_acl_mutex;
    use super::classify_open_mutex_result;
    use super::probe_named_mutex;
    use pretty_assertions::assert_eq;
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::Foundation::ERROR_ACCESS_DENIED;
    use windows_sys::Win32::Foundation::ERROR_FILE_NOT_FOUND;
    use windows_sys::Win32::System::Threading::CreateMutexW;

    #[test]
    fn access_denied_and_unknown_probe_results_are_typed() {
        for code in [ERROR_ACCESS_DENIED, 12345] {
            let error = classify_open_mutex_result(0, code).expect_err("probe must fail closed");
            let typed = error
                .downcast_ref::<ReadAclMutexError>()
                .expect("typed mutex error");
            assert_eq!(
                *typed,
                ReadAclMutexError {
                    operation: ReadAclMutexOperation::Probe,
                    code,
                }
            );
        }
        assert_eq!(
            classify_open_mutex_result(0, ERROR_FILE_NOT_FOUND).expect("missing mutex"),
            ReadAclMutexState::Absent
        );
    }

    #[test]
    fn real_mutex_probe_uses_observable_present_and_absent_states() {
        let name = format!(r"Local\SingularityReadAclMutexTest_{}", std::process::id());
        let name = super::to_wide(&name);
        let handle = unsafe { CreateMutexW(std::ptr::null_mut(), 0, name.as_ptr()) };
        assert_ne!(handle, 0, "test mutex must be created");
        assert_eq!(
            probe_named_mutex(&format!(
                r"Local\SingularityReadAclMutexTest_{}",
                std::process::id()
            ))
            .expect("probe existing mutex"),
            ReadAclMutexState::Present
        );
        unsafe {
            CloseHandle(handle);
        }
        assert_eq!(
            probe_named_mutex(&format!(
                r"Local\SingularityReadAclMutexTest_{}",
                std::process::id()
            ))
            .expect("probe released mutex"),
            ReadAclMutexState::Absent
        );
    }

    #[test]
    fn created_mutex_grants_only_the_probe_path_to_observers() {
        let name = format!(
            r"Local\SingularityReadAclMutexSecurityTest_{}",
            std::process::id()
        );
        let guard = acquire_named_read_acl_mutex(&name)
            .expect("create mutex with probe ACL")
            .expect("test mutex must be new");
        assert_eq!(
            probe_named_mutex(&name).expect("probe mutex with synchronize"),
            ReadAclMutexState::Present
        );
        drop(guard);
        assert_eq!(
            probe_named_mutex(&name).expect("probe released mutex"),
            ReadAclMutexState::Absent
        );
    }
}
