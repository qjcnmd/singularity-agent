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
use std::time::{Duration, Instant};

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
///
/// 句柄由 `OwnedHandle` 独占持有：创建成功即交出所有权，配置失败与析构都走
/// 同一条自动关闭路径，不再有第二个手工释放点。
pub(super) struct JobObject {
    handle: OwnedHandle,
}

impl JobObject {
    pub(super) fn new() -> io::Result<Self> {
        let raw = unsafe { CreateJobObjectW(null(), null()) };
        if raw == 0 {
            return Err(last_os_error("CreateJobObjectW"));
        }
        // 不变量：CreateJobObjectW 成功即返回有效句柄；所有权随即交给 OwnedHandle。
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
            // 先取系统错误再返回：此刻句柄仍有效，错误在句柄关闭前取得。
            return Err(last_os_error("SetInformationJobObject"));
        }
        Ok(Self { handle })
    }

    /// 把尚未恢复运行的子进程绑定进作业；此后它派生的子孙都无法逃逸出整树
    /// 终止范围。
    ///
    /// 绑定失败意味着该进程不在本次作业内（例如已被不允许嵌套的祖先作业占用），
    /// 作业终止对它是空操作，调用方必须改为单独终止它——这个归属事实由调用方
    /// 保存，不能假设绑定总是成功。
    fn assign(&self, process: HANDLE) -> io::Result<()> {
        #[cfg(test)]
        if super::faults::take_assign_failure() {
            return Err(io::Error::other(
                "injected AssignProcessToJobObject failure",
            ));
        }
        let assigned =
            unsafe { AssignProcessToJobObject(self.handle.as_raw_handle() as HANDLE, process) };
        if assigned == 0 {
            return Err(last_os_error("AssignProcessToJobObject"));
        }
        Ok(())
    }

    /// 整树终止：作业对象由内核连带终止所有子孙进程。
    ///
    /// 返回值是内核的实际结果，调用方必须按回收失败报告，不能当作已经终止：
    /// 终止被拒绝时进程树仍然存活。同一动作重复执行不会改变结果，句柄关闭时的
    /// kill-on-close 仍是资源兜底，因此这里不做重试。
    fn terminate(&self) -> io::Result<()> {
        #[cfg(test)]
        if super::faults::take_terminate_failure() {
            return Err(io::Error::other("injected TerminateJobObject failure"));
        }
        let terminated = unsafe { TerminateJobObject(self.handle.as_raw_handle() as HANDLE, 1) };
        if terminated == 0 {
            return Err(last_os_error("TerminateJobObject"));
        }
        Ok(())
    }
}

/// 终止动作之后的有界回收窗口。
///
/// 窗口内没有观察到退出就不再等待，回收结果按未知报告；作业句柄关闭时的
/// kill-on-close 是资源兜底。常规收尾与启动失败共用同一个窗口，两条路径的有界
/// 语义因此一致。
const RECLAIM_GRACE: Duration = Duration::from_secs(5);

/// 有界等待的结果：区分已回收、回收超时与回收错误。
///
/// 三态是契约的一部分：只有 `Exited` 能声称子进程已经结束；`TimedOut` 与
/// `Failed` 都表示回收结果未知，调用方必须原样报告，不得写成已确认结束。这里
/// 不携带退出状态：结束原因由主等待环确定，回收阶段的退出状态不是要报告的事实。
pub(super) enum WaitOutcome {
    Exited,
    TimedOut,
    Failed(io::Error),
}

/// 已纳入平台进程树管理的 shell 子进程。
///
/// `owned_by_job` 是启动时绑定的实际结果，它决定回收时的终止动作（见
/// [`ManagedChild::reclaim`]）。在类型里保存一次，回收时就不再由调用方另行声明
/// 一个可能与实际归属不符的事实。
pub(crate) struct ManagedChild {
    pub(super) child: Child,
    job: JobObject,
    owned_by_job: bool,
}

impl ManagedChild {
    /// 回收本次调用的进程树：一次终止动作，随后一次有界等待。
    ///
    /// 这是全工具唯一的回收入口——常规收尾与启动失败共用它，两条路径的终止动作、
    /// 等待窗口与失败报告因此不会各自漂移。返回值是回收本身的失败文案（终止被
    /// 拒绝、窗口内未退出、等待出错），只作为附加信息，不决定也不覆盖主结束原因。
    ///
    /// 终止动作由实际归属决定：归属成功时作业对象整树终止已经覆盖主进程，再补
    /// 一次 `Child::kill` 不会改变结果；归属失败时它不在本作业内，作业终止对它
    /// 是空操作，只能单独终止它。
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

    /// 观察子进程是否已经退出。主等待环与有界回收共用这一处观察点，等待失败的
    /// 语义因此在两处完全一致。
    pub(super) fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        #[cfg(test)]
        if let Some(error) = super::faults::take_wait_failure() {
            return Err(error);
        }
        self.child.try_wait()
    }

    /// 有界等待子进程结束：窗口内观察到退出即返回已回收，超时与等待失败分别
    /// 返回未知结果，绝不无限阻塞。
    fn wait_bounded(&mut self, timeout: Duration) -> WaitOutcome {
        #[cfg(test)]
        if super::faults::take_wait_expiry() {
            return WaitOutcome::TimedOut;
        }
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

/// 启动子进程并纳入平台进程树管理。
///
/// 顺序即契约：先建作业，再以 `CREATE_SUSPENDED` 创建子进程——被挂起的主线程
/// 在恢复前不会执行任何用户命令，也不能派生下一代——随后绑定作业，最后用本
/// 边界自己持有的初始线程句柄恢复它。任何一步失败都按同一条回收路径终止尚未
/// 运行的子进程并释放全部句柄，因此不会留下可运行的、未归属本次作业的后代，
/// 也不会把启动失败拖成无界等待。
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
    let child = command.spawn()?;
    let process = child.as_raw_handle() as HANDLE;
    // 归属先于恢复：只有归属成功的进程才允许执行用户命令。归属失败时保持挂起，
    // 不恢复一个不属于本次作业的进程。
    let assigned = job.assign(process);
    let owned_by_job = assigned.is_ok();
    let resumed = if owned_by_job {
        resume_suspended_thread(&child)
    } else {
        Ok(())
    };
    let primary = match (assigned, resumed) {
        (Ok(()), Ok(())) => {
            return Ok(ManagedChild {
                child,
                job,
                owned_by_job,
            });
        }
        (Err(error), _) => error,
        (Ok(()), Err(error)) => error,
    };
    // 启动失败不另写清理路径：复用常规回收的同一个入口，同样只等一个有界窗口。
    // 主错误是启动失败本身，回收失败只作为附加信息跟在它后面。
    let mut failed = ManagedChild {
        child,
        job,
        owned_by_job,
    };
    let failures = failed.reclaim();
    Err(attach_reclaim_failures(primary, &failures))
}

/// 恢复被 `CREATE_SUSPENDED` 挂起的初始线程；线程句柄在恢复后立即关闭。
fn resume_suspended_thread(child: &Child) -> io::Result<()> {
    #[cfg(test)]
    if super::faults::take_resume_failure() {
        return Err(io::Error::other("injected ResumeThread failure"));
    }
    let thread = owned_initial_thread(child.id())?;
    let resumed = unsafe { ResumeThread(thread.as_raw_handle() as HANDLE) };
    if resumed == u32::MAX {
        return Err(last_os_error("ResumeThread"));
    }
    Ok(())
}

/// 把回收失败附加到启动错误上：启动失败是主错误，回收结果只是附加事实。保留原
/// 错误类别，调用方仍能按 `kind` 判断失败原因。
fn attach_reclaim_failures(primary: io::Error, failures: &[String]) -> io::Error {
    if failures.is_empty() {
        return primary;
    }
    io::Error::new(
        primary.kind(),
        format!("{primary}; {}", failures.join("; ")),
    )
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

/// 启动边界的测试：注入点见 [`crate::tools::bash::faults`]，它们让内核拒绝调用
/// 才可能出现的失败分支与常规路径一样可断言。
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::path::Path;
    use std::time::{Duration, Instant};

    use super::super::faults;
    use super::spawn_in_job;

    /// 用 `cmd.exe` 启动一条会写标记文件的命令：标记文件出现就说明子进程被恢复
    /// 并真正执行过用户命令，没出现则说明它一直停在挂起状态。
    ///
    /// 这里不用 bash：本模块测试的是启动与回收边界，与后端 shell 无关，`cmd.exe`
    /// 在任何 Windows 上都存在，且 `echo` 是内建命令，不会额外派生子进程。
    fn launch(dir: &Path, marker: &Path) -> std::io::Result<super::ManagedChild> {
        spawn_in_job(
            "cmd.exe",
            &["/c".to_string(), format!("echo ran > {}", marker.display())],
            dir,
        )
    }

    /// 绑定失败是启动错误，不是回收错误：主错误保留绑定失败本身，未归属本次作业
    /// 的子进程不得被恢复执行，回收走与常规收尾同一个有界入口。
    #[test]
    fn an_assign_failure_keeps_the_launch_error_and_does_not_run_the_child() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("ran.txt");
        faults::fail_next_assign();
        let started = Instant::now();
        let error = match launch(dir.path(), &marker) {
            Ok(_) => panic!("an injected assignment failure must fail the launch"),
            Err(error) => error,
        };
        assert!(
            error.to_string().contains("AssignProcessToJobObject"),
            "{error}"
        );
        assert!(
            !marker.exists(),
            "a child outside the job must not be resumed"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "launch cleanup must be bounded, took {:?}",
            started.elapsed()
        );
    }

    /// 恢复失败：子进程已经归属本次作业，清理由作业整树终止承担；启动错误仍是
    /// 主错误，用户命令同样没有执行。
    #[test]
    fn a_resume_failure_reclaims_through_the_job_and_keeps_the_launch_error() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("ran.txt");
        faults::fail_next_resume();
        let started = Instant::now();
        let error = match launch(dir.path(), &marker) {
            Ok(_) => panic!("an injected resume failure must fail the launch"),
            Err(error) => error,
        };
        let text = error.to_string();
        assert!(text.contains("ResumeThread"), "{text}");
        assert!(
            !text.contains("failed to terminate"),
            "the job-owned child is reclaimed by the tree termination: {text}"
        );
        assert!(!marker.exists(), "a suspended child must not be resumed");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "launch cleanup must be bounded, took {:?}",
            started.elapsed()
        );
    }

    /// 启动失败的清理失败同样有界，且不覆盖启动错误：终止被拒绝时窗口到点即报告
    /// 回收结果未知，而不是退化成无界等待，也不是把未知结果写成已确认结束。
    #[test]
    fn a_failed_launch_cleanup_is_bounded_and_keeps_the_launch_error() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("ran.txt");
        faults::fail_next_resume();
        faults::fail_next_terminate();
        let started = Instant::now();
        let error = match launch(dir.path(), &marker) {
            Ok(_) => panic!("an injected resume failure must fail the launch"),
            Err(error) => error,
        };
        let text = error.to_string();
        assert!(text.contains("ResumeThread"), "{text}");
        assert!(
            text.contains("failed to terminate the command process tree"),
            "{text}"
        );
        assert!(text.contains("did not exit within"), "{text}");
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "a failed cleanup must still be bounded, took {:?}",
            started.elapsed()
        );
    }

    /// 成功启动的对照：被恢复的子进程真的会执行命令并写出标记文件，回收也在
    /// 窗口内观察到退出。没有这个对照，上面两处「标记文件不存在」的断言可能只是
    /// 命令本身没生效，而不是子进程没被执行。
    #[test]
    fn a_successful_launch_runs_the_command_and_exits_within_the_window() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("ran.txt");
        let mut started = launch(dir.path(), &marker).unwrap();
        assert!(
            matches!(
                started.wait_bounded(Duration::from_secs(10)),
                super::WaitOutcome::Exited
            ),
            "a resumed child must exit within the window"
        );
        assert!(marker.exists(), "the control launch must run the command");
    }
}
