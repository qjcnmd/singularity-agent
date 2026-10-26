//! 持有进程、作业对象和输出，前台转后台时整体转交，不重新启动命令。

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use process_wrap::tokio::{ChildWrapper, CommandWrap, CreationFlags, JobObject, KillOnDrop};
use singularity_protocol::BackgroundTaskState;
use tokio::process::Command;
use tokio::time::{Instant, Interval};
use tokio_util::sync::CancellationToken;

use crate::tools::registry::{ABORTED_MESSAGE, ToolExecution};

use super::pump::OutputStreams;
use super::shell::bash_path;
use super::spec::BashArgs;
use crate::tools::output::CaptureState;

const OUTPUT_DRAIN_GRACE: Duration = Duration::from_secs(2);
const OUTPUT_TRUNCATED_BACKGROUND_NOTE: &str =
    "[output truncated: a descendant process was still writing when the command exited]";

pub(super) enum ProcessEvent {
    Progress(String),
    Finished(Outcome),
}

pub(super) enum Outcome {
    Exited(i32),
    Stopped,
    Failed(String),
    CleanupFailed(String),
}

impl Outcome {
    pub(super) fn state(&self, duration_ms: u64) -> BackgroundTaskState {
        match self {
            Self::Exited(exit_code) => BackgroundTaskState::Exited { exit_code: *exit_code, duration_ms },
            Self::Stopped => BackgroundTaskState::Stopped { duration_ms },
            Self::Failed(message) | Self::CleanupFailed(message) => {
                BackgroundTaskState::Failed { message: message.clone(), duration_ms }
            }
        }
    }
}

pub(super) struct Process {
    child: Box<dyn ChildWrapper>,
    output: OutputStreams,
    capture: CaptureState,
    progress: Interval,
    started: Instant,
    deadline: Option<Instant>,
    timeout_ms: Option<u64>,
}

impl Process {
    pub(super) fn spawn(args: &BashArgs, cwd: &Path) -> Result<Self, String> {
        let shell = bash_path()?;
        let started = Instant::now();
        let mut command = Command::new(&shell);
        command
            .args(["-c", &args.command])
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut wrapped = CommandWrap::from(command);
        wrapped
            .wrap(CreationFlags(windows::Win32::System::Threading::CREATE_NO_WINDOW))
            .wrap(JobObject)
            .wrap(KillOnDrop);
        let mut child = wrapped.spawn().map_err(|error| format!("failed to spawn shell {shell}: {error}"))?;
        let output = OutputStreams::new(
            child.stdout().take().expect("bash stdout is piped"),
            child.stderr().take().expect("bash stderr is piped"),
        );
        let mut progress = tokio::time::interval(Duration::from_millis(100));
        progress.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let timeout_ms = args.timeout_ms.map(std::num::NonZeroU64::get);
        Ok(Self {
            child,
            output,
            capture: CaptureState::default(),
            progress,
            started,
            deadline: timeout_ms.map(|ms| started + Duration::from_millis(ms)),
            timeout_ms,
        })
    }

    pub(super) async fn next(&mut self, signal: &CancellationToken) -> ProcessEvent {
        loop {
            tokio::select! {
                biased;
                _ = signal.cancelled() => return ProcessEvent::Finished(Outcome::Stopped),
                _ = wait_deadline(self.deadline) => return ProcessEvent::Finished(Outcome::Failed(format!(
                    "Command timed out after {} ms and was terminated", self.timeout_ms.expect("deadline has timeout")
                ))),
                // 先等主进程；finish 再终止并等待整棵进程树。
                status = self.child.inner_mut().wait() => return ProcessEvent::Finished(match status {
                    Ok(status) => match status.code() {
                        Some(code) => Outcome::Exited(code),
                        None => Outcome::Failed("Command terminated without an exit code".to_string()),
                    },
                    Err(error) => Outcome::Failed(format!("failed to wait for the command process: {error}")),
                }),
                _ = self.progress.tick() => {},
                chunk = self.output.next(), if self.output.is_open() => match chunk {
                    Some(Ok(chunk)) => tokio::task::block_in_place(|| self.capture.ingest(&chunk)),
                    Some(Err(error)) => return ProcessEvent::Finished(Outcome::Failed(error.to_string())),
                    None => {},
                }
            }
            if let Some(output) = self.capture.current_output() {
                return ProcessEvent::Progress(output);
            }
        }
    }

    /// 回收整树后才给出终态；主进程正常退出也不能遗留后代。
    pub(super) async fn finish(mut self, mut outcome: Outcome) -> (ToolExecution, Outcome) {
        let mut failures = Vec::new();
        if let Err(error) = self.child.start_kill() {
            failures.push(format!("failed to terminate the command process tree: {error}"));
        }
        // 包装层的 wait 等待整个作业退出，包括主进程留下的后代。
        match tokio::time::timeout(Duration::from_secs(5), self.child.wait()).await {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => failures.push(format!("failed to wait for the command process tree: {error}")),
            Err(_) => failures.push("the command process tree did not exit within 5000 ms".into()),
        }
        let drain = tokio::time::sleep(OUTPUT_DRAIN_GRACE);
        tokio::pin!(drain);
        let mut truncated = false;
        while self.output.is_open() {
            tokio::select! {
                biased;
                _ = &mut drain => { truncated = true; break; }
                chunk = self.output.next() => match chunk {
                    Some(Ok(chunk)) => tokio::task::block_in_place(|| self.capture.ingest(&chunk)),
                    Some(Err(error)) => failures.push(error.to_string()),
                    None => {},
                }
            }
        }
        if !failures.is_empty() {
            let primary = match &outcome {
                Outcome::Failed(message) => format!("{message}; "),
                _ => String::new(),
            };
            outcome = Outcome::CleanupFailed(format!("{primary}{}", failures.join("; ")));
        }
        let (mut content, spill_note) = self.capture.finish();
        let is_error = match &outcome {
            Outcome::Exited(0) => false,
            Outcome::Exited(code) => {
                append_status(&mut content, &format!("Command exited with code {code}"));
                true
            }
            Outcome::Stopped => {
                append_status(&mut content, ABORTED_MESSAGE);
                true
            }
            Outcome::Failed(message) | Outcome::CleanupFailed(message) => {
                append_status(&mut content, message);
                true
            }
        };
        if content.is_empty() {
            content = "(no output)".to_string();
        }
        if truncated {
            append_status(&mut content, OUTPUT_TRUNCATED_BACKGROUND_NOTE);
        }
        if let Some(note) = spill_note {
            append_status(&mut content, &note);
        }
        let mut execution = ToolExecution::text(content);
        execution.is_error = is_error;
        execution.duration_ms = Some(singularity_core::duration_millis(self.started.elapsed()));
        (execution, outcome)
    }
}

async fn wait_deadline(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

fn append_status(content: &mut String, status: &str) {
    if !content.is_empty() {
        content.push_str("\n\n");
    }
    content.push_str(status);
}
