//! 子进程启动与进程树归属的内核边界（Windows）。
//!
//! 这里集中拥有本次调用相关的一切系统句柄：作业对象、主进程、初始线程与两条
//! 管道。启动顺序保证「能被执行的进程」在运行前就已归属本次作业，而不是先运行
//! 再补绑；失败路径按相反顺序回收，不留可运行的孤儿。

#![cfg(windows)]
#![allow(unsafe_code)] // Windows 平台进程创建与进程树终止的内核 API 集中在此模块。

use std::ffi::c_void;
use std::io;
use std::mem::size_of;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::os::windows::process::CommandExt;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::ptr::null;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject,
};
use windows_sys::Win32::System::Threading::{
    CREATE_NO_WINDOW, CREATE_SUSPENDED, OpenThread, ResumeThread, THREAD_SUSPEND_RESUME,
};

/// 在失败点立即读取系统错误并补上操作名；必须在失败的调用之后立刻调用。
fn last_os_error(operation: &str) -> io::Error {
    let base = io::Error::last_os_error();
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

    /// 把尚未恢复运行的子进程绑定进作业；此后它派生的子孙都无法逃逸出整树
    /// 终止范围。
    fn assign(&self, process: HANDLE) -> io::Result<()> {
        let assigned = unsafe { AssignProcessToJobObject(self.handle, process) };
        if assigned == 0 {
            return Err(last_os_error("AssignProcessToJobObject"));
        }
        Ok(())
    }

    /// 整树终止：作业对象由内核连带终止所有子孙进程。
    fn terminate(&self) {
        unsafe { TerminateJobObject(self.handle, 1) };
    }
}

impl Drop for JobObject {
    fn drop(&mut self) {
        // 关闭带 KILL_ON_JOB_CLOSE 的句柄会连带终止仍在运行的子孙进程；
        // 这是进程树存活的最终所有权边界。
        unsafe { CloseHandle(self.handle) };
    }
}

/// 已纳入平台进程树管理的 shell 子进程。
///
/// 终止必须走 [`ManagedChild::kill_tree`]：它同时终止作业对象与主进程。
pub(crate) struct ManagedChild {
    pub(super) child: Child,
    job: JobObject,
}

impl ManagedChild {
    /// 整树终止：作业对象内核级连带原子终止所有子孙进程，随后对主进程补一次
    /// kill，确保句柄状态确定收敛。
    pub(super) fn kill_tree(&mut self) {
        self.job.terminate();
        let _ = self.child.kill();
    }

    /// 有界回收子进程，超时放弃，避免残留句柄无限阻塞。
    pub(super) fn wait_bounded(&mut self, timeout: std::time::Duration) -> Option<ExitStatus> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => return Some(status),
                Ok(None) => {}
                Err(_) => return None,
            }
            if std::time::Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
}

/// 启动子进程并纳入平台进程树管理。
///
/// 顺序即契约：先建作业，再以 `CREATE_SUSPENDED` 创建子进程——被挂起的主线程
/// 在恢复前不会执行任何用户命令，也不能派生下一代——随后绑定作业，最后用本
/// 边界自己持有的初始线程句柄恢复它。任何一步失败都终止尚未运行的子进程并
/// 释放全部句柄，因此不会留下可运行的、未归属本次作业的后代。
///
/// 这里保留 `std::process::Command` 负责命令行转义、环境与管道建立：它是唯一
/// 拥有这些语义的实现，不因本项修复而复制一份（Rust 固定工具链的稳定
/// `CommandExt` 仍不提供创建时的作业属性，见 `PROC_THREAD_ATTRIBUTE_JOB_LIST`）。
pub(crate) fn spawn_in_job(
    shell: &str,
    shell_args: &[String],
    cwd: &std::path::Path,
) -> io::Result<ManagedChild> {
    let job = JobObject::new()?;
    let mut command = Command::new(shell);
    command
        .args(shell_args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW | CREATE_SUSPENDED);
    let mut child = command.spawn()?;
    let process = child.as_raw_handle() as HANDLE;
    let started = job.assign(process).and_then(|()| {
        let thread = owned_initial_thread(child.id())?;
        let resumed = unsafe { ResumeThread(thread.as_raw_handle() as HANDLE) };
        if resumed == u32::MAX {
            return Err(last_os_error("ResumeThread"));
        }
        Ok(())
    });
    match started {
        Ok(()) => Ok(ManagedChild { child, job }),
        Err(error) => {
            // 子进程保持挂起，主进程句柄仍由 Child 拥有；先整树终止再回收。
            job.terminate();
            let _ = child.kill();
            let _ = child.wait();
            Err(error)
        }
    }
}

/// 打开刚创建子进程的初始线程；句柄由本边界拥有，随 `OwnedHandle` 在恢复后
/// 立即关闭。
///
/// `CREATE_SUSPENDED` 保证该进程在恢复前只有一个线程且不会自行创建线程，因此
/// 快照中属于它的线程就是 `CreateProcess` 建立的主线程。稳定工具链的
/// `std::process` 不暴露主线程句柄（`sys::process::windows::Process::
/// main_thread_handle` 在 unstable 的 `process_internals` 之后），因此这里按
/// 进程 id 枚举线程，而不是重写整个 `CreateProcessW` 调用。
fn owned_initial_thread(process_id: u32) -> io::Result<OwnedHandle> {
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(last_os_error("CreateToolhelp32Snapshot"));
    }
    let mut entry: THREADENTRY32 = unsafe { std::mem::zeroed() };
    entry.dwSize = size_of::<THREADENTRY32>() as u32;
    let mut found = 0;
    let mut present = unsafe { Thread32First(snapshot, &mut entry) } != 0;
    while present {
        if entry.th32OwnerProcessID == process_id {
            found = entry.th32ThreadID;
            break;
        }
        present = unsafe { Thread32Next(snapshot, &mut entry) } != 0;
    }
    unsafe { CloseHandle(snapshot) };
    if found == 0 {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("no thread found for suspended process {process_id}"),
        ));
    }
    let thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, found) };
    if thread == 0 {
        return Err(last_os_error("OpenThread"));
    }
    // 不变量：OpenThread 成功即返回有效句柄；所有权随即交给 OwnedHandle。
    Ok(unsafe { OwnedHandle::from_raw_handle(thread as *mut c_void) })
}
