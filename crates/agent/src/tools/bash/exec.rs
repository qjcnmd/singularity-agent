//! bash 工具执行环：进程树管理、主等待/排空循环与退出状态投影。

use std::io;
use std::process::ExitStatus;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use crate::tools::registry::{ABORTED_MESSAGE, ExecuteContext, ToolExecution, error_result};

use super::capture::CaptureState;
use super::job_object;
use super::pump::pump_output;
use super::shell::shell_command;
use super::spec::{BashArgs, DEFAULT_TIMEOUT_MS};

/// 输出分块读取管道的容量上限。
const OUTPUT_QUEUE_CAPACITY: usize = 32;
/// 主等待环与排空阶段的轮询切片：recv_timeout 粗粒度醒来检查取消、超时与退出。
const OUTPUT_POLL_INTERVAL: Duration = Duration::from_millis(25);
/// 子进程退出后排空残留缓冲输出的宽限，超时则停止 pump 并标记截断。
const OUTPUT_DRAIN_GRACE: Duration = Duration::from_millis(2_000);
/// 后台进程仍持有管道写端导致输出被截断时的可见标记。
pub(super) const OUTPUT_TRUNCATED_BACKGROUND_NOTE: &str =
    "[output truncated: a background process is still writing]";

pub(crate) fn execute(args: &BashArgs, ctx: ExecuteContext<'_>) -> ToolExecution {
    let ExecuteContext {
        cwd,
        signal,
        mut on_update,
        ..
    } = ctx;
    let command = args.command.as_str();
    let timeout_ms = args.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS);
    let (shell, shell_args) = match shell_command(command) {
        Ok(command) => command,
        Err(error) => return error_result(error),
    };
    if signal.is_cancelled() {
        return error_result(ABORTED_MESSAGE);
    }
    let mut managed = match job_object::spawn_in_job(&shell, &shell_args, cwd) {
        Ok(child) => child,
        Err(error) => {
            return error_result(format!("failed to spawn shell {shell}: {error}"));
        }
    };
    // 不变量：job_object::spawn_in_job 配置了 piped stdout/stderr，take 必为 Some。
    #[allow(clippy::expect_used)]
    let stdout = managed.child.stdout.take().expect("bash stdout is piped");
    #[allow(clippy::expect_used)]
    let stderr = managed.child.stderr.take().expect("bash stderr is piped");
    let stop = Arc::new(AtomicBool::new(false));
    let (sender, receiver) = mpsc::sync_channel(OUTPUT_QUEUE_CAPACITY);
    let stderr_sender = sender.clone();
    {
        let stdout_stop = Arc::clone(&stop);
        let stderr_stop = Arc::clone(&stop);
        thread::spawn(move || pump_output(stdout, sender, stdout_stop, "stdout"));
        thread::spawn(move || pump_output(stderr, stderr_sender, stderr_stop, "stderr"));
    }

    let mut state = CaptureState::new(command);
    let started = Instant::now();
    let mut output_errors = Vec::new();
    let mut readers_drained = false;
    // 运行阶段：按粗粒度切片等待输出块，并在每次醒来的间隙检查取消与超时。
    // 双泵 EOF（Disconnected）只说明管道已关闭；退出状态仍必须从子进程回收，
    // 此后改为纯定时轮询直到观察到退出。
    //
    // 本环只确定结束原因：正常观察到退出、取消、超时、输出错误与等待错误保留
    // 各自区别；离开循环后统一终止、回收并生成一次结果。
    let outcome = loop {
        if !readers_drained {
            match receiver.recv_timeout(OUTPUT_POLL_INTERVAL) {
                Ok(Ok(chunk)) => state.ingest(&chunk),
                Ok(Err(error)) => {
                    // 活动阶段的读错直接停止命令；排空阶段的读错另行汇总。
                    break BashOutcome::OutputFailed(error);
                }
                Err(RecvTimeoutError::Disconnected) => readers_drained = true,
                Err(RecvTimeoutError::Timeout) => {}
            }
        } else {
            thread::sleep(OUTPUT_POLL_INTERVAL);
        }
        publish_output(&mut state, &mut on_update);
        if signal.is_cancelled() {
            break BashOutcome::Aborted;
        }
        if started.elapsed() >= Duration::from_millis(timeout_ms) {
            break BashOutcome::TimedOut(timeout_ms);
        }
        match managed.try_wait() {
            Ok(Some(status)) => break BashOutcome::Completed(status),
            Ok(None) => {}
            // 观察失败只是本次调用的结束原因之一：它与其他非正常结束共用后面的
            // 回收与输出收尾，已捕获的输出和完整输出提示因此不会在错误分支上丢掉。
            Err(error) => break BashOutcome::WaitFailed(error),
        }
    };
    // 唯一的回收点：自然观察到退出就已经回收了子进程；取消、超时、输出读取失败
    // 与等待失败都必须终止进程树并在同一个有界窗口内等它结束。回收失败只作为
    // 附加信息进入结果，不覆盖上面确定的结束原因。
    let cleanup_failures = if matches!(outcome, BashOutcome::Completed(_)) {
        Vec::new()
    } else {
        managed.reclaim()
    };
    // 排空阶段：主进程已退出（或已被整树终止），但管道中可能仍有缓冲输出，
    // 或子进程树成员仍持有写端。单一接收体覆盖两个时间窗口：第一窗口等待尾部输出，
    // 到 OUTPUT_DRAIN_GRACE 期限时停止 pump 并把截止时间切换到第二窗口；第二窗口
    // 读至 pump 停止产出为止。进入第二窗口即说明后台进程仍持有写端，输出按截断处理。
    let mut output_truncated_by_background = false;
    if !readers_drained {
        let grace_deadline = Instant::now() + OUTPUT_DRAIN_GRACE;
        let mut converge_deadline: Option<Instant> = None;
        loop {
            let wait = match converge_deadline {
                Some(deadline) => match deadline.checked_duration_since(Instant::now()) {
                    Some(remaining) => remaining.min(OUTPUT_POLL_INTERVAL),
                    None => break,
                },
                None => OUTPUT_POLL_INTERVAL,
            };
            match receiver.recv_timeout(wait) {
                Ok(Ok(chunk)) => state.ingest(&chunk),
                Ok(Err(error)) => output_errors.push(error),
                Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => {}
            }
            publish_output(&mut state, &mut on_update);
            if converge_deadline.is_none() && Instant::now() >= grace_deadline {
                stop.store(true, Ordering::SeqCst);
                converge_deadline = Some(Instant::now() + OUTPUT_DRAIN_GRACE);
                output_truncated_by_background = true;
            }
        }
    }

    let mut content = state.final_output();
    // 附加失败信息：输出读取错误与进程回收失败。它们都排在结束原因之后，只补充
    // 事实，不改变主原因。
    let mut auxiliary_failures: Vec<String> = output_errors
        .into_iter()
        .map(|error| error.to_string())
        .collect();
    auxiliary_failures.extend(cleanup_failures);
    let is_error = append_outcome(&mut content, outcome, &auxiliary_failures);
    if output_truncated_by_background {
        // 后台进程仍持有管道写端；命令本身已结束，截断仅为信息提示而非错误。
        append_status(&mut content, OUTPUT_TRUNCATED_BACKGROUND_NOTE);
    }
    // 保存完整输出的结果独立于命令退出状态；失败时保留原因，避免误报完整路径。
    state.ensure_spill_for_final_truncation();
    if let Some(spill) = &state.spill {
        let note = match spill {
            Ok(spill) => format!("Full output: {}", spill.path.display()),
            Err(error) => format!("Full output could not be saved: {error}"),
        };
        append_status(&mut content, &note);
    }
    ToolExecution {
        content,
        is_error,
        diff: None,
        duration_ms: None,
        read_source: None,
    }
}

/// 把结束原因与附加失败信息写进结果文本：结束原因在前，附加信息只补充事实。
/// 返回本次调用是否为失败结果。
fn append_outcome(
    content: &mut String,
    outcome: BashOutcome,
    auxiliary_failures: &[String],
) -> bool {
    let mut is_error = false;
    match outcome {
        BashOutcome::OutputFailed(error) => {
            append_status(content, &error.to_string());
            is_error = true;
        }
        BashOutcome::WaitFailed(error) => {
            // 观察失败只说这次观察失败：已捕获的输出与完整输出路径仍然有效。
            append_status(
                content,
                &format!("failed to wait for the command process: {error}"),
            );
            is_error = true;
        }
        BashOutcome::Aborted => {
            append_status(content, ABORTED_MESSAGE);
            is_error = true;
        }
        BashOutcome::TimedOut(ms) => {
            append_status(
                content,
                &format!(
                    "Command timed out after {ms} ms and was terminated; the output above is what \
                     it produced before that. Re-run with a larger timeout_ms if the work needs \
                     more time, or narrow the command."
                ),
            );
            is_error = true;
        }
        BashOutcome::Completed(status) => {
            if status.success() {
                if content.is_empty() {
                    *content = "(no output)".to_string();
                }
            } else {
                append_status(content, &describe_exit(status));
                is_error = true;
            }
        }
    }
    for failure in auxiliary_failures {
        append_status(content, failure);
        is_error = true;
    }
    is_error
}

fn publish_output(state: &mut CaptureState, on_update: &mut Option<&mut dyn FnMut(String)>) {
    if let Some(callback) = on_update.as_mut()
        && let Some(output) = state.current_output()
    {
        callback(output);
    }
}

fn append_status(content: &mut String, status: &str) {
    if !content.is_empty() {
        content.push_str("\n\n");
    }
    content.push_str(status);
}

/// 把失败退出状态投影为错误文案。
fn describe_exit(status: ExitStatus) -> String {
    match status.code() {
        Some(code) => format!("Command exited with code {code}"),
        None => "Command terminated".to_string(),
    }
}

enum BashOutcome {
    Completed(ExitStatus),
    Aborted,
    TimedOut(u64),
    OutputFailed(io::Error),
    WaitFailed(io::Error),
}
