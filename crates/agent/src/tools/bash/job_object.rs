//! 子进程启动与进程树归属的内核边界（仅 Windows）。
//!
//! 本次调用的所有系统句柄都集中在这里：作业对象、主进程、初始线程和两条管道。启动
//! 顺序保证「可能被执行的进程」运行前已归属本次作业，失败按相反顺序回收，不留孤儿。

#![cfg(windows)]
#![allow(unsafe_code)] // Windows 平台进程创建与进程树终止的内核 API 集中在此模块。

use std::ffi::c_void;
use std::io;
use std::mem::size_of;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::os::windows::process::CommandExt;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::ptr::null;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{ERROR_NO_MORE_FILES, HANDLE, INVALID_HANDLE_VALUE};
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

/// 在失败点立刻读取系统错误，并补上操作名；必须在失败的那次调用之后马上调用。
fn last_os_error(operation: &str) -> io::Error {
    let base = io::Error::last_os_error();
    io::Error::new(base.kind(), format!("{operation}: {base}"))
}

/// 子进程一旦绑进作业，它派生的所有子孙都留在同一个作业里；关闭作业句柄或显式终止
/// 时，内核会连带杀掉整棵树，不必逐个枚举进程。句柄由 `OwnedHandle` 独占持有：创建
/// 成功就交出所有权，配置失败和析构都走同一条自动关闭路径，不再有第二个手工释放处。
pub(super) struct JobObject {
    handle: OwnedHandle,
}

impl JobObject {
    pub(super) fn new() -> io::Result<Self> {
        let raw = unsafe { CreateJobObjectW(null(), null()) };
        if raw == 0 {
            return Err(last_os_error("CreateJobObjectW"));
        }
        // 不变量：CreateJobObjectW 一旦成功就返回有效句柄，所有权随即交给 OwnedHandle。
        let handle = unsafe { OwnedHandle::from_raw_handle(raw as *mut c_void) };
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let configured = unsafe {
            SetInformationJobObject(
                handle.as_raw_handle() as HANDLE,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const c_void,
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if configured == 0 {
            // 先把系统错误取出来再返回：此刻句柄还有效，错误必须在句柄关闭之前读。
            return Err(last_os_error("SetInformationJobObject"));
        }
        Ok(Self { handle })
    }

    /// 把还没恢复运行的子进程绑进作业；此后它派生的子孙都逃不出整树终止的范围。
    ///
    /// 绑定失败意味着这个进程不在本次作业内（例如已被一个不允许嵌套的祖先作业占用），
    /// 作业终止对它就是空操作，调用方必须改成单独终止它，不能假设绑定一定成功。
    fn assign(&self, process: HANDLE) -> io::Result<()> {
        let assigned =
            unsafe { AssignProcessToJobObject(self.handle.as_raw_handle() as HANDLE, process) };
        if assigned == 0 {
            return Err(last_os_error("AssignProcessToJobObject"));
        }
        Ok(())
    }

    /// 整树终止：内核会连带终止作业里的所有子孙进程。
    ///
    /// 返回值是内核的实际结果，调用方必须按回收失败来报告：终止被拒绝时进程树仍然
    /// 活着。重复执行同一动作不改变结果，句柄关闭时的 kill-on-close 仍兜底资源回收。
    fn terminate(&self) -> io::Result<()> {
        let terminated = unsafe { TerminateJobObject(self.handle.as_raw_handle() as HANDLE, 1) };
        if terminated == 0 {
            return Err(last_os_error("TerminateJobObject"));
        }
        Ok(())
    }
}

/// 终止动作之后等待回收的有界窗口：窗口内没观察到退出就不再等，回收结果按未知上报，
/// 作业句柄关闭时的 kill-on-close 兜底资源回收。常规收尾和启动失败共用同一个窗口，
/// 两条路径的「有界」语义因此一致。
const RECLAIM_GRACE: Duration = Duration::from_secs(5);

/// 有界等待的结果：区分已回收、回收超时和回收出错。
///
/// 这三种状态是契约的一部分：只有 `Exited` 能说子进程已经结束；`TimedOut` 和 `Failed`
/// 都表示回收结果未知，调用方必须原样上报，不能写成「已确认结束」。这里不带退出状态：
/// 结束原因由主等待环确定。
pub(super) enum WaitOutcome {
    Exited,
    TimedOut,
    Failed(io::Error),
}

/// 已经纳入平台进程树管理的 shell 子进程。
///
/// `owned_by_job` 记录启动时绑定的实际结果，决定回收时用哪种终止动作（见
/// [`ManagedChild::reclaim`]），不必由调用方另外声明一个可能与实际归属不符的事实。
pub(crate) struct ManagedChild {
    pub(super) child: Child,
    job: JobObject,
    owned_by_job: bool,
}

impl ManagedChild {
    /// 回收本次调用的进程树：先做一次终止动作，再做一次有界等待。
    ///
    /// 这是整个工具唯一的回收入口——常规收尾和启动失败都用它，所以两条路径的终止动作、
    /// 等待窗口和失败报告不会各自跑偏。返回值是回收本身的失败文案（终止被拒绝、窗口内
    /// 没退出、等待出错），只作附加信息，不覆盖主要的结束原因。
    ///
    /// 用哪种终止动作取决于实际归属：归属成功时作业对象的整树终止已覆盖主进程，再补
    /// 一次 `Child::kill` 也不改变结果；归属失败时它不在本作业内，只能单独终止。
    pub(super) fn reclaim(&mut self) -> Vec<String> {
        let mut failures = Vec::new();
        let terminated = if self.owned_by_job {
            self.job.terminate()
        } else {
            self.child.kill()
        };
        if let Err(error) = terminated {
            failures.push(format!(
                "failed to terminate the command process tree: {error}"
            ));
        }
        match self.wait_bounded(RECLAIM_GRACE) {
            WaitOutcome::Exited => {}
            WaitOutcome::TimedOut => failures.push(format!(
                "the command process did not exit within {} ms; its exit state is unknown",
                RECLAIM_GRACE.as_millis()
            )),
            WaitOutcome::Failed(error) => {
                failures.push(format!("failed to wait for the command process: {error}"))
            }
        }
        failures
    }

    /// 观察子进程是否已经退出；主等待环和有界回收共用这一个观察点。
    pub(super) fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }

    /// 有界地等子进程结束：窗口内观察到退出就返回已回收，超时和等待失败各自返回
    /// 未知结果，绝不无限阻塞。
    fn wait_bounded(&mut self, timeout: Duration) -> WaitOutcome {
        let deadline = Instant::now() + timeout;
        loop {
            match self.try_wait() {
                Ok(Some(_)) => return WaitOutcome::Exited,
                Ok(None) => {}
                Err(error) => return WaitOutcome::Failed(error),
            }
            if Instant::now() >= deadline {
                return WaitOutcome::TimedOut;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

/// 启动子进程，并把它纳入平台的进程树管理。
///
/// 这个顺序就是契约：先建作业，再用 `CREATE_SUSPENDED` 创建子进程——被挂起的主线程在
/// 恢复之前不会执行任何用户命令，也派生不出下一代——接着绑定作业，最后用本边界自己
/// 持有的初始线程句柄把它恢复。任何一步失败都走同一条回收路径：终止尚未运行的子进程
/// 并释放全部句柄，既不会留下没归属本次作业的后代，也不会把启动失败拖成无限等待。
///
/// 命令行转义、环境与管道仍交给 `std::process::Command`（稳定 `CommandExt` 不提供创建
/// 时的作业属性，见 `PROC_THREAD_ATTRIBUTE_JOB_LIST`）：这些语义只有它实现得完整。
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
    let child = command.spawn()?;
    let process = child.as_raw_handle() as HANDLE;
    // 回收主体先建出来，归属结果初始为 false，只有 assign 成功才改成 true。
    let mut started = ManagedChild {
        child,
        job,
        owned_by_job: false,
    };
    let assigned = started.job.assign(process);
    if assigned.is_ok() {
        started.owned_by_job = true;
    }
    let launched = assigned.and_then(|()| resume_suspended_thread(&started.child));
    if let Err(primary) = launched {
        let failures = started.reclaim();
        return Err(attach_reclaim_failures(primary, &failures));
    }
    Ok(started)
}

/// 恢复被 `CREATE_SUSPENDED` 挂起的初始线程；线程句柄在恢复之后立刻关闭。
fn resume_suspended_thread(child: &Child) -> io::Result<()> {
    let thread = owned_initial_thread(child.id())?;
    let resumed = unsafe { ResumeThread(thread.as_raw_handle() as HANDLE) };
    if resumed == u32::MAX {
        return Err(last_os_error("ResumeThread"));
    }
    Ok(())
}

/// 把回收失败附加到启动错误上：启动失败是主要错误，回收结果只是附加事实。保留原
/// 有的错误类别，调用方仍然能按 `kind` 判断失败原因。
fn attach_reclaim_failures(primary: io::Error, failures: &[String]) -> io::Error {
    if failures.is_empty() {
        return primary;
    }
    io::Error::new(
        primary.kind(),
        format!("{primary}; {}", failures.join("; ")),
    )
}

/// 打开刚创建的子进程的初始线程；句柄归本边界所有，随 `OwnedHandle` 在恢复之后立刻关闭。
///
/// `CREATE_SUSPENDED` 保证这个进程在恢复之前只有一个线程，也不会自己创建线程，所以
/// 快照里属于它的线程就是 `CreateProcess` 建立的主线程。稳定工具链的 `std::process`
/// 不暴露主线程句柄（`main_thread_handle` 在 unstable 的 `process_internals` 之后），
/// 因此这里按进程 id 枚举线程，而不是把整个 `CreateProcessW` 调用重写一遍。
fn owned_initial_thread(process_id: u32) -> io::Result<OwnedHandle> {
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(last_os_error("CreateToolhelp32Snapshot"));
    }
    // 不变量：快照句柄一旦创建成功，所有权随即交给 OwnedHandle。
    let snapshot = unsafe { OwnedHandle::from_raw_handle(snapshot as *mut c_void) };
    let mut entry: THREADENTRY32 = unsafe { std::mem::zeroed() };
    entry.dwSize = size_of::<THREADENTRY32>() as u32;
    let mut found = 0;
    let first = unsafe { Thread32First(snapshot.as_raw_handle() as HANDLE, &mut entry) };
    if first != 0 {
        loop {
            if entry.th32OwnerProcessID == process_id {
                found = entry.th32ThreadID;
                break;
            }
            let next = unsafe { Thread32Next(snapshot.as_raw_handle() as HANDLE, &mut entry) };
            if next == 0 {
                // 返回 0 可能是枚举结束，也可能是真的失败了，必须读错误码来区分；
                // 不能把系统错误当成「这个进程没有线程」。
                if let Some(error) = enumeration_end_error("Thread32Next") {
                    return Err(error);
                }
                break;
            }
        }
    } else if let Some(error) = enumeration_end_error("Thread32First") {
        return Err(error);
    }
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
    // 不变量：OpenThread 一旦成功就返回有效句柄，所有权随即交给 OwnedHandle。
    Ok(unsafe { OwnedHandle::from_raw_handle(thread as *mut c_void) })
}

/// 线程枚举调用返回 0 之后，区分「枚举正常结束」和「真的失败」。
///
/// 官方契约要求调用方读取 GetLastError：`ERROR_NO_MORE_FILES` 表示没有更多条目，属于
/// 正常结束；其他错误码必须带上调用名原样上报。必须在失败的那次调用之后马上调用。
fn enumeration_end_error(operation: &str) -> Option<io::Error> {
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(ERROR_NO_MORE_FILES as i32) {
        return None;
    }
    Some(io::Error::new(
        error.kind(),
        format!("{operation}: {error}"),
    ))
}
