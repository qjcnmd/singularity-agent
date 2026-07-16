use anyhow::Result;
use singularity_windows_sandbox::product_identity::READ_ACL_MUTEX_NAME;
use singularity_windows_sandbox::to_wide;
use std::ffi::OsStr;
use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Foundation::WAIT_ABANDONED_0;
use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::System::Threading::INFINITE;
use windows_sys::Win32::System::Threading::ReleaseMutex;
use windows_sys::Win32::System::Threading::WaitForSingleObject;

pub(super) struct ReadAclMutexGuard {
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

pub(super) fn acquire_read_acl_mutex() -> Result<ReadAclMutexGuard> {
    let name = to_wide(OsStr::new(READ_ACL_MUTEX_NAME));
    let handle = unsafe { CreateMutexW(std::ptr::null_mut(), 0, name.as_ptr()) };
    if handle == 0 {
        return Err(anyhow::anyhow!("CreateMutexW failed: {}", unsafe {
            GetLastError()
        }));
    }
    let wait = unsafe { WaitForSingleObject(handle, INFINITE) };
    if wait != WAIT_OBJECT_0 && wait != WAIT_ABANDONED_0 {
        unsafe {
            CloseHandle(handle);
        }
        return Err(anyhow::anyhow!("WaitForSingleObject failed: {wait}"));
    }
    Ok(ReadAclMutexGuard { handle })
}
