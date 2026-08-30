//! bash 工具执行环：进程树管理、主等待/排空循环与退出状态投影。

use std::io;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(windows)]
use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;

use crate::tools::registry::{ABORTED_MESSAGE, ExecuteContext, ToolExecution, error_result};

use super::capture::{CaptureState, command_slug};
#[cfg(windows)]
use super::job_object;
use super::pump::pump_output;
use super::shell::shell_command;
use super::spec::BashArgs;

/// 输出分块读取管道的容量上限。
const OUTPUT_QUEUE_CAPACITY: usize = 32;
/// 主等待环与排空阶段的轮询切片：recv_timeout 粗粒度醒来检查取消、超时与退出。
const OUTPUT_POLL_INTERVAL: Duration = Duration::from_millis(25);
/// 子进程退出后排空残留缓冲输出的宽限，超时则停止 pump 并标记截断。
const OUTPUT_DRAIN_GRACE: Duration = Duration::from_millis(2_000);
/// 后台进程仍持有管道写端导致输出被截断时的可见标记。
pub(super) const OUTPUT_TRUNCATED_BACKGROUND_NOTE: &str =
    "[output truncated: a background process is still writing]";
/// 进程终止后的有界回收窗口。
const WAIT_GRACE: Duration = Duration::from_secs(5);

/// 命令执行超时仅在显式提供 `timeout_ms`（正整数毫秒）时生效；未提供时不主动超时。
pub(crate) const DESCRIPTION: &str = "Execute a bash command in the current working directory. Returns stdout and stderr. Output is truncated to last 2000 lines or 50KB (whichever is hit first); when truncated, the full output is saved to a temp file and its path is appended as a `Full output:` line. Provide timeout_ms to bound execution; without it a command runs until completion or interruption.";

pub(crate) fn execute(args: &BashArgs, ctx: ExecuteContext<'_>) -> ToolExecution {
    let ExecuteContext {
        cwd,
        signal,
        mut on_update,
        ..
    } = ctx;
    let command = args.command.clone();
    let timeout = args.timeout_ms;
    let (shell, shell_args) = match shell_command(&command) {
        Ok(command) => command,
        Err(error) => return error_result(error),
    };
    let mut managed = match spawn_shell(&shell, &shell_args, cwd) {
        Ok(child) => child,
        Err(error) => {
            return error_result(format!("failed to spawn shell {shell}: {error}"));
        }
    };
    // 不变量：spawn_shell 配置了 piped stdout/stderr，take 必为 Some。
    #[allow(clippy::expect_used)]
    let stdout = managed.child.stdout.take().expect("bash stdout is piped");
    #[allow(clippy::expect_used)]
    let stderr = managed.child.stderr.take().expect("bash stderr is piped");
    let (sender, receiver) = mpsc::sync_channel(OUTPUT_QUEUE_CAPACITY);
    let stderr_sender = sender.clone();
    // 每个 pump 线程做有界读（见 pump_output）：即使后台进程拿住管道写端造成
    // 阻塞，stop 标志也会让线程在宽限后确定收敛；JoinHandle 仍被丢弃（detach）。
    let stop = Arc::new(AtomicBool::new(false));
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        let stdout_wait = stdout.as_raw_fd();
        let stderr_wait = stderr.as_raw_fd();
        let stdout_stop = Arc::clone(&stop);
        let stderr_stop = Arc::clone(&stop);
        thread::spawn(move || pump_output(stdout, sender, stdout_stop, stdout_wait));
        thread::spawn(move || pump_output(stderr, stderr_sender, stderr_stop, stderr_wait));
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        let stdout_wait = stdout.as_raw_handle() as isize;
        let stderr_wait = stderr.as_raw_handle() as isize;
        let stdout_stop = Arc::clone(&stop);
        let stderr_stop = Arc::clone(&stop);
        thread::spawn(move || pump_output(stdout, sender, stdout_stop, stdout_wait));
        thread::spawn(move || pump_output(stderr, stderr_sender, stderr_stop, stderr_wait));
    }

    let mut state = CaptureState {
        command_slug: command_slug(&command),
        ..CaptureState::default()
    };
    let deadline = timeout.map(|ms| Instant::now() + Duration::from_millis(ms));
    // 主等待环的每条退出路径都恰好回收一次退出状态或直接返回错误。
    let exit_status;
    let mut outcome = BashOutcome::Completed;
    let mut readers_drained = false;
    // 运行阶段：按粗粒度切片等待输出块，并在每次醒来的间隙检查取消与超时。
    // 双泵 EOF（Disconnected）只说明管道已关闭；退出状态仍必须从子进程回收，
    // 此后改为纯定时轮询直到 try_wait 观察到退出。
    loop {
        if !readers_drained {
            match receiver.recv_timeout(OUTPUT_POLL_INTERVAL) {
                Ok(chunk) => ingest_chunk(&mut state, &chunk, &mut on_update),
                Err(RecvTimeoutError::Disconnected) => readers_drained = true,
                Err(RecvTimeoutError::Timeout) => {}
            }
        } else {
            thread::sleep(OUTPUT_POLL_INTERVAL);
        }
        if let Some(signal) = signal
            && signal.is_cancelled()
        {
            managed.kill_tree();
            outcome = BashOutcome::Aborted;
            exit_status = wait_for_exit(&mut managed);
            break;
        }
        if let Some(deadline) = deadline
            && Instant::now() >= deadline
        {
            managed.kill_tree();
            // 不变量：deadline 存在时 timeout 必存在。
            #[allow(clippy::expect_used)]
            let timed_out = BashOutcome::TimedOut(timeout.expect("deadline implies timeout"));
            outcome = timed_out;
            exit_status = wait_for_exit(&mut managed);
            break;
        }
        match managed.try_wait() {
            Ok(Some(status)) => {
                exit_status = Some(status);
                break;
            }
            Ok(None) => {}
            Err(error) => {
                managed.kill_tree();
                let _ = wait_for_exit(&mut managed);
                return error_result(format!("failed to wait for child process: {error}"));
            }
        }
    }
    // 排空阶段：主进程已退出（或已被整树终止），但管道中可能仍有缓冲输出，
    // 或子进程树成员仍持有写端。读至所有发送端关闭（EOF）为止，最长宽限
    // OUTPUT_DRAIN_GRACE；超时说明后台进程仍持有写端，此时停止 pump 并把
    // 输出标记为截断，线程随后确定收敛。
    let mut output_truncated_by_background = false;
    if !readers_drained {
        let grace_deadline = Instant::now() + OUTPUT_DRAIN_GRACE;
        loop {
            match receiver.recv_timeout(OUTPUT_POLL_INTERVAL) {
                Ok(chunk) => ingest_chunk(&mut state, &chunk, &mut on_update),
                Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => {}
            }
            if Instant::now() >= grace_deadline {
                stop.store(true, Ordering::SeqCst);
                let converge = Instant::now() + OUTPUT_DRAIN_GRACE;
                while let Some(remaining) = converge.checked_duration_since(Instant::now()) {
                    match receiver.recv_timeout(remaining.min(OUTPUT_POLL_INTERVAL)) {
                        Ok(chunk) => ingest_chunk(&mut state, &chunk, &mut on_update),
                        Err(RecvTimeoutError::Disconnected) => break,
                        Err(RecvTimeoutError::Timeout) => {}
                    }
                }
                output_truncated_by_background = true;
                break;
            }
        }
    }

    let progress = state.final_progress();
    let mut content = progress.output_text;
    if let Some(note) = progress.note {
        content.push_str("\n\n");
        content.push_str(&note);
    }
    let mut is_error = false;
    match outcome {
        BashOutcome::Aborted => {
            append_status(&mut content, ABORTED_MESSAGE);
            is_error = true;
        }
        BashOutcome::TimedOut(ms) => {
            append_status(&mut content, &format!("Command timed out after {ms} ms"));
            is_error = true;
        }
        BashOutcome::Completed => match exit_status {
            // 进程正常结束（无信号且退出码为 0）判定为成功。
            Some(status) if status.success() => {
                if content.is_empty() {
                    content = "(no output)".to_string();
                }
            }
            Some(status) => {
                append_status(&mut content, &describe_exit(status));
                is_error = true;
            }
            // 完成路径的退出状态必然已回收；缺失时按失败报告，不伪装成功。
            None => {
                append_status(&mut content, "Command exited without a status");
                is_error = true;
            }
        },
    }
    if output_truncated_by_background {
        // 后台进程仍持有管道写端；命令本身已结束，截断仅为信息提示而非错误。
        append_status(&mut content, OUTPUT_TRUNCATED_BACKGROUND_NOTE);
    }
    // 截断实际发生（最终裁剪或内部窗口丢弃过字节）时，保证完整输出已落盘
    // 并在结果尾部附路径；spill 创建失败时保持无路径的旧行为。
    state.ensure_spill_for_final_truncation();
    if let Some(spill_path) = state.spill_path() {
        append_status(
            &mut content,
            &format!("Full output: {}", spill_path.display()),
        );
    }
    if let Some(callback) = on_update.as_mut() {
        callback(&state.current_output());
    }
    ToolExecution { content, is_error }
}

fn ingest_chunk(
    state: &mut CaptureState,
    chunk: &str,
    on_update: &mut Option<&mut dyn FnMut(&str)>,
) {
    state.ingest(chunk);
    if let Some(callback) = on_update.as_mut() {
        callback(&state.current_output());
    }
}

fn append_status(content: &mut String, status: &str) {
    if !content.is_empty() {
        content.push_str("\n\n");
    }
    content.push_str(status);
}

/// 把失败退出状态投影为错误文案：Unix 上被信号终止时报告信号号，
/// 其余情况报告退出码。
fn describe_exit(status: ExitStatus) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return format!("Command terminated by signal {signal}");
        }
    }
    match status.code() {
        Some(code) => format!("Command exited with code {code}"),
        None => "Command terminated".to_string(),
    }
}

enum BashOutcome {
    Completed,
    Aborted,
    TimedOut(u64),
}

/// 已纳入平台进程树管理的 shell 子进程。
///
/// 终止必须走 [`Self::kill_tree`]：它同时作用于平台的整树机制（Job Object /
/// 进程组）和主进程自身，保证没有孤儿存活。
pub(super) struct ManagedChild {
    pub(super) child: Child,
    #[cfg(windows)]
    job: job_object::JobObject,
}

impl ManagedChild {
    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }

    /// 整树终止：
    /// - Windows：Job Object 内核级连带原子终止所有子孙进程；
    /// - Unix：向创建时绑定的独立进程组广播 SIGKILL。
    ///
    /// 随后对主进程补一次 kill，确保句柄状态确定收敛。
    pub(super) fn kill_tree(&mut self) {
        #[cfg(windows)]
        {
            let _ = self.job.terminate(1);
        }
        #[cfg(unix)]
        {
            // 负数 pid 定向 spawn 时绑定的独立进程组（process_group(0)），
            // 直接经 libc 发信号，不依赖外部 kill 二进制与 PATH。
            let pid = self.child.id() as i32;
            #[allow(unsafe_code)]
            // Unix 整树终止经 libc::kill 向进程组发 SIGKILL，与平台的底层能力一致。
            unsafe {
                libc::kill(-pid, libc::SIGKILL);
            }
        }
        let _ = self.child.kill();
    }

    /// 有界回收子进程（默认 5 秒，超时放弃），避免残留句柄无限阻塞。
    pub(super) fn wait_bounded(&mut self, timeout: Duration) -> Option<ExitStatus> {
        let deadline = Instant::now() + timeout;
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => return Some(status),
                Ok(None) => {}
                Err(_) => return None,
            }
            if Instant::now() >= deadline {
                return None;
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
}

/// 启动 shell 子进程并纳入平台进程树管理：
/// Windows 先建 `KILL_ON_JOB_CLOSE` 作业再 spawn、成功后立即绑定，绑定失败则
/// 杀掉刚启动的进程；Unix 以独立进程组 spawn。两条路径都保证可整树终止。
pub(super) fn spawn_shell(
    shell: &str,
    shell_args: &[String],
    cwd: &Path,
) -> io::Result<ManagedChild> {
    let mut command = Command::new(shell);
    command
        .args(shell_args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    #[cfg(windows)]
    {
        // 先建作业：作业创建失败时不会留下任何未受管子进程。
        let job = job_object::JobObject::new()?;
        let mut child = command.spawn()?;
        if let Err(error) = job.assign(&child) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
        Ok(ManagedChild { child, job })
    }
    #[cfg(not(windows))]
    {
        let child = command.spawn()?;
        Ok(ManagedChild { child })
    }
}

fn wait_for_exit(managed: &mut ManagedChild) -> Option<ExitStatus> {
    managed.wait_bounded(WAIT_GRACE)
}
