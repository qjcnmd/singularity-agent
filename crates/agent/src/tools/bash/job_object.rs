//! 进程树终止的内核边界：`KILL_ON_JOB_CLOSE` 作业对象的 RAII 封装（Windows）。

#![cfg(windows)]
#![allow(unsafe_code)] // Windows 平台进程树终止的内核 API 集中在此模块。

use std::ffi::c_void;
use std::io;
use std::mem::size_of;
use std::os::windows::io::AsRawHandle;
use std::process::Child;
use std::ptr::null;

use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject,
};

fn last_os_error(operation: &str) -> io::Error {
    let base = io::Error::from_raw_os_error(unsafe { GetLastError() } as i32);
    io::Error::new(base.kind(), format!("{operation}: {base}"))
}

/// 子进程一经绑定，其派生的全部子孙都留在同一作业内；关闭作业句柄或显式
/// 终止都会由内核连带杀死整棵树，不依赖逐个枚举进程。
pub(super) struct JobObject {
    handle: HANDLE,
}

impl JobObject {
    pub(super) fn new() -> io::Result<Self> {
        let handle = unsafe { CreateJobObjectW(null(), null()) };
        if handle == 0 {
            return Err(last_os_error("CreateJobObjectW"));
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let configured = unsafe {
            SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const c_void,
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if configured == 0 {
            let error = last_os_error("SetInformationJobObject");
            unsafe { CloseHandle(handle) };
            return Err(error);
        }
        Ok(Self { handle })
    }

    /// 把已创建的子进程绑定进作业；此后它派生的子孙都无法逃逸出整树终止范围。
    pub(super) fn assign(&self, child: &Child) -> io::Result<()> {
        let assigned =
            unsafe { AssignProcessToJobObject(self.handle, child.as_raw_handle() as HANDLE) };
        if assigned == 0 {
            return Err(last_os_error("AssignProcessToJobObject"));
        }
        Ok(())
    }

    pub(super) fn terminate(&self, exit_code: u32) -> bool {
        unsafe { TerminateJobObject(self.handle, exit_code) != 0 }
    }
}

impl Drop for JobObject {
    fn drop(&mut self) {
        // 关闭带 KILL_ON_JOB_CLOSE 的句柄会连带终止仍在运行的子孙进程；
        // 这是进程树存活的最终所有权边界。
        unsafe { CloseHandle(self.handle) };
    }
}
