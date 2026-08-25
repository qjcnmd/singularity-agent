//! bash 工具：在当前工作目录下执行 Shell 命令行指令。
//!
//! - **超时控制**：仅在显式提供 `timeout_ms` 时生效（正整数毫秒），到点强制终止整棵子进程树并返回超时错误；未提供时不主动超时。
//! - **输出流式捕获与截断**：标准输出（stdout）与标准错误（stderr）合并捕获；结果输出保留尾部最后 2000 行 / 50KB，截断实际发生时完整输出写入临时文件并在结果尾部附 `Full output: <路径>`。
//! - **中断处理**：收到外部取消信号（`CancellationToken`）时立即终止进程树并返回 `Command aborted`。
//! - **进程树隔离**：Windows 将子进程绑定到 `KILL_ON_JOB_CLOSE` 的 Job Object，Unix 使用独立进程组；两条路径都支持内核级整树终止。

use std::io::{self, Read, Write};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Deserializer, de::Error as _};
use uuid::Uuid;

#[cfg(windows)]
use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;

#[cfg(windows)]
#[allow(unsafe_code)] // Windows 平台进程树终止的内核 API 集中在此模块。
mod job_object {
    //! 进程树终止的内核边界：`KILL_ON_JOB_CLOSE` 作业对象的 RAII 封装。
    //!
    //! 子进程一经绑定，其派生的全部子孙都留在同一作业内；关闭作业句柄或显式
    //! 终止都会由内核连带杀死整棵树，不依赖逐个枚举进程。

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
}

use serde_json::{Value, json};

use super::registry::{
    ExecuteContext, ToolError, ToolExecution, deserialize_args_or_error, error_result,
    validate_args,
};
use super::truncate::{
    DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, TruncatedBy, format_size, truncate_tail,
};

/// 内存中保留的尾部缓冲区字节上限（100KB），防止超大单行输出耗尽内存。
const INTERNAL_TAIL_MAX_BYTES: usize = DEFAULT_MAX_BYTES * 2;
/// 输出分块读取管道的容量上限。
const OUTPUT_QUEUE_CAPACITY: usize = 32;
/// 主等待环与排空阶段的轮询切片：recv_timeout 粗粒度醒来检查取消、超时与退出。
const OUTPUT_POLL_INTERVAL: Duration = Duration::from_millis(25);
/// pump 有界读的等待切片：无数据且未 EOF 时按此周期醒来检查停止标志。
const OUTPUT_PIPE_READ_TIMEOUT: Duration = Duration::from_millis(200);
/// 子进程退出后排空残留缓冲输出的宽限，超时则停止 pump 并标记截断。
const OUTPUT_DRAIN_GRACE: Duration = Duration::from_millis(2_000);
/// 后台进程仍持有管道写端导致输出被截断时的可见标记。
const OUTPUT_TRUNCATED_BACKGROUND_NOTE: &str =
    "[output truncated: a background process is still writing]";

/// 命令执行超时仅在显式提供 `timeout_ms`（正整数毫秒）时生效；未提供时不主动超时。
pub(crate) const DESCRIPTION: &str = "Execute a bash command in the current working directory. Returns stdout and stderr. Output is truncated to last 2000 lines or 50KB (whichever is hit first); when truncated, the full output is saved to a temp file and its path is appended as a `Full output:` line. Provide timeout_ms to bound execution; without it a command runs until completion or interruption.";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BashArgs {
    command: String,
    #[serde(default, deserialize_with = "deserialize_timeout_ms")]
    timeout_ms: Option<u64>,
}

fn deserialize_timeout_ms<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    value
        .as_u64()
        .filter(|timeout| *timeout > 0)
        .map(Some)
        .ok_or_else(|| D::Error::custom("invalid timeout_ms: must be a positive integer"))
}

pub(crate) fn parameters() -> Value {
    json!({
        "type": "object",
        "properties": {
            "command": { "type": "string", "description": "Bash command to execute" },
            "timeout_ms": {
                "type": "integer",
                "minimum": 1,
                "description": "Timeout in milliseconds (optional; omit to run without a timeout)"
            },
        },
        "required": ["command"],
        "additionalProperties": false,
    })
}

pub(crate) fn spec() -> super::registry::ToolSpec {
    super::registry::ToolSpec {
        name: "bash",
        description: DESCRIPTION,
        parameters: parameters(),
        validate: validate_args::<BashArgs>,
        execute,
    }
}

pub(crate) fn execute(ctx: ExecuteContext<'_>) -> Result<ToolExecution, ToolError> {
    let ExecuteContext {
        args: raw_args,
        cwd,
        signal,
        mut on_update,
    } = ctx;
    let args = match deserialize_args_or_error::<BashArgs>(&raw_args) {
        Ok(args) => args,
        Err(execution) => return Ok(execution),
    };
    let command = args.command;
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
    let stdout = managed.child.stdout.take().expect("bash stdout is piped");
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
            outcome = BashOutcome::TimedOut(timeout.expect("deadline implies timeout"));
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
            append_status(&mut content, "Command aborted");
            is_error = true;
        }
        BashOutcome::TimedOut(ms) => {
            append_status(&mut content, &format!("Command timed out after {ms} ms"));
            is_error = true;
        }
        BashOutcome::Completed => match exit_status.and_then(|status| status.code()) {
            // 进程正常结束（退出码为 0 或不可得时判定为成功）。
            Some(0) | None => {
                if content.is_empty() {
                    content = "(no output)".to_string();
                }
            }
            Some(code) => {
                append_status(&mut content, &format!("Command exited with code {code}"));
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
    Ok(ToolExecution { content, is_error })
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

enum BashOutcome {
    Completed,
    Aborted,
    TimedOut(u64),
}

/// 已纳入平台进程树管理的 shell 子进程。
///
/// 终止必须走 [`Self::kill_tree`]：它同时作用于平台的整树机制（Job Object /
/// 进程组）和主进程自身，保证没有孤儿存活。
struct ManagedChild {
    child: Child,
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
    fn kill_tree(&mut self) {
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
    fn wait_bounded(&mut self, timeout: Duration) -> Option<ExitStatus> {
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
fn spawn_shell(shell: &str, shell_args: &[String], cwd: &Path) -> io::Result<ManagedChild> {
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
    managed.wait_bounded(Duration::from_secs(5))
}

/// 用于有界等待管道可读性的平台句柄。
#[cfg(unix)]
type PipeWait = std::os::unix::io::RawFd;
#[cfg(windows)]
type PipeWait = isize;

/// 有界等待管道可读性：返回 true 表示可立即读取（有数据或已 EOF/断开），
/// false 表示在 `timeout` 内既无数据也未 EOF（后台进程可能仍持有写端）。
#[cfg(unix)]
#[allow(unsafe_code)] // Unix 使用 libc::poll 做有界读等待，与平台的底层能力一致。
fn wait_pipe_readable(wait: PipeWait, timeout: Duration) -> bool {
    let mut descriptor = libc::pollfd {
        fd: wait,
        events: libc::POLLIN,
        revents: 0,
    };
    let timeout_ms = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX) as libc::c_int;
    loop {
        let result = unsafe { libc::poll(&mut descriptor as *mut _, 1, timeout_ms) };
        if result < 0 {
            // EINTR 后重试；其余错误交由随后的 read() 报告真实原因。
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return true;
        }
        return result > 0;
    }
}

/// 有界等待管道可读性（Windows：`WaitForSingleObject` 对匿名管道句柄不是
/// 可靠的可读信号——句柄并非可等待对象时调用直接失败，pump 将永远等不到
/// 数据；改用 `PeekNamedPipe` 非破坏性查询待读字节与断开状态）。
#[cfg(windows)]
#[allow(unsafe_code)] // Windows 管道可读性经 PeekNamedPipe 查询，与平台的底层能力一致。
fn wait_pipe_readable(wait: PipeWait, timeout: Duration) -> bool {
    use windows_sys::Win32::Foundation::{ERROR_BROKEN_PIPE, ERROR_NO_DATA, GetLastError};
    use windows_sys::Win32::System::Pipes::PeekNamedPipe;
    let mut available: u32 = 0;
    let peek_result = unsafe {
        let ok = PeekNamedPipe(
            wait as _,
            core::ptr::null_mut(),
            0,
            core::ptr::null_mut(),
            &mut available,
            core::ptr::null_mut(),
        ) != 0;
        if ok {
            Ok(available)
        } else {
            Err(GetLastError())
        }
    };
    match peek_result {
        // 有待读字节：立即读取。
        Ok(available) if available > 0 => return true,
        // 写端已关闭或管道正在关闭：立即放行，由 read() 报告 EOF 或真实错误。
        Err(error) if error == ERROR_BROKEN_PIPE || error == ERROR_NO_DATA => return true,
        _ => {}
    }
    // 无数据且未断开：按切片节奏轮询，保持 stop 标志的收敛语义。
    thread::sleep(timeout);
    false
}

/// 从管道读取字节流，过滤控制字符并按块发送至通道。
///
/// 每次读取前有界等待管道可读性；`stop` 置位后在线程下一个等待切片内收敛，
/// 因此即使后台进程一直持有管道写端，线程也必会结束而不会无限阻塞。
fn pump_output(
    mut reader: impl Read + Send + 'static,
    sender: mpsc::SyncSender<String>,
    stop: Arc<AtomicBool>,
    wait: PipeWait,
) {
    let mut decoder = Utf8Decoder::default();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        if !wait_pipe_readable(wait, OUTPUT_PIPE_READ_TIMEOUT) {
            // 无数据且未 EOF：回到循环头重新检查停止标志。
            continue;
        }
        match reader.read(&mut buffer) {
            Ok(0) => {
                let text = decoder.decode(&[], true);
                if !text.is_empty() && sender.send(text).is_err() {
                    break;
                }
                break;
            }
            Ok(read) => {
                let text = decoder.decode(&buffer[..read], false);
                if !text.is_empty() && sender.send(text).is_err() {
                    break;
                }
            }
            // A read error is not a real EOF.  Keep the incomplete carry
            // private to this stream and do not synthesize a replacement byte
            // for a pipe that was interrupted or closed abnormally.
            Err(_) => break,
        }
    }
}

/// 过滤不可见的控制字符（保留 `\t`、`\n`、`\r`），其余字节按 UTF-8 进行安全解码。
#[derive(Default)]
struct Utf8Decoder {
    pending: Vec<u8>,
}

impl Utf8Decoder {
    fn decode(&mut self, bytes: &[u8], eof: bool) -> String {
        self.pending.extend_from_slice(bytes);
        let mut output = String::new();
        loop {
            match std::str::from_utf8(&self.pending) {
                Ok(text) => {
                    output.push_str(&sanitize_decoded_output(text));
                    self.pending.clear();
                    break;
                }
                Err(error) => {
                    let valid = error.valid_up_to();
                    if valid > 0 {
                        let text = std::str::from_utf8(&self.pending[..valid])
                            .expect("valid_up_to must describe valid UTF-8");
                        output.push_str(&sanitize_decoded_output(text));
                        self.pending.drain(..valid);
                    }
                    if let Some(error_len) = error.error_len() {
                        output.push('\u{FFFD}');
                        self.pending.drain(..error_len);
                        continue;
                    }
                    if eof {
                        output.push('\u{FFFD}');
                        self.pending.clear();
                    }
                    break;
                }
            }
        }
        output
    }
}

fn sanitize_decoded_output(text: &str) -> String {
    text.chars()
        .filter(|character| matches!(character, '\t' | '\n' | '\r') || (*character as u32) > 0x1f)
        .filter(|&character| character != '\r')
        .collect()
}

/// 根据宿主系统环境选择合适的 Shell 执行命令：
/// Windows 严格使用发现的 Git Bash 或 PATH 中的 bash.exe（绝不回退至 cmd.exe）；
/// Unix 环境优先使用 `/bin/bash`，回退使用 `sh`。
fn shell_command(command: &str) -> Result<(String, Vec<String>), String> {
    #[cfg(windows)]
    {
        bash_shell_command(command, find_bash_on_windows())
    }
    #[cfg(not(windows))]
    {
        if Path::new("/bin/bash").exists() {
            return Ok((
                "/bin/bash".to_string(),
                vec!["-c".to_string(), command.to_string()],
            ));
        }
        Ok((
            "sh".to_string(),
            vec!["-c".to_string(), command.to_string()],
        ))
    }
}

/// Validate the shell prerequisite once at a process entry point using the
/// same discovery rules as the bash tool.
pub fn ensure_available() -> Result<(), String> {
    shell_command(":").map(|_| ())
}

#[cfg(windows)]
fn bash_shell_command(
    command: &str,
    bash: Option<String>,
) -> Result<(String, Vec<String>), String> {
    let Some(bash) = bash else {
        return Err(
            "Git Bash is required but bash.exe was not found. Install Git for Windows from https://git-scm.com/install/windows, or add the Git bin directory containing bash.exe to PATH."
                .to_string(),
        );
    };
    Ok((bash, vec!["-c".to_string(), command.to_string()]))
}

#[cfg(windows)]
fn find_bash_on_windows() -> Option<String> {
    let mut candidates = Vec::new();
    for var in ["ProgramFiles", "ProgramFiles(x86)", "ProgramW6432"] {
        if let Ok(program_files) = std::env::var(var) {
            candidates.push(format!("{program_files}\\Git\\bin\\bash.exe"));
        }
    }
    if let Ok(path) = std::env::var("PATH") {
        for dir in path.split(';') {
            if dir.is_empty() {
                continue;
            }
            let candidate = Path::new(dir).join("bash.exe");
            // System32 下的 bash.exe 是 WSL 启动器存根：路径语义、进程模型与
            // Unix shell 完全不同，且在无发行版/服务未运行的环境中静默无输出，
            // 绝不能作为 bash 工具的执行后端。
            if candidate.starts_with(std::env::var("SystemRoot").unwrap_or_default())
                && candidate.ends_with("System32\\bash.exe")
            {
                continue;
            }
            candidates.push(candidate.display().to_string());
        }
    }
    candidates
        .into_iter()
        .find(|candidate| Path::new(candidate).is_file())
}

/// 截断发生时保存完整输出的临时文件写入器。位于
/// `<TEMP>/singularity-tool-output/<uuid>/<命令slug>.log`，不主动清理
/// 创建新 spill 时惰性删除同根目录下超过七天的旧文件。
struct SpillWriter {
    path: std::path::PathBuf,
    file: std::fs::File,
}

impl SpillWriter {
    /// 以 `initial` 为完整初始内容创建 spill 文件。
    fn create(slug: &str, initial: &str) -> io::Result<Self> {
        let root = std::env::temp_dir().join("singularity-tool-output");
        std::fs::create_dir_all(&root)?;
        cleanup_old_spills(&root, std::time::SystemTime::now());
        let dir = root.join(Uuid::new_v4().to_string());
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{slug}.log"));
        let mut file = std::fs::File::create(&path)?;
        file.write_all(initial.as_bytes())?;
        Ok(Self { path, file })
    }

    fn append(&mut self, text: &str) -> io::Result<()> {
        self.file.write_all(text.as_bytes())
    }
}

const SPILL_RETENTION: std::time::Duration = std::time::Duration::from_secs(7 * 24 * 60 * 60);

fn cleanup_old_spills(root: &std::path::Path, now: std::time::SystemTime) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        let Ok(age) = now.duration_since(modified) else {
            continue;
        };
        if age <= SPILL_RETENTION {
            continue;
        }
        if metadata.is_file() {
            let _ = std::fs::remove_file(path);
        } else if metadata.is_dir() {
            let _ = std::fs::remove_dir_all(path);
        }
    }
}

/// 把命令文本投影为文件名安全的 slug（ASCII 字母数字与 `-_.`，其余折叠为
/// `-`，去除首尾 `-`，最长 40 字符）。
fn command_slug(command: &str) -> String {
    let mut slug: String = command
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric()
                || character == '-'
                || character == '_'
                || character == '.'
            {
                character
            } else {
                '-'
            }
        })
        .collect();
    slug = slug.trim_matches('-').to_string();
    if slug.is_empty() {
        slug = "command".to_string();
    }
    slug.truncate(40);
    slug
}

/// 累计输出状态：尾部缓冲（上限 2×50KB）、行/字节计数。超出展示上限的输出
/// 只保留尾部缓冲；首次丢弃字节前创建 spill 文件保存完整输出，其后每个
/// chunk 同步追加，保证截断时完整输出可从 spill 恢复。
#[derive(Default)]
struct CaptureState {
    tail: String,
    total_bytes: usize,
    completed_lines: usize,
    has_open_line: bool,
    current_line_bytes: usize,
    spill: Option<SpillWriter>,
    spill_failed: bool,
    command_slug: String,
}

impl CaptureState {
    fn total_lines(&self) -> usize {
        self.completed_lines + usize::from(self.has_open_line)
    }

    fn is_truncated(&self) -> bool {
        self.total_lines() > DEFAULT_MAX_LINES || self.total_bytes > DEFAULT_MAX_BYTES
    }

    /// spill 文件路径（已成功创建时）。
    fn spill_path(&self) -> Option<&std::path::Path> {
        self.spill.as_ref().map(|spill| spill.path.as_path())
    }

    /// 确保完整输出已在落盘通道中：成功一次后为 no-op，失败一次后不再重试。
    /// 必须在尾部缓冲丢弃任何字节之前调用，写入的才是完整输出。
    fn ensure_spill(&mut self, initial: &str) {
        if self.spill.is_some() || self.spill_failed {
            return;
        }
        match SpillWriter::create(&self.command_slug, initial) {
            Ok(writer) => self.spill = Some(writer),
            Err(_) => self.spill_failed = true,
        }
    }

    /// 当前应展示给流式回调的输出（超限时为截断尾部）。
    fn current_output(&self) -> String {
        if self.is_truncated() {
            truncate_tail(&self.tail).content
        } else {
            self.tail.clone()
        }
    }

    /// 吸收一个清洗后的 chunk：更新计数与尾部缓冲。
    fn ingest(&mut self, text: &str) {
        self.total_bytes += text.len();
        self.completed_lines += text.bytes().filter(|byte| *byte == b'\n').count();
        match text.rfind('\n') {
            Some(last_newline) => {
                let trailing = &text[last_newline + 1..];
                self.current_line_bytes = trailing.len();
                self.has_open_line = !trailing.is_empty();
            }
            None => {
                self.current_line_bytes += text.len();
                self.has_open_line = true;
            }
        }
        if let Some(spill) = self.spill.as_mut() {
            let _ = spill.append(text);
        }
        self.tail.push_str(text);
        if self.tail.len() > INTERNAL_TAIL_MAX_BYTES {
            // 首次丢弃前保存完整窗口；此后完整输出只存在于 spill。
            self.ensure_spill(&self.tail.clone());
            trim_to_last_bytes(&mut self.tail, INTERNAL_TAIL_MAX_BYTES);
        }
    }

    /// 截断已发生且 spill 尚未启用（最终裁剪型截断，尾部缓冲从未丢弃字节）
    /// 时，把完整输出一次性写入 spill。
    fn ensure_spill_for_final_truncation(&mut self) {
        if self.is_truncated() {
            self.ensure_spill(&self.tail.clone());
        }
    }

    /// 生成最终的展示文本与截断说明信息。
    fn final_progress(&self) -> BashProgress {
        let tail_result = truncate_tail(&self.tail);
        let total_lines = self.total_lines();
        if !self.is_truncated() {
            return BashProgress {
                output_text: self.tail.clone(),
                note: None,
            };
        }
        let truncated_by = if tail_result.truncated {
            tail_result.truncated_by.unwrap_or(TruncatedBy::Lines)
        } else if self.total_bytes > DEFAULT_MAX_BYTES {
            TruncatedBy::Bytes
        } else {
            TruncatedBy::Lines
        };
        let start_line = total_lines.saturating_sub(tail_result.output_lines) + 1;
        let end_line = total_lines;
        let note = if tail_result.last_line_partial {
            format!(
                "[Showing last {} of line {end_line} (line is {}).]",
                format_size(tail_result.content.len()),
                format_size(self.current_line_bytes)
            )
        } else if truncated_by == TruncatedBy::Lines {
            format!("[Showing lines {start_line}-{end_line} of {total_lines}.]")
        } else {
            format!(
                "[Showing lines {start_line}-{end_line} of {total_lines} ({} limit).]",
                format_size(DEFAULT_MAX_BYTES),
            )
        };
        BashProgress {
            output_text: tail_result.content,
            note: Some(note),
        }
    }
}

struct BashProgress {
    output_text: String,
    note: Option<String>,
}

/// 保留字符串最后 `max_bytes` 字节（截到 UTF-8 字符边界，原地操作）。
fn trim_to_last_bytes(text: &mut String, max_bytes: usize) {
    if text.len() <= max_bytes {
        return;
    }
    let mut start = text.len() - max_bytes;
    while start < text.len() && (text.as_bytes()[start] & 0b1100_0000) == 0b1000_0000 {
        start += 1;
    }
    text.drain(..start);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::registry::ToolRegistry;
    use crate::tools::test_support::context;
    use serde_json::json;
    use tempfile::tempdir;

    fn run(command: &str, timeout_ms: Option<u64>) -> ToolExecution {
        let dir = tempdir().expect("temp dir");
        let mut args = json!({ "command": command });
        if let Some(ms) = timeout_ms {
            args["timeout_ms"] = json!(ms);
        }
        ToolRegistry::new()
            .execute("bash", context(args, dir.path()))
            .expect("execute")
    }

    #[test]
    fn utf8_decoder_reassembles_every_split_boundary() {
        let bytes = "前界\r\n后".as_bytes();
        for split in 0..=bytes.len() {
            let mut decoder = Utf8Decoder::default();
            let mut output = decoder.decode(&bytes[..split], false);
            output.push_str(&decoder.decode(&bytes[split..], true));
            assert_eq!(output, "前界\n后", "split={split}");
        }
    }

    #[test]
    fn utf8_decoder_replaces_only_incomplete_tail_at_eof() {
        let mut decoder = Utf8Decoder::default();
        assert_eq!(decoder.decode(&[0xe5, 0xa4], false), "");
        assert_eq!(decoder.decode(&[], true), "\u{FFFD}");
        let mut invalid = Utf8Decoder::default();
        assert_eq!(invalid.decode(&[0xff, b'a'], true), "\u{FFFD}a");
    }

    #[test]
    fn stdout_and_stderr_decoders_keep_independent_carry_state() {
        let mut stdout = Utf8Decoder::default();
        let mut stderr = Utf8Decoder::default();
        let stdout_first = "输".as_bytes();
        let stderr_first = "错".as_bytes();
        assert_eq!(stdout.decode(&stdout_first[..1], false), "");
        assert_eq!(stderr.decode(&stderr_first[..1], false), "");
        assert_eq!(stdout.decode(&stdout_first[1..], true), "输");
        assert_eq!(stderr.decode(&stderr_first[1..], true), "错");
    }

    #[test]
    fn small_output_is_returned_in_full() {
        let result = run("echo hello", None);
        assert!(!result.is_error, "unexpected error: {}", result.content);
        assert!(
            result.content.contains("hello"),
            "content: {}",
            result.content
        );
        assert!(
            !result.content.contains("[Showing"),
            "small output must not be truncated: {}",
            result.content
        );
    }

    #[test]
    fn timeout_ms_lower_bound_is_accepted() {
        let result = run("sleep 0.01", Some(1));
        assert!(result.is_error, "1ms should time out");
        assert!(
            result.content.contains("timed out"),
            "content: {}",
            result.content
        );
    }

    #[test]
    fn timeout_ms_large_values_are_accepted() {
        let result = run("echo ok", Some(600_000_000));
        assert!(!result.is_error, "content: {}", result.content);
        assert!(result.content.contains("ok"));
    }

    #[test]
    fn omitted_timeout_runs_to_completion() {
        let started = Instant::now();
        let result = run("sleep 2; echo late", None);
        assert!(!result.is_error, "content: {}", result.content);
        assert!(
            result.content.contains("late"),
            "content: {}",
            result.content
        );
        assert!(
            started.elapsed() >= Duration::from_secs(2),
            "command must not be killed by an implicit timeout"
        );
    }

    #[test]
    fn timeout_ms_wrong_types_are_typed_errors() {
        let dir = tempdir().expect("temp dir");
        for bad in [
            json!("120000"),
            json!(1.5),
            json!(-1),
            json!(null),
            json!(true),
            json!({"ms": 1}),
        ] {
            let args = json!({"command": "echo should-not-run", "timeout_ms": bad});
            let result = ToolRegistry::new()
                .execute("bash", context(args, dir.path()))
                .expect("execute");
            assert!(result.is_error, "bad timeout {bad:?} was accepted");
            assert!(
                result.content.contains("timeout_ms")
                    && (result.content.contains("invalid timeout_ms")
                        || result.content.contains("must be of type integer")
                        || result.content.contains("must be >= 1")),
                "content: {}",
                result.content
            );
        }
    }

    #[test]
    fn timeout_ms_zero_is_rejected_before_spawn() {
        let result = run("echo should-not-run", Some(0));
        assert!(result.is_error, "content: {}", result.content);
        assert!(result.content.contains("invalid timeout_ms"));
    }

    #[test]
    fn non_zero_exit_code_is_error() {
        let result = run("exit 3", None);
        assert!(result.is_error, "content: {}", result.content);
        assert!(
            result.content.contains("Command exited with code 3"),
            "content: {}",
            result.content
        );
    }

    #[test]
    fn large_output_truncates_to_tail_and_spills_full_output() {
        let dir = tempdir().expect("temp dir");
        let content = (1..=2500)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let file = dir.path().join("large.txt");
        std::fs::write(&file, content).expect("write fixture");
        let result = run(&format!("cat \"{}\"", file.display()), None);
        assert!(!result.is_error, "unexpected error: {}", result.content);
        assert!(result.content.contains("line 2500"), "tail must be kept");
        assert!(
            !result.content.lines().any(|line| line == "line 1"),
            "head must be dropped"
        );
        assert!(
            result.content.starts_with("line 501"),
            "tail starts at first kept line"
        );
        assert!(result.content.contains("[Showing lines"), "missing note");
        // 截断发生时必须给出完整输出文件路径，且文件内容包含被截掉的头部。
        let full_output_line = result
            .content
            .lines()
            .find(|line| line.starts_with("Full output: "))
            .expect("truncated output must carry a Full output path");
        let spill_path = Path::new(full_output_line.trim_start_matches("Full output: "));
        let spilled = std::fs::read_to_string(spill_path)
            .unwrap_or_else(|error| panic!("read spill file {}: {error}", spill_path.display()));
        for line in ["line 1", "line 2", "line 1250", "line 2500"] {
            assert!(
                spilled.lines().any(|candidate| candidate == line),
                "spill file must contain {line}"
            );
        }
    }

    #[test]
    fn small_output_never_spills() {
        let result = run("echo hello", None);
        assert!(!result.is_error, "content: {}", result.content);
        assert!(
            !result.content.contains("Full output:"),
            "untruncated output must not reference a spill file, content: {}",
            result.content
        );
    }

    #[test]
    fn timeout_terminates_and_marks_error() {
        let result = run("sleep 10", Some(300));
        assert!(result.is_error, "content: {}", result.content);
        assert!(
            result.content.contains("timed out after 300 ms"),
            "content: {}",
            result.content
        );
    }

    #[test]
    fn cancellation_terminates_shell_tree_promptly() {
        let dir = tempdir().expect("temp dir");
        let token = singularity_core::CancellationToken::new();
        let worker_token = token.clone();
        let cwd = dir.path().to_path_buf();
        // Keep a descendant alive so the platform tree-containment path,
        // rather than only terminating the shell, is exercised.
        let command = "sleep 30 & wait".to_string();
        let started = Instant::now();
        let worker = thread::spawn(move || {
            ToolRegistry::new().execute(
                "bash",
                ExecuteContext {
                    args: json!({"command": command}),
                    cwd: &cwd,
                    signal: Some(&worker_token),
                    on_update: None,
                },
            )
        });
        thread::sleep(Duration::from_millis(150));
        token.cancel();
        let result = worker.join().expect("bash worker").expect("execute");
        assert!(result.is_error, "cancelled command must be an error");
        assert!(
            result.content.contains("Command aborted"),
            "content: {}",
            result.content
        );
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(5),
            "cancellation must terminate promptly, took {elapsed:?}"
        );
    }

    /// D-004 树杀专项：取消后，仍持活的后代进程必须在有界时间内停止产出。
    ///
    /// 后台循环每秒向文件追加一个 tick；主 shell 用 `wait` 挂住以保持整树存活。
    /// 取消返回后观察若干采样点：若树杀失效，tick 会持续增长；树杀生效则
    /// 相邻采样必然相等且只允许极少量 in-flight 余量。
    #[test]
    fn cancellation_stops_descendant_output_growth() {
        let dir = tempdir().expect("temp dir");
        let tick_file = dir.path().join("ticks.txt");
        let token = singularity_core::CancellationToken::new();
        let worker_token = token.clone();
        let cwd = dir.path().to_path_buf();
        let command = format!(
            "for i in $(seq 1 30); do echo t >> \"{}\"; sleep 1; done & wait",
            tick_file.display()
        );
        let worker = thread::spawn(move || {
            ToolRegistry::new().execute(
                "bash",
                ExecuteContext {
                    args: json!({"command": command}),
                    cwd: &cwd,
                    signal: Some(&worker_token),
                    on_update: None,
                },
            )
        });
        // 至少等一个 tick 落盘，确认后代循环已在运行。
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let ticks = tick_count(&tick_file);
            if ticks > 0 || Instant::now() >= deadline {
                assert!(ticks > 0, "descendant loop must start producing ticks");
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
        let before = tick_count(&tick_file);
        thread::sleep(Duration::from_millis(150));
        token.cancel();
        let result = worker.join().expect("bash worker").expect("execute");
        assert!(
            result.content.contains("Command aborted"),
            "content: {}",
            result.content
        );
        // 取消后的三个采样点：相邻相等即证明产出已停止。
        let sample_a = sample_after(&tick_file, Duration::from_millis(1200));
        let sample_b = sample_after(&tick_file, Duration::from_millis(800));
        let sample_c = sample_after(&tick_file, Duration::from_millis(800));
        assert!(
            (sample_a == sample_b || sample_b == sample_c) && sample_c <= before + 2,
            "descendant must stop producing after tree kill: before={before} a={sample_a} b={sample_b} c={sample_c}"
        );
    }

    fn tick_count(path: &Path) -> usize {
        std::fs::read_to_string(path)
            .map(|text| text.lines().count())
            .unwrap_or(0)
    }

    fn sample_after(path: &Path, delay: Duration) -> usize {
        thread::sleep(delay);
        tick_count(path)
    }

    #[test]
    fn background_process_holding_pipe_truncates_output_boundedly() {
        // 主 shell 退出后一个孙进程仍持有 stdout 写端：pump 做有界读，宽限后
        // 截断输出并给出标记，而非无限阻塞；后台进程本身不受强杀影响。
        let result = run("echo captured; (sleep 3) &", None);
        assert!(!result.is_error, "content: {}", result.content);
        assert!(
            result.content.contains("captured"),
            "content: {}",
            result.content
        );
        assert!(
            result.content.contains(OUTPUT_TRUNCATED_BACKGROUND_NOTE),
            "truncation note missing, content: {}",
            result.content
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_job_object_lifecycle_terminates_spawned_process() {
        let shell = std::env::var("ComSpec").unwrap_or_else(|_| "cmd.exe".to_string());
        let cwd = std::env::current_dir().expect("current directory");
        let mut managed = spawn_shell(
            &shell,
            &["/C".to_string(), "ping -n 10 127.0.0.1".to_string()],
            &cwd,
        )
        .expect("std Command spawn with job assignment");
        let _ = managed.child.stdout.take();
        let _ = managed.child.stderr.take();
        managed.kill_tree();
        let status = managed
            .wait_bounded(Duration::from_secs(5))
            .expect("wait child");
        assert!(!status.success(), "terminated process must not be success");
    }

    #[cfg(windows)]
    #[test]
    fn missing_bash_reports_configuration_error_instead_of_cmd_fallback() {
        let error = bash_shell_command("echo should-not-run", None).expect_err("missing bash");
        assert!(error.contains("Git for Windows"), "{error}");
        assert!(error.contains("bash.exe"), "{error}");
        assert!(
            error.contains("https://git-scm.com/install/windows"),
            "{error}"
        );
        assert!(!error.to_ascii_lowercase().contains("cmd"), "{error}");
    }
}
