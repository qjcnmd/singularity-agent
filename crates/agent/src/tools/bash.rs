//! bash 工具：进程内执行 shell 命令（对齐 Pi bash 语义）。
//!
//! - 默认超时 120 秒（`timeout_ms` 缺省时生效），超时后杀进程树并返回错误；
//!   模型可显式传 `timeout_ms` 覆盖（长命令）。默认超时是信任边界安全网：
//!   无界命令（如全盘 `find`）若无线索约束会永久挂起并阻塞整个 turn。
//! - stdout+stderr 合并；输出截断到**最后** 2000 行/50KB（保留尾部），超限时把完整输出
//!   写入系统临时目录（`bash-<uuid>.log`），内容尾部附 `[Full output: <path>]` 说明。
//! - 中断信号（`CancellationToken`）到达时杀进程树并返回 "Command aborted"。
//! - 非 0 退出码 → is_error，内容附 "Command exited with code N"。
//! - Windows 上 spawn 前清除本进程 stdout/stderr 句柄的继承位：否则子进程树（含
//!   强杀后的残留孙进程）会继承并直写本进程的 stdout 管道，非 UTF-8 字节会破坏
//!   JSON-RPC 流（CLI 报 "stream did not contain valid UTF-8"）。

use std::fs::File;
use std::io::{self, Read, Write};
use std::path::Path;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(windows)]
#[allow(unsafe_code)] // 唯一 unsafe 模块：单次 SetHandleInformation 调用（见模块注释）。
mod handle_inheritance {
    use std::os::windows::io::{AsRawHandle, RawHandle};

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn SetHandleInformation(h_object: RawHandle, dw_mask: u32, dw_flags: u32) -> i32;
    }

    const HANDLE_FLAG_INHERIT: u32 = 0x0000_0001;

    /// 清除 stdout/stderr 句柄的继承位，防止 spawn 时子进程（及孙进程）继承并
    /// 直写本进程的 stdout/stderr 管道（见模块注释）。尽力而为：句柄无效时忽略。
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
}

use serde_json::{Value, json};

use super::registry::{ExecuteContext, ToolError, ToolExecution, error_result};
use super::truncate::{
    DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, TruncatedBy, format_size, truncate_tail,
};

/// 内部保留缓冲上限：2×50KB（对齐 Pi `maxOutputBytes`），防止单条超大输出撑爆内存；
/// 超过该上限的部分只存在于完整输出临时文件。
const INTERNAL_TAIL_MAX_BYTES: usize = DEFAULT_MAX_BYTES * 2;

/// 完整输出临时文件名的随机后缀。
const FULL_OUTPUT_FILE_PREFIX: &str = "bash-";
const FULL_OUTPUT_FILE_SUFFIX: &str = ".log";

// 数字与 truncate::DEFAULT_MAX_LINES/DEFAULT_MAX_BYTES 保持一致（与 Pi 描述文本等价）。
/// 命令默认超时：模型未显式传 `timeout_ms` 时的安全网，防止无界命令永久挂起。
pub(crate) const DEFAULT_TIMEOUT_MS: u64 = 120_000;
/// 命令超时硬上限：显式 `timeout_ms` 也不能让单条命令超过 10 分钟。
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
        execution_mode: super::registry::ToolExecutionMode::Parallel,
        execute,
    }
}

pub(crate) fn execute(ctx: ExecuteContext<'_>) -> Result<ToolExecution, ToolError> {
    let ExecuteContext {
        args,
        cwd,
        signal,
        mut on_update,
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
    let (shell, shell_args) = shell_command(command);
    #[cfg(windows)]
    handle_inheritance::deny_inherit_std_streams();
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
    let stdout = child.stdout.take().expect("bash stdout is piped");
    let stderr = child.stderr.take().expect("bash stderr is piped");
    let (sender, receiver) = mpsc::channel();
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
                kill_process_tree(&mut child);
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
            // Pi 语义：exitCode 不可得（例如外部终止）时不视为失败。
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

/// 读取管道字节，清洗控制字符、去掉 `\r`，分块送入 channel（Pi `onChunk` 语义）。
fn pump_output(mut reader: impl Read + Send + 'static, sender: mpsc::Sender<String>) {
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

/// 过滤控制字符（保留 `\t`、`\n`、`\r`），对齐 Pi `sanitizeBinaryOutput`；
/// 其余字节按 UTF-8 解码，非法序列替换为 U+FFFD。
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

/// 选择执行 shell（对齐 Pi `getShellConfig`）：Windows 优先 Git Bash，其次 PATH 上的
/// bash，都没有时回退 `cmd /C`（Pi 在无 bash 时直接报错；回退保证工具在任何 Windows
/// 上都可用）；Unix 优先 `/bin/bash`，否则 `sh -c`。
fn shell_command(command: &str) -> (String, Vec<String>) {
    #[cfg(windows)]
    {
        if let Some(bash) = find_bash_on_windows() {
            return (bash, vec!["-c".to_string(), command.to_string()]);
        }
        (
            "cmd".to_string(),
            vec!["/C".to_string(), command.to_string()],
        )
    }
    #[cfg(not(windows))]
    {
        if Path::new("/bin/bash").exists() {
            return (
                "/bin/bash".to_string(),
                vec!["-c".to_string(), command.to_string()],
            );
        }
        (
            "sh".to_string(),
            vec!["-c".to_string(), command.to_string()],
        )
    }
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

/// 杀进程树：Windows 用 `taskkill /T /F`（含子进程），随后 `child.kill()` 兜底；
/// 其他平台直接 kill 直接子进程（Phase 3 再引入平台级进程树枚举）。
fn kill_process_tree(child: &mut Child) {
    #[cfg(windows)]
    {
        let pid = child.id();
        // taskkill 自身绝不能向 app-server 的 stdio JSON-RPC 流写任何输出：
        // stdout/stderr 全部指向 null，避免中断时污染协议帧。
        let _ = Command::new("taskkill")
            .args(["/T", "/F", "/PID", &pid.to_string()])
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

    /// 最终展示文本 + 截断说明（对齐 Pi `createProgress` 与 bash.js 的 note 拼接）。
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
        let (shell, _) = shell_command("");
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
        let (shell, _) = shell_command("");
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
}
