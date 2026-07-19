use anyhow::Result;
use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt as _;
use std::path::Path;
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_IO_INCOMPLETE, ERROR_NOT_FOUND, ERROR_NOTIFY_ENUM_DIR,
    ERROR_OPERATION_ABORTED, GetLastError, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_FLAG_OVERLAPPED,
    FILE_LIST_DIRECTORY, FILE_NOTIFY_CHANGE_ATTRIBUTES, FILE_NOTIFY_CHANGE_CREATION,
    FILE_NOTIFY_CHANGE_DIR_NAME, FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_CHANGE_LAST_WRITE,
    FILE_NOTIFY_CHANGE_SECURITY, FILE_NOTIFY_CHANGE_SIZE, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, OPEN_EXISTING, ReadDirectoryChangesW,
};
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows_sys::Win32::System::Threading::{CreateEventW, INFINITE, WaitForSingleObject};

const CHANGE_BUFFER_BYTES: usize = 64 * 1024;
const CHANGE_FILTER: u32 = FILE_NOTIFY_CHANGE_FILE_NAME
    | FILE_NOTIFY_CHANGE_DIR_NAME
    | FILE_NOTIFY_CHANGE_ATTRIBUTES
    | FILE_NOTIFY_CHANGE_SIZE
    | FILE_NOTIFY_CHANGE_LAST_WRITE
    | FILE_NOTIFY_CHANGE_CREATION
    | FILE_NOTIFY_CHANGE_SECURITY;

/// A fail-closed observation of workspace mutations during one sandbox command.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkspaceChangeObservation {
    Unchanged,
    Changed,
    Unknown,
}

/// Registers a recursive Windows directory-change request before the sandbox child starts.
pub struct WorkspaceChangeMonitor {
    directory: HANDLE,
    event: HANDLE,
    overlapped: Box<OVERLAPPED>,
    _buffer: Box<[u8; CHANGE_BUFFER_BYTES]>,
    pending: bool,
}

impl WorkspaceChangeMonitor {
    /// Starts monitoring an existing workspace without following a final reparse point.
    pub fn start(workspace: &Path) -> Result<Self> {
        let mut wide = workspace.as_os_str().encode_wide().collect::<Vec<_>>();
        wide.push(0);
        let directory = unsafe {
            CreateFileW(
                wide.as_ptr(),
                FILE_LIST_DIRECTORY,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_OVERLAPPED,
                0,
            )
        };
        if directory == INVALID_HANDLE_VALUE {
            anyhow::bail!(
                "workspace change monitor open failed with error {}",
                unsafe { GetLastError() }
            );
        }
        let event = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
        if event == 0 {
            let code = unsafe { GetLastError() };
            unsafe { CloseHandle(directory) };
            anyhow::bail!("workspace change monitor event failed with error {code}");
        }
        let mut overlapped = Box::new(unsafe { std::mem::zeroed::<OVERLAPPED>() });
        overlapped.hEvent = event;
        let mut buffer = Box::new([0u8; CHANGE_BUFFER_BYTES]);
        let started = unsafe {
            ReadDirectoryChangesW(
                directory,
                buffer.as_mut_ptr() as *mut c_void,
                CHANGE_BUFFER_BYTES as u32,
                1,
                CHANGE_FILTER,
                std::ptr::null_mut(),
                overlapped.as_mut() as *mut OVERLAPPED,
                None,
            )
        };
        if started == 0 {
            let code = unsafe { GetLastError() };
            unsafe {
                CloseHandle(event);
                CloseHandle(directory);
            }
            anyhow::bail!("workspace change monitor registration failed with error {code}");
        }
        Ok(Self {
            directory,
            event,
            overlapped,
            _buffer: buffer,
            pending: true,
        })
    }

    /// Finishes the observation after the sandbox child and its Job Object have exited.
    pub fn finish(mut self) -> Result<WorkspaceChangeObservation> {
        self.finish_pending()
    }

    fn finish_pending(&mut self) -> Result<WorkspaceChangeObservation> {
        if !self.pending {
            return Ok(WorkspaceChangeObservation::Unknown);
        }
        let mut transferred = 0u32;
        let completed = unsafe {
            GetOverlappedResult(
                self.directory,
                self.overlapped.as_mut(),
                &mut transferred,
                0,
            )
        };
        if completed != 0 {
            self.pending = false;
            return Ok(if transferred == 0 {
                WorkspaceChangeObservation::Unknown
            } else {
                WorkspaceChangeObservation::Changed
            });
        }
        let code = unsafe { GetLastError() };
        if code == ERROR_NOTIFY_ENUM_DIR {
            self.pending = false;
            return Ok(WorkspaceChangeObservation::Unknown);
        }
        if code != ERROR_IO_INCOMPLETE {
            self.pending = false;
            anyhow::bail!("workspace change monitor query failed with error {code}");
        }

        let cancelled = unsafe { CancelIoEx(self.directory, self.overlapped.as_mut()) };
        if cancelled == 0 {
            let cancel_code = unsafe { GetLastError() };
            if cancel_code != ERROR_NOT_FOUND {
                unsafe { CloseHandle(self.directory) };
                self.directory = INVALID_HANDLE_VALUE;
                let wait = unsafe { WaitForSingleObject(self.event, INFINITE) };
                self.pending = false;
                anyhow::bail!(
                    "workspace change monitor cancel failed with error {cancel_code} (wait={wait})"
                );
            }
        }
        let wait = unsafe { WaitForSingleObject(self.event, INFINITE) };
        if wait != 0 {
            self.pending = false;
            anyhow::bail!("workspace change monitor wait failed with status {wait}");
        }
        transferred = 0;
        let drained = unsafe {
            GetOverlappedResult(
                self.directory,
                self.overlapped.as_mut(),
                &mut transferred,
                0,
            )
        };
        self.pending = false;
        if drained != 0 {
            return Ok(if transferred == 0 {
                WorkspaceChangeObservation::Unknown
            } else {
                WorkspaceChangeObservation::Changed
            });
        }
        let drain_code = unsafe { GetLastError() };
        if drain_code == ERROR_OPERATION_ABORTED {
            Ok(WorkspaceChangeObservation::Unchanged)
        } else if drain_code == ERROR_NOTIFY_ENUM_DIR {
            Ok(WorkspaceChangeObservation::Unknown)
        } else {
            anyhow::bail!("workspace change monitor drain failed with error {drain_code}")
        }
    }
}

impl Drop for WorkspaceChangeMonitor {
    fn drop(&mut self) {
        if self.pending {
            let cancelled = unsafe { CancelIoEx(self.directory, self.overlapped.as_mut()) };
            if cancelled == 0 && unsafe { GetLastError() } != ERROR_NOT_FOUND {
                unsafe { CloseHandle(self.directory) };
                self.directory = INVALID_HANDLE_VALUE;
            }
            unsafe { WaitForSingleObject(self.event, INFINITE) };
            self.pending = false;
        }
        unsafe {
            CloseHandle(self.event);
            if self.directory != INVALID_HANDLE_VALUE {
                CloseHandle(self.directory);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{WorkspaceChangeMonitor, WorkspaceChangeObservation};

    #[test]
    fn monitor_distinguishes_changed_and_unchanged_workspaces() {
        let workspace = tempfile::tempdir().expect("workspace");
        let unchanged = WorkspaceChangeMonitor::start(workspace.path()).expect("start monitor");
        assert_eq!(
            unchanged.finish().expect("finish unchanged monitor"),
            WorkspaceChangeObservation::Unchanged
        );

        let changed = WorkspaceChangeMonitor::start(workspace.path()).expect("start monitor");
        std::fs::write(workspace.path().join("changed.txt"), b"changed").expect("write change");
        assert_eq!(
            changed.finish().expect("finish changed monitor"),
            WorkspaceChangeObservation::Changed
        );
    }
}
