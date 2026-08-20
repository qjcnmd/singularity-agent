//! bash 工具：在当前工作目录下执行 Shell 命令行指令。
//!
//! - **超时控制**：默认超时 120 秒（支持参数 `timeout_ms` 覆盖，硬上限 600 秒）；超时后自动强制终止整棵子进程树并返回超时错误。
//! - **输出流式捕获与截断**：标准输出（stdout）与标准错误（stderr）合并捕获；结果输出保留尾部最后 2000 行 / 50KB；
//!   超出部分自动完整转储至系统临时文件（`bash-<uuid>.log`），并在返回结果尾部附带日志路径提示。
//! - **中断处理**：收到外部取消信号（`CancellationToken`）时立即终止进程树并返回 `Command aborted`。
//! - **进程隔离与管道保护**：在 Windows 上启动子进程前清除句柄继承标志，避免残留子进程直写 stdout 管道破坏 JSON-RPC 流。

use std::fs::File;
use std::io::{self, Read, Write};
use std::path::Path;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(windows)]
#[allow(unsafe_code, clippy::upper_case_acronyms)] // Windows 平台底层进程与作业对象管理。
mod windows_process {
    use std::ffi::c_void;
    use std::os::windows::io::{AsRawHandle, RawHandle};
    use std::ptr;

    type HANDLE = *mut c_void;
    type BOOL = i32;
    type DWORD = u32;

    #[repr(C)]
    struct IO_COUNTERS {
        read_operation_count: u64,
        write_operation_count: u64,
        other_operation_count: u64,
        read_transfer_count: u64,
        write_transfer_count: u64,
        other_transfer_count: u64,
    }

    #[repr(C)]
    struct JOBOBJECT_BASIC_LIMIT_INFORMATION {
        per_process_user_time_limit: i64,
        per_job_user_time_limit: i64,
        limit_flags: DWORD,
        minimum_working_set_size: usize,
        maximum_working_set_size: usize,
        active_process_limit: DWORD,
        affinity: usize,
        priority_class: DWORD,
        scheduling_class: DWORD,
    }

    #[repr(C)]
    struct JOBOBJECT_EXTENDED_LIMIT_INFORMATION {
        basic_limit_information: JOBOBJECT_BASIC_LIMIT_INFORMATION,
        io_info: IO_COUNTERS,
        process_memory_limit: usize,
        job_memory_limit: usize,
        peak_process_memory_limit: usize,
        peak_job_memory_limit: usize,
    }

    const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: DWORD = 0x0000_2000;
    const JOB_OBJECT_EXTENDED_LIMIT_INFORMATION: i32 = 9;
    const HANDLE_FLAG_INHERIT: DWORD = 0x0000_0001;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn SetHandleInformation(h_object: RawHandle, dw_mask: DWORD, dw_flags: DWORD) -> BOOL;
        fn CreateJobObjectW(lp_job_attributes: *const c_void, lp_name: *const u16) -> HANDLE;
        fn SetInformationJobObject(
            h_job: HANDLE,
            job_object_information_class: i32,
            lp_job_object_information: *const c_void,
            cb_job_object_information_length: DWORD,
        ) -> BOOL;
        fn AssignProcessToJobObject(h_job: HANDLE, h_process: RawHandle) -> BOOL;
        fn TerminateJobObject(h_job: HANDLE, u_exit_code: u32) -> BOOL;
        fn CloseHandle(h_object: HANDLE) -> BOOL;
    }

    /// 清除 stdout/stderr 句柄的继承位，防止 spawn 时子进程（及孙进程）继承并
    /// 直写本进程的 stdout/stderr 管道。尽力而为：句柄无效时忽略。
    pub(super) fn deny_inherit_std_streams() {
        for handle in [
            std::io::stdout().as_raw_handle(),
            std::io::stderr().as_raw_handle(),
        ] {
            unsafe {
                SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0);
            }
        }
    }

    /// Windows 作业对象（Job Object）RAII 保护结构体。
    ///
    /// 绑定至此 Job Object 的子进程及其派生的所有孙进程，在 Job 关闭或主动终止时，
    /// 将由 Windows NT 内核强制、原子地全部终止，防止孤儿孙进程逃逸或文件句柄泄漏。
    pub(super) struct JobObjectGuard {
        handle: HANDLE,
    }

    impl JobObjectGuard {
        /// 创建并配置一个新的私有 Job Object，启用 `KILL_ON_JOB_CLOSE` 限制。
        pub(super) fn new() -> Option<Self> {
            unsafe {
                let handle = CreateJobObjectW(ptr::null(), ptr::null());
                if handle.is_null() {
                    return None;
                }
                let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                info.basic_limit_information.limit_flags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                let res = SetInformationJobObject(
                    handle,
                    JOB_OBJECT_EXTENDED_LIMIT_INFORMATION,
                    &info as *const _ as *const c_void,
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as DWORD,
                );
                if res == 0 {
                    CloseHandle(handle);
                    return None;
                }
                Some(Self { handle })
            }
        }

        /// 将子进程句柄关联加入到此 Job Object 中。
        pub(super) fn assign_process(&self, process_handle: RawHandle) -> bool {
            unsafe { AssignProcessToJobObject(self.handle, process_handle) != 0 }
        }

        /// 主动强制终止 Job Object 内的所有进程。
        pub(super) fn terminate(&self, exit_code: u32) -> bool {
            unsafe { TerminateJobObject(self.handle, exit_code) != 0 }
        }
    }

    impl Drop for JobObjectGuard {
        fn drop(&mut self) {
            unsafe {
                // 关闭句柄；若启用了 KILL_ON_JOB_CLOSE，操作系统内核会自动清理该 Job 内的所有进程。
                CloseHandle(self.handle);
            }
        }
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
    windows_process::deny_inherit_std_streams();
    #[cfg(windows)]
    let job_guard = windows_process::JobObjectGuard::new();

    let mut command = Command::new(&shell);
    command
        .args(&shell_args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            return error_result(format!("failed to spawn shell {shell}: {error}"));
        }
    };
    #[cfg(windows)]
    if let Some(guard) = &job_guard {
        use std::os::windows::io::AsRawHandle;
        guard.assign_process(child.as_raw_handle());
    }

    let stdout = child.stdout.take().expect("bash stdout is piped");
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
            kill_process_tree(
                &mut child,
                #[cfg(windows)]
                job_guard.as_ref(),
            );
            outcome = BashOutcome::Aborted;
            exit_status = wait_for_exit(&mut child);
            break;
        }
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            kill_process_tree(
                &mut child,
                #[cfg(windows)]
                job_guard.as_ref(),
            );
            outcome = BashOutcome::TimedOut(timeout);
            exit_status = wait_for_exit(&mut child);
            break;
        }
        if state.capture_error.is_some() {
            kill_process_tree(
                &mut child,
                #[cfg(windows)]
                job_guard.as_ref(),
            );
            let _ = child.wait();
            break;
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                exit_status = Some(status);
                break;
            }
            Ok(None) => {}
            Err(error) => {
                kill_process_tree(
                    &mut child,
                    #[cfg(windows)]
                    job_guard.as_ref(),
                );
                let _ = child.wait();
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
    let mut buffer = [0u8; 64 * 1024];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                let text = sanitize_binary_output(&buffer[..read]);
                let text = text.replace('\r', "");
                if !text.is_empty() && sender.send(text).is_err() {
                    break;
                }
            }
            Err(_) => break,
        }
    }
}

/// 过滤不可见的控制字符（保留 `\t`、`\n`、`\r`），其余字节按 UTF-8 进行安全解码。
fn sanitize_binary_output(bytes: &[u8]) -> String {
    let mut cleaned = Vec::with_capacity(bytes.len());
    for &byte in bytes {
        if matches!(byte, b'\t' | b'\n' | b'\r') || byte > 0x1f {
            cleaned.push(byte);
        }
    }
    String::from_utf8_lossy(&cleaned).into_owned()
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
/// - 最后统一调用 `Child::kill` 确保主进程句柄状态收敛。
fn kill_process_tree(
    child: &mut Child,
    #[cfg(windows)] job: Option<&windows_process::JobObjectGuard>,
) {
    #[cfg(windows)]
    {
        if let Some(job) = job {
            job.terminate(1);
        }
    }
    #[cfg(unix)]
    {
        let pid = child.id().to_string();
        let _ = Command::new("kill")
            .args(["-KILL", &format!("-{pid}")])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let _ = child.kill();
}

/// 等待子进程被回收（最多 5 秒，超时放弃，避免残留句柄无限阻塞）。
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

/// 累计输出状态：尾部缓冲（上限 2×50KB）、行/字节计数、完整输出临时文件。
#[derive(Default)]
struct CaptureState {
    tail: String,
    total_bytes: usize,
    completed_lines: usize,
    has_open_line: bool,
    current_line_bytes: usize,
    full_output: Option<(PathBuf, File)>,
    full_output_written: bool,
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
        if !self.full_output_written {
            if self.total_bytes > DEFAULT_MAX_BYTES || self.total_lines() > DEFAULT_MAX_LINES {
                self.create_full_output_file()?;
                self.full_output_written = true;
            }
        } else if let Some((_, file)) = self.full_output.as_mut() {
            file.write_all(text.as_bytes())?;
        }
        trim_to_last_bytes(&mut self.tail, INTERNAL_TAIL_MAX_BYTES);
        Ok(())
    }

    fn create_full_output_file(&mut self) -> io::Result<()> {
        let path = std::env::temp_dir().join(format!(
            "{FULL_OUTPUT_FILE_PREFIX}{}{FULL_OUTPUT_FILE_SUFFIX}",
            uuid::Uuid::now_v7()
        ));
        let mut file = File::create(&path)?;
        file.write_all(self.tail.as_bytes())?;
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
        let note = if tail_result.last_line_partial {
            format!(
                "[Showing last {} of line {end_line} (line is {}). Full output: {path}]",
                format_size(tail_result.content.len()),
                format_size(self.current_line_bytes)
            )
        } else if truncated_by == TruncatedBy::Lines {
            format!("[Showing lines {start_line}-{end_line} of {total_lines}. Full output: {path}]")
        } else {
            format!(
                "[Showing lines {start_line}-{end_line} of {total_lines} ({} limit). Full output: {path}]",
                format_size(DEFAULT_MAX_BYTES)
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
        let command = if cfg!(windows) {
            "ping -n 30 127.0.0.1".to_string()
        } else {
            // Keep a descendant alive so the Unix process-group path, rather
            // than only Child::kill, is exercised.
            "sleep 30 & wait".to_string()
        };
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
    fn windows_job_object_guard_lifecycle() {
        use std::os::windows::io::AsRawHandle;
        use std::process::Command;

        let guard = windows_process::JobObjectGuard::new();
        assert!(
            guard.is_some(),
            "JobObjectGuard creation must succeed on Windows"
        );
        let guard = guard.unwrap();

        let mut child = Command::new("cmd")
            .args(["/C", "ping -n 10 127.0.0.1"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn test process");

        let assigned = guard.assign_process(child.as_raw_handle());
        assert!(assigned, "assigning process to job object must succeed");

        let terminated = guard.terminate(1);
        assert!(terminated, "terminating job object must succeed");

        let status = child.wait().expect("wait child");
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
