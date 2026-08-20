//! bash 工具：在当前工作目录下执行 Shell 命令行指令。
//!
//! - **超时控制**：默认超时 120 秒（支持参数 `timeout_ms` 覆盖，硬上限 600 秒）；超时后自动强制终止整棵子进程树并返回超时错误。
//! - **输出流式捕获与截断**：标准输出（stdout）与标准错误（stderr）合并捕获；结果输出保留尾部最后 2000 行 / 50KB；
//!   超出部分自动转储至私有临时目录（`~/.singularity/tmp/bash/bash-<uuid>.log`），单个 spill 上限 64 MiB，并在返回结果尾部附带日志路径提示。
//!   创建新 spill 时惰性清理同目录下名称匹配且超过 24 小时的旧 spill 文件；超出 64 MiB 时删除未完成文件并报告 full output 不可用。
//! - **中断处理**：收到外部取消信号（`CancellationToken`）时立即终止进程树并返回 `Command aborted`。
//! - **进程隔离与管道保护**：Windows 启动时只把 stdout/stderr 写端列入显式继承白名单，
//!   避免无关句柄或残留子进程直写宿主流。

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::Path;
use std::path::PathBuf;
use std::process::ExitStatus;
#[cfg(unix)]
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

#[cfg(windows)]
#[allow(unsafe_code)] // Windows 平台底层进程、管道与作业对象管理集中在此模块。
mod windows_process {
    use std::ffi::c_void;
    use std::fs::File;
    use std::io;
    use std::mem::size_of;
    use std::os::windows::io::{FromRawHandle, RawHandle};
    use std::os::windows::process::ExitStatusExt;
    use std::path::Path;
    use std::ptr::{null, null_mut};
    use std::time::Duration;

    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, HANDLE, HANDLE_FLAG_INHERIT, SetHandleInformation,
        WAIT_OBJECT_0, WAIT_TIMEOUT,
    };
    use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
    use windows_sys::Win32::System::JobObjects::{
        CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JobObjectExtendedLimitInformation, SetInformationJobObject, TerminateJobObject,
    };
    use windows_sys::Win32::System::Pipes::CreatePipe;
    use windows_sys::Win32::System::Threading::{
        CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT, CreateProcessW,
        DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT, GetExitCodeProcess,
        InitializeProcThreadAttributeList, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
        PROC_THREAD_ATTRIBUTE_JOB_LIST, PROCESS_INFORMATION, STARTF_USESTDHANDLES, STARTUPINFOEXW,
        TerminateProcess, UpdateProcThreadAttribute, WaitForSingleObject,
    };

    fn last_error(_operation: &str) -> io::Error {
        io::Error::from_raw_os_error(unsafe { GetLastError() } as i32)
    }

    fn close_handle(handle: HANDLE) {
        if handle != 0 {
            // Ownership is transferred into this helper only on setup failure or
            // after CreateProcess has copied the inheritable pipe handle.
            unsafe { CloseHandle(handle) };
        }
    }

    /// Windows 作业对象（Job Object）RAII 保护结构体。
    ///
    /// PROC_THREAD_ATTRIBUTE_JOB_LIST 将 Job 绑定在 CreateProcessW 的创建
    /// 边界内；成功返回的进程从出生起就不能逃逸到未管理的进程树。
    pub(super) struct JobObjectGuard {
        handle: HANDLE,
    }

    impl JobObjectGuard {
        fn try_new() -> io::Result<Self> {
            // The returned handle is owned by this guard.  Every failure after
            // creation closes it before returning, so no setup path leaks it.
            let handle = unsafe { CreateJobObjectW(null(), null()) };
            if handle == 0 {
                return Err(last_error("CreateJobObjectW"));
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
                close_handle(handle);
                return Err(last_error("SetInformationJobObject"));
            }
            Ok(Self { handle })
        }

        pub(super) fn terminate(&self, exit_code: u32) -> bool {
            unsafe { TerminateJobObject(self.handle, exit_code) != 0 }
        }
    }

    impl Drop for JobObjectGuard {
        fn drop(&mut self) {
            // Closing a KILL_ON_JOB_CLOSE job handle terminates any still-running
            // descendants; this is the final ownership boundary for the tree.
            close_handle(self.handle);
        }
    }

    pub(super) struct ChildProcess {
        job: JobObjectGuard,
        process: HANDLE,
        stdout: Option<File>,
        stderr: Option<File>,
    }

    impl ChildProcess {
        pub(super) fn take_stdout(&mut self) -> Option<File> {
            self.stdout.take()
        }

        pub(super) fn take_stderr(&mut self) -> Option<File> {
            self.stderr.take()
        }

        pub(super) fn try_wait(&self) -> io::Result<Option<std::process::ExitStatus>> {
            let result = unsafe { WaitForSingleObject(self.process, 0) };
            if result == WAIT_TIMEOUT {
                return Ok(None);
            }
            if result != WAIT_OBJECT_0 {
                return Err(last_error("WaitForSingleObject"));
            }
            Ok(Some(self.exit_status()?))
        }

        pub(super) fn wait_bounded(&self, timeout: Duration) -> Option<std::process::ExitStatus> {
            let timeout_ms = timeout.as_millis().min(u32::MAX as u128) as u32;
            let result = unsafe { WaitForSingleObject(self.process, timeout_ms) };
            (result == WAIT_OBJECT_0)
                .then(|| self.exit_status().ok())
                .flatten()
        }

        fn exit_status(&self) -> io::Result<std::process::ExitStatus> {
            let mut code = 0u32;
            if unsafe { GetExitCodeProcess(self.process, &mut code) } == 0 {
                return Err(last_error("GetExitCodeProcess"));
            }
            Ok(std::process::ExitStatus::from_raw(code))
        }

        pub(super) fn terminate_tree(&self, exit_code: u32) {
            let _ = self.job.terminate(exit_code);
            // TerminateProcess is only a fallback for a job API failure; the Job
            // remains the authoritative tree-wide termination mechanism.
            unsafe { TerminateProcess(self.process, exit_code) };
        }
    }

    impl Drop for ChildProcess {
        fn drop(&mut self) {
            close_handle(self.process);
        }
    }

    fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// Quote one Windows command-line argument for the CRT parser used by bash.
    fn quote_arg(value: &str) -> String {
        if !value.is_empty()
            && !value
                .chars()
                .any(|character| matches!(character, ' ' | '\t' | '"'))
        {
            return value.to_string();
        }
        let mut quoted = String::with_capacity(value.len().saturating_add(2));
        quoted.push('"');
        let mut backslashes = 0usize;
        for character in value.chars() {
            if character == '\\' {
                backslashes = backslashes.saturating_add(1);
                continue;
            }
            if character == '"' {
                quoted.extend(std::iter::repeat_n(
                    '\\',
                    backslashes.saturating_mul(2).saturating_add(1),
                ));
                quoted.push('"');
            } else {
                quoted.extend(std::iter::repeat_n('\\', backslashes));
                quoted.push(character);
            }
            backslashes = 0;
        }
        quoted.extend(std::iter::repeat_n('\\', backslashes.saturating_mul(2)));
        quoted.push('"');
        quoted
    }

    pub(super) fn spawn(
        shell: &str,
        shell_args: &[String],
        cwd: &Path,
    ) -> io::Result<ChildProcess> {
        let job = JobObjectGuard::try_new()?;
        let security = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: null_mut(),
            bInheritHandle: 1,
        };
        let mut stdout_read: HANDLE = 0;
        let mut stdout_write: HANDLE = 0;
        let mut stderr_read: HANDLE = 0;
        let mut stderr_write: HANDLE = 0;

        // Pipe handles are explicitly owned until CreateProcess succeeds.  Read
        // ends are made non-inheritable; write ends are the only handles listed
        // in the extended startup attributes.
        if unsafe { CreatePipe(&mut stdout_read, &mut stdout_write, &security, 0) } == 0 {
            return Err(last_error("CreatePipe(stdout)"));
        }
        if unsafe { SetHandleInformation(stdout_read, HANDLE_FLAG_INHERIT, 0) } == 0 {
            close_handle(stdout_read);
            close_handle(stdout_write);
            return Err(last_error("SetHandleInformation(stdout)"));
        }
        if unsafe { CreatePipe(&mut stderr_read, &mut stderr_write, &security, 0) } == 0 {
            close_handle(stdout_read);
            close_handle(stdout_write);
            return Err(last_error("CreatePipe(stderr)"));
        }
        if unsafe { SetHandleInformation(stderr_read, HANDLE_FLAG_INHERIT, 0) } == 0 {
            close_handle(stdout_read);
            close_handle(stdout_write);
            close_handle(stderr_read);
            close_handle(stderr_write);
            return Err(last_error("SetHandleInformation(stderr)"));
        }

        let mut attribute_size = 0usize;
        // The first call intentionally probes the required allocation size.
        let _ = unsafe { InitializeProcThreadAttributeList(null_mut(), 2, 0, &mut attribute_size) };
        if attribute_size == 0 {
            close_handle(stdout_read);
            close_handle(stdout_write);
            close_handle(stderr_read);
            close_handle(stderr_write);
            return Err(last_error("InitializeProcThreadAttributeList(size)"));
        }
        let units = attribute_size.saturating_add(size_of::<usize>().saturating_sub(1))
            / size_of::<usize>();
        let mut attribute_storage = vec![0usize; units];
        let attribute_list = attribute_storage.as_mut_ptr().cast();
        if unsafe { InitializeProcThreadAttributeList(attribute_list, 2, 0, &mut attribute_size) }
            == 0
        {
            close_handle(stdout_read);
            close_handle(stdout_write);
            close_handle(stderr_read);
            close_handle(stderr_write);
            return Err(last_error("InitializeProcThreadAttributeList"));
        }
        let job_handle = job.handle;
        let handles = [stdout_write, stderr_write];
        let update = unsafe {
            UpdateProcThreadAttribute(
                attribute_list,
                0,
                PROC_THREAD_ATTRIBUTE_JOB_LIST as usize,
                &job_handle as *const HANDLE as *const c_void,
                size_of::<HANDLE>(),
                null_mut(),
                null(),
            ) != 0
                && UpdateProcThreadAttribute(
                    attribute_list,
                    0,
                    PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                    handles.as_ptr().cast(),
                    size_of_val(&handles),
                    null_mut(),
                    null(),
                ) != 0
        };
        if !update {
            unsafe { DeleteProcThreadAttributeList(attribute_list) };
            close_handle(stdout_read);
            close_handle(stdout_write);
            close_handle(stderr_read);
            close_handle(stderr_write);
            return Err(last_error("UpdateProcThreadAttribute"));
        }

        let mut startup: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
        startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
        startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        startup.StartupInfo.hStdOutput = stdout_write;
        startup.StartupInfo.hStdError = stderr_write;
        let mut process_info: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
        startup.lpAttributeList = attribute_list;
        let shell_wide = wide(shell);
        let mut command_line = wide(
            &std::iter::once(shell)
                .chain(shell_args.iter().map(String::as_str))
                .map(quote_arg)
                .collect::<Vec<_>>()
                .join(" "),
        );
        let cwd_wide = wide(&cwd.to_string_lossy());
        let created = unsafe {
            CreateProcessW(
                shell_wide.as_ptr(),
                command_line.as_mut_ptr(),
                null(),
                null(),
                1,
                EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT | CREATE_NO_WINDOW,
                null(),
                cwd_wide.as_ptr(),
                &startup.StartupInfo,
                &mut process_info,
            )
        };
        unsafe { DeleteProcThreadAttributeList(attribute_list) };
        if created == 0 {
            close_handle(stdout_read);
            close_handle(stdout_write);
            close_handle(stderr_read);
            close_handle(stderr_write);
            return Err(last_error("CreateProcessW"));
        }

        // The child owns the inherited write ends.  The parent closes its copies
        // immediately so reader EOF is tied to the real process tree.
        close_handle(stdout_write);
        close_handle(stderr_write);
        close_handle(process_info.hThread);
        let stdout = unsafe { File::from_raw_handle(stdout_read as RawHandle) };
        let stderr = unsafe { File::from_raw_handle(stderr_read as RawHandle) };
        Ok(ChildProcess {
            job,
            process: process_info.hProcess,
            stdout: Some(stdout),
            stderr: Some(stderr),
        })
    }
}

use serde_json::{Value, json};

use super::registry::{ExecuteContext, ToolError, ToolExecution, error_result};
use super::truncate::{
    DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, TruncatedBy, format_size, truncate_tail,
};

/// 内存中保留的尾部缓冲区字节上限（100KB），防止超大单行输出耗尽内存。
const INTERNAL_TAIL_MAX_BYTES: usize = DEFAULT_MAX_BYTES * 2;
/// 输出分块读取管道的容量上限。
const OUTPUT_QUEUE_CAPACITY: usize = 32;

/// 完整输出转储临时文件的前缀与后缀。
const FULL_OUTPUT_FILE_PREFIX: &str = "bash-";
const FULL_OUTPUT_FILE_SUFFIX: &str = ".log";
const FULL_OUTPUT_MAX_BYTES: usize = 64 * 1024 * 1024;
const FULL_OUTPUT_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// 命令执行默认超时时间（120 秒）。
pub(crate) const DEFAULT_TIMEOUT_MS: u64 = 120_000;
/// 命令执行最大允许超时时间（600 秒 / 10 分钟）。
pub(crate) const MAX_TIMEOUT_MS: u64 = 600_000;
pub(crate) const DESCRIPTION: &str = "Execute a bash command in the current working directory. Returns stdout and stderr. Output is truncated to last 2000 lines or 50KB (whichever is hit first). If truncated, full output is saved to a temp file. Commands time out after 120 seconds by default; provide timeout_ms to override.";

pub(crate) fn parameters() -> Value {
    json!({
        "type": "object",
        "properties": {
            "command": { "type": "string", "description": "Bash command to execute" },
            "timeout_ms": {
                "type": "integer",
                "minimum": 1,
                "maximum": 600000,
                "description": "Timeout in milliseconds (optional, defaults to 120000)"
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
        execute,
    }
}

pub(crate) fn execute(ctx: ExecuteContext<'_>) -> Result<ToolExecution, ToolError> {
    let ExecuteContext {
        args,
        cwd,
        signal,
        mut on_update,
        mutation_queue: _,
    } = ctx;
    let Some(command) = args.get("command").and_then(Value::as_str) else {
        return error_result("missing required parameter \"command\"");
    };
    // 只有字段缺失才使用 DEFAULT_TIMEOUT_MS；字段存在但类型错误/越界
    // 必须返回 typed argument error，不能静默回退默认值。
    let timeout = match args.get("timeout_ms") {
        None => DEFAULT_TIMEOUT_MS,
        Some(Value::Number(number)) => match number.as_u64() {
            Some(timeout) => timeout,
            None => {
                return error_result(format!(
                    "invalid timeout_ms: must be an integer between 1 and {MAX_TIMEOUT_MS}"
                ));
            }
        },
        Some(_) => {
            return error_result(format!(
                "invalid timeout_ms: must be an integer between 1 and {MAX_TIMEOUT_MS}"
            ));
        }
    };
    if timeout == 0 || timeout > MAX_TIMEOUT_MS {
        return error_result(format!(
            "invalid timeout_ms: must be between 1 and {MAX_TIMEOUT_MS} milliseconds"
        ));
    }
    let (shell, shell_args) = match shell_command(command) {
        Ok(command) => command,
        Err(error) => return error_result(error),
    };
    #[cfg(windows)]
    let mut child = match windows_process::spawn(&shell, &shell_args, cwd) {
        Ok(child) => child,
        Err(error) => {
            return error_result(format!("failed to spawn shell {shell}: {error}"));
        }
    };
    #[cfg(unix)]
    let mut child = {
        let mut command = Command::new(&shell);
        command
            .args(&shell_args)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        use std::os::unix::process::CommandExt;
        command.process_group(0);
        match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                return error_result(format!("failed to spawn shell {shell}: {error}"));
            }
        }
    };
    #[cfg(windows)]
    let stdout = child.take_stdout().expect("bash stdout is piped");
    #[cfg(windows)]
    let stderr = child.take_stderr().expect("bash stderr is piped");
    #[cfg(unix)]
    let stdout = child.stdout.take().expect("bash stdout is piped");
    #[cfg(unix)]
    let stderr = child.stderr.take().expect("bash stderr is piped");
    let (sender, receiver) = mpsc::sync_channel(OUTPUT_QUEUE_CAPACITY);
    let stderr_sender = sender.clone();
    // 读取线程在 EOF 时自行退出；JoinHandle 直接丢弃（detach），不 join，
    // 避免被残留子进程持有的管道句柄无限阻塞。
    thread::spawn(move || pump_output(stdout, sender));
    thread::spawn(move || pump_output(stderr, stderr_sender));

    let mut state = CaptureState::default();
    let deadline = Some(Instant::now() + Duration::from_millis(timeout));
    let mut exit_status = None;
    let mut outcome = BashOutcome::Completed;
    loop {
        drain(&receiver, &mut state, &mut on_update);
        if let Some(signal) = signal
            && signal.is_cancelled()
        {
            kill_process_tree(&mut child);
            outcome = BashOutcome::Aborted;
            exit_status = wait_for_exit(&mut child);
            break;
        }
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            kill_process_tree(&mut child);
            outcome = BashOutcome::TimedOut(timeout);
            exit_status = wait_for_exit(&mut child);
            break;
        }
        if state.capture_error.is_some() {
            kill_process_tree(&mut child);
            let _ = wait_for_exit(&mut child);
            break;
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                exit_status = Some(status);
                break;
            }
            Ok(None) => {}
            Err(error) => {
                kill_process_tree(&mut child);
                let _ = wait_for_exit(&mut child);
                return error_result(format!("failed to wait for child process: {error}"));
            }
        }
        thread::sleep(Duration::from_millis(5));
    }
    // 子进程已退出，但管道中可能仍有缓冲输出（或子进程树仍持有管道句柄）。
    // 读到所有发送端关闭（EOF）为止，最长宽限 2 秒，避免被残留子进程无限阻塞。
    let grace = Instant::now() + Duration::from_millis(2_000);
    loop {
        let readers_ended = drain(&receiver, &mut state, &mut on_update);
        if readers_ended || Instant::now() >= grace {
            break;
        }
        thread::sleep(Duration::from_millis(5));
    }
    // 理论不可达的兜底：已超限但临时文件尚未创建（例如极端截断路径），补建。
    if state.capture_error.is_none()
        && state.is_truncated()
        && state.full_output_path().is_none()
        && let Err(error) = state.create_full_output_file()
    {
        state.capture_error = Some(error);
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
    if let Some(error) = state.capture_error.as_ref() {
        append_status(
            &mut content,
            &format!("[failed to save full output to temp file: {error}]"),
        );
        is_error = true;
    }
    if let Some(callback) = on_update.as_mut() {
        callback(&state.current_output());
    }
    Ok(ToolExecution { content, is_error })
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

/// 从管道读取字节流，过滤控制字符并按块发送至通道。
fn pump_output(mut reader: impl Read + Send + 'static, sender: mpsc::SyncSender<String>) {
    let mut decoder = Utf8Decoder::default();
    let mut buffer = [0u8; 64 * 1024];
    loop {
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
                        output.push('�');
                        self.pending.drain(..error_len);
                        continue;
                    }
                    if eof {
                        output.push('�');
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

/// 接收并处理所有当前可用 chunk；返回读取线程是否已全部结束（队列耗尽且发送端关闭）。
fn drain<'a>(
    receiver: &Receiver<String>,
    state: &mut CaptureState,
    on_update: &mut Option<&'a mut dyn FnMut(&str)>,
) -> bool {
    let mut ingested = false;
    let mut readers_ended = false;
    loop {
        match receiver.try_recv() {
            Ok(chunk) => {
                ingested = true;
                if let Err(error) = state.ingest(&chunk) {
                    state.capture_error = Some(error);
                    break;
                }
            }
            Err(TryRecvError::Empty) => break,
            Err(TryRecvError::Disconnected) => {
                readers_ended = true;
                break;
            }
        }
    }
    if ingested && let Some(on_update) = on_update.as_mut() {
        on_update(&state.current_output());
    }
    readers_ended
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

#[cfg(windows)]
fn bash_shell_command(
    command: &str,
    bash: Option<String>,
) -> Result<(String, Vec<String>), String> {
    let Some(bash) = bash else {
        return Err(
            "bash executable not found; install Git for Windows or add bash.exe to PATH"
                .to_string(),
        );
    };
    Ok((bash, vec!["-c".to_string(), command.to_string()]))
}

#[cfg(windows)]
fn find_bash_on_windows() -> Option<String> {
    let mut candidates = Vec::new();
    for var in ["ProgramFiles", "ProgramFiles(x86)"] {
        if let Ok(program_files) = std::env::var(var) {
            candidates.push(format!("{program_files}\\Git\\bin\\bash.exe"));
        }
    }
    if let Ok(path) = std::env::var("PATH") {
        for dir in path.split(';') {
            if !dir.is_empty() {
                candidates.push(format!("{dir}\\bash.exe"));
            }
        }
    }
    candidates
        .into_iter()
        .find(|candidate| Path::new(candidate).is_file())
}

/// 终止进程树：
/// - Windows：通过内核级 Job Object 强制连带原子终止所有子孙进程；
/// - Unix：向创建时绑定的独立进程组广播 SIGKILL；
/// - Unix 额外调用 `Child::kill`，确保主进程句柄状态收敛。
#[cfg(windows)]
fn kill_process_tree(child: &mut windows_process::ChildProcess) {
    child.terminate_tree(1);
}

#[cfg(unix)]
fn kill_process_tree(child: &mut Child) {
    let pid = child.id().to_string();
    let _ = Command::new("kill")
        .args(["-KILL", &format!("-{pid}")])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let _ = child.kill();
}

/// 等待子进程被回收（最多 5 秒，超时放弃，避免残留句柄无限阻塞）。
#[cfg(windows)]
fn wait_for_exit(child: &mut windows_process::ChildProcess) -> Option<ExitStatus> {
    child.wait_bounded(Duration::from_secs(5))
}

#[cfg(unix)]
fn wait_for_exit(child: &mut Child) -> Option<ExitStatus> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match child.try_wait() {
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

fn spill_directory() -> io::Result<PathBuf> {
    let base = singularity_core::user_singularity_home()
        .unwrap_or_else(|| std::env::temp_dir().join("singularity"));
    let directory = base.join("tmp").join("bash");
    fs::create_dir_all(&directory)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
    }
    Ok(directory)
}

fn recognized_spill_name(name: &std::ffi::OsStr) -> bool {
    let name = name.to_string_lossy();
    let Some(identity) = name
        .strip_prefix(FULL_OUTPUT_FILE_PREFIX)
        .and_then(|name| name.strip_suffix(FULL_OUTPUT_FILE_SUFFIX))
    else {
        return false;
    };
    uuid::Uuid::parse_str(identity).is_ok()
}

fn cleanup_old_spills(directory: &Path) -> Option<String> {
    cleanup_old_spills_at(directory, SystemTime::now())
}

fn cleanup_old_spills_at(directory: &Path, now: SystemTime) -> Option<String> {
    let mut failures = 0usize;
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) => {
            return Some(format!("could not scan old spill files: {error}"));
        }
    };
    for entry in entries {
        let Ok(entry) = entry else {
            failures = failures.saturating_add(1);
            continue;
        };
        let name = entry.file_name();
        if !recognized_spill_name(&name) {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            failures = failures.saturating_add(1);
            continue;
        };
        let Ok(age) = metadata
            .modified()
            .and_then(|modified| now.duration_since(modified).map_err(io::Error::other))
        else {
            continue;
        };
        if age > FULL_OUTPUT_MAX_AGE && fs::remove_file(entry.path()).is_err() {
            failures = failures.saturating_add(1);
        }
    }
    (failures > 0).then(|| format!("could not remove {failures} old spill file(s)"))
}

/// 累计输出状态：尾部缓冲（上限 2×50KB）、行/字节计数、完整输出临时文件。
#[derive(Default)]
struct CaptureState {
    tail: String,
    total_bytes: usize,
    completed_lines: usize,
    has_open_line: bool,
    current_line_bytes: usize,
    full_buffer: Vec<u8>,
    full_output: Option<(PathBuf, File)>,
    full_output_bytes: usize,
    full_output_written: bool,
    full_output_unavailable: bool,
    spill_cleanup_warning: Option<String>,
    capture_error: Option<io::Error>,
}

impl CaptureState {
    fn total_lines(&self) -> usize {
        self.completed_lines + usize::from(self.has_open_line)
    }

    fn is_truncated(&self) -> bool {
        self.total_lines() > DEFAULT_MAX_LINES || self.total_bytes > DEFAULT_MAX_BYTES
    }

    /// 当前应展示给流式回调的输出（超限时为截断尾部）。
    fn current_output(&self) -> String {
        if self.is_truncated() {
            truncate_tail(&self.tail).content
        } else {
            self.tail.clone()
        }
    }

    fn full_output_path(&self) -> Option<&PathBuf> {
        self.full_output.as_ref().map(|(path, _)| path)
    }

    /// 吸收一个清洗后的 chunk：更新计数与尾部缓冲；一旦超限，创建完整输出临时文件，
    /// 之后每个 chunk 都追加进去。文件内容与展示内容一致（清洗、去 `\r` 后）。
    fn ingest(&mut self, text: &str) -> io::Result<()> {
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
        self.tail.push_str(text);
        if !self.full_output_written && !self.full_output_unavailable {
            self.full_buffer.extend_from_slice(text.as_bytes());
            if self.total_bytes > FULL_OUTPUT_MAX_BYTES {
                self.full_output_unavailable = true;
                self.full_buffer.clear();
                return Err(io::Error::new(
                    io::ErrorKind::QuotaExceeded,
                    format!(
                        "full output unavailable: exceeded {} limit",
                        format_size(FULL_OUTPUT_MAX_BYTES)
                    ),
                ));
            }
            if self.is_truncated() {
                self.create_full_output_file()?;
                self.full_output_written = true;
            }
        } else if self.full_output_written {
            let projected = self.full_output_bytes.saturating_add(text.len());
            if projected > FULL_OUTPUT_MAX_BYTES {
                let path = self.full_output.take().map(|(path, file)| {
                    drop(file);
                    path
                });
                if let Some(path) = path {
                    let _ = fs::remove_file(path);
                }
                self.full_output_unavailable = true;
                return Err(io::Error::new(
                    io::ErrorKind::QuotaExceeded,
                    format!(
                        "full output unavailable: exceeded {} limit",
                        format_size(FULL_OUTPUT_MAX_BYTES)
                    ),
                ));
            }
            if let Some((_, file)) = self.full_output.as_mut() {
                file.write_all(text.as_bytes())?;
            }
            self.full_output_bytes = projected;
        }
        trim_to_last_bytes(&mut self.tail, INTERNAL_TAIL_MAX_BYTES);
        Ok(())
    }

    fn create_full_output_file(&mut self) -> io::Result<()> {
        let directory = spill_directory()?;
        self.create_full_output_file_in(&directory)
    }

    fn create_full_output_file_in(&mut self, directory: &Path) -> io::Result<()> {
        self.spill_cleanup_warning = cleanup_old_spills(directory);
        let path = directory.join(format!(
            "{FULL_OUTPUT_FILE_PREFIX}{}{FULL_OUTPUT_FILE_SUFFIX}",
            uuid::Uuid::now_v7()
        ));
        if self.full_buffer.len() > FULL_OUTPUT_MAX_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::QuotaExceeded,
                format!(
                    "full output unavailable: exceeded {} limit",
                    format_size(FULL_OUTPUT_MAX_BYTES)
                ),
            ));
        }
        let mut file = singularity_core::create_owner_only_file(&path)?;
        if let Err(error) = file.write_all(&self.full_buffer) {
            let _ = fs::remove_file(&path);
            return Err(error);
        }
        self.full_output_bytes = self.full_buffer.len();
        self.full_buffer.clear();
        self.full_output = Some((path, file));
        Ok(())
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
        let path = self
            .full_output_path()
            .map(|path| path.display().to_string())
            .unwrap_or_default();
        let full_output_note = if path.is_empty() {
            format!(
                "Full output unavailable ({} limit)",
                format_size(FULL_OUTPUT_MAX_BYTES)
            )
        } else {
            format!("Full output: {path}")
        };
        let cleanup_warning = self
            .spill_cleanup_warning
            .as_ref()
            .map(|warning| format!("; spill cleanup warning: {warning}"))
            .unwrap_or_default();
        let note = if tail_result.last_line_partial {
            format!(
                "[Showing last {} of line {end_line} (line is {}). {full_output_note}{cleanup_warning}]",
                format_size(tail_result.content.len()),
                format_size(self.current_line_bytes)
            )
        } else if truncated_by == TruncatedBy::Lines {
            format!(
                "[Showing lines {start_line}-{end_line} of {total_lines}. {full_output_note}{cleanup_warning}]"
            )
        } else {
            format!(
                "[Showing lines {start_line}-{end_line} of {total_lines} ({} limit). {full_output_note}{cleanup_warning}]",
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

    fn dump_command(shell: &str, path: &Path) -> String {
        if shell.to_lowercase().contains("cmd") {
            format!("type \"{}\"", path.display())
        } else {
            format!("cat \"{}\"", path.display())
        }
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
        assert_eq!(decoder.decode(&[], true), "�");
        let mut invalid = Utf8Decoder::default();
        assert_eq!(invalid.decode(&[0xff, b'a'], true), "�a");
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
    fn spill_identity_rejects_unrelated_files() {
        assert!(recognized_spill_name(std::ffi::OsStr::new(
            "bash-018f4f4c-3f5f-7f1f-9f2a-9db4e2a7bf42.log"
        )));
        assert!(!recognized_spill_name(std::ffi::OsStr::new(
            "bash-not-a-uuid.log"
        )));
        assert!(!recognized_spill_name(std::ffi::OsStr::new(
            "other-018f4f4c-3f5f-7f1f-9f2a-9db4e2a7bf42.log"
        )));
    }

    #[test]
    fn spill_cleanup_lifecycle_and_diagnostics() {
        let dir = tempdir().expect("spill dir");
        let valid_old_uuid = "bash-018f4f4c-3f5f-7f1f-9f2a-9db4e2a7bf41.log";
        let valid_new_uuid = "bash-018f4f4c-3f5f-7f1f-9f2a-9db4e2a7bf42.log";
        let non_spill = "other-file.txt";

        let old_path = dir.path().join(valid_old_uuid);
        let new_path = dir.path().join(valid_new_uuid);
        let non_spill_path = dir.path().join(non_spill);

        fs::write(&old_path, "old output").expect("write old");
        fs::write(&new_path, "new output").expect("write new");
        fs::write(&non_spill_path, "unrelated").expect("write unrelated");

        let file_time = fs::metadata(&old_path)
            .expect("meta")
            .modified()
            .expect("mod");

        // 1. Current time = 1 hour after creation: all files retained
        let now_1h = file_time + Duration::from_secs(3600);
        let warning = cleanup_old_spills_at(dir.path(), now_1h);
        assert!(warning.is_none());
        assert!(old_path.exists());
        assert!(new_path.exists());
        assert!(non_spill_path.exists());

        // 2. Current time = 25 hours after creation: valid spill older than 24h is deleted, unrelated file is preserved
        let now_25h = file_time + Duration::from_secs(25 * 3600);
        let warning = cleanup_old_spills_at(dir.path(), now_25h);
        assert!(warning.is_none());
        assert!(!old_path.exists(), "expired spill must be deleted");
        assert!(!new_path.exists(), "expired spill must be deleted");
        assert!(non_spill_path.exists(), "unrelated file must be preserved");

        // 3. Scan non-existent directory returns scan diagnostic
        let bad_dir = dir.path().join("does_not_exist");
        let scan_error = cleanup_old_spills_at(&bad_dir, now_25h);
        assert!(scan_error.is_some_and(|w| w.contains("could not scan old spill files")));
    }

    #[test]
    fn spill_overflow_removes_partial_file_and_reports_unavailable() {
        let directory = tempdir().expect("spill directory");
        let mut state = CaptureState {
            full_buffer: vec![b'x'; FULL_OUTPUT_MAX_BYTES],
            ..CaptureState::default()
        };
        state
            .create_full_output_file_in(directory.path())
            .expect("create exact-cap spill");
        state.full_output_written = true;
        let path = state.full_output_path().expect("spill path").clone();
        assert_eq!(
            fs::metadata(&path).expect("spill metadata").len(),
            FULL_OUTPUT_MAX_BYTES as u64
        );
        let error = state.ingest("x").expect_err("N+1 spill must fail closed");
        assert!(error.to_string().contains("full output unavailable"));
        assert!(state.full_output_path().is_none());
        assert!(!path.exists(), "partial spill must be deleted");
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
    fn timeout_ms_upper_bound_is_accepted() {
        let result = run("echo ok", Some(MAX_TIMEOUT_MS));
        assert!(!result.is_error, "content: {}", result.content);
        assert!(result.content.contains("ok"));
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
    fn timeout_ms_above_upper_bound_is_rejected_before_spawn() {
        let result = run("echo should-not-run", Some(MAX_TIMEOUT_MS + 1));
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
    fn large_output_keeps_tail_and_saves_full_output() {
        let dir = tempdir().expect("temp dir");
        let content = (1..=2500)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let file = dir.path().join("large.txt");
        std::fs::write(&file, content).expect("write fixture");
        let (shell, _) = shell_command("").expect("shell");
        let command = dump_command(&shell, &file);
        let result = run(&command, None);
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
        assert!(result.content.contains("Full output:"), "missing path");
        let start = result.content.find("Full output: ").expect("marker") + "Full output: ".len();
        let end = result.content[start..].find(']').expect("marker end") + start;
        let full_path = &result.content[start..end];
        let full = std::fs::read_to_string(full_path).expect("full output file readable");
        assert!(full.contains("line 1"), "full output keeps the head");
        assert!(full.contains("line 2500"), "full output keeps the tail");
        assert_eq!(full.lines().count(), 2500, "full output has all lines");
    }

    #[test]
    fn timeout_terminates_and_marks_error() {
        let (shell, _) = shell_command("").expect("shell");
        let command = if shell.to_lowercase().contains("cmd") {
            "ping -n 10 127.0.0.1"
        } else {
            "sleep 10"
        };
        let result = run(command, Some(300));
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
                    mutation_queue: None,
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

    #[cfg(windows)]
    #[test]
    fn windows_create_time_job_object_lifecycle() {
        let shell = std::env::var("ComSpec").unwrap_or_else(|_| "cmd.exe".to_string());
        let cwd = std::env::current_dir().expect("current directory");
        let mut child = windows_process::spawn(
            &shell,
            &["/C".to_string(), "ping -n 10 127.0.0.1".to_string()],
            &cwd,
        )
        .expect("CreateProcessW with PROC_THREAD_ATTRIBUTE_JOB_LIST");
        let _ = child.take_stdout();
        let _ = child.take_stderr();
        child.terminate_tree(1);
        let status = child
            .wait_bounded(Duration::from_secs(5))
            .expect("wait child");
        assert!(!status.success(), "terminated process must not be success");
    }

    #[cfg(windows)]
    #[test]
    fn missing_bash_reports_configuration_error_instead_of_cmd_fallback() {
        let error = bash_shell_command("echo should-not-run", None).expect_err("missing bash");
        assert!(error.contains("bash executable not found"), "{error}");
        assert!(!error.to_ascii_lowercase().contains("cmd"), "{error}");
    }
}
