use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use process_wrap::tokio::{ChildWrapper, CommandWrap, CreationFlags, JobObject, KillOnDrop};
use protocol::{BackgroundTaskState, CommandOutputSnapshot};
use tokio::process::Command;
use tokio::time::{Instant, Interval};
use tokio_util::sync::CancellationToken;

use crate::tools::registry::{ABORTED_MESSAGE, ToolDisplay, ToolExecution};

use super::pump::{OutputChunk, OutputStreams};
use super::shell::bash_path;
use super::spec::BashArgs;
use crate::tools::output::CaptureState;

const OUTPUT_DRAIN_GRACE: Duration = Duration::from_secs(2);
pub(super) const OUTPUT_TRUNCATED_BACKGROUND_NOTE: &str =
    "[output truncated: a descendant process was still writing when the command exited]";

pub(super) enum ProcessEvent {
    Progress {
        display: String,
        model: CommandOutputSnapshot,
    },
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
    model_capture: CaptureState,
    progress: Interval,
    started: Instant,
    timeout: Option<(Instant, u64)>,
}

impl Process {
    pub(super) fn persist_output(&mut self) {
        tokio::task::block_in_place(|| self.model_capture.persist());
    }

    fn ingest(&mut self, chunk: &OutputChunk) {
        self.capture.ingest(&chunk.display);
        let force_spill = self.capture.is_truncated() && !self.model_capture.has_spill();
        if force_spill || self.model_capture.will_write_disk(&chunk.model) {
            tokio::task::block_in_place(|| {
                self.model_capture.ingest(&chunk.model);
                if force_spill {
                    self.model_capture.persist();
                }
            });
        } else {
            self.model_capture.ingest(&chunk.model);
        }
    }

    pub(super) async fn spawn(args: &BashArgs, cwd: &Path, output_directory: &Path) -> Result<Self, String> {
        let command_text = args.command.clone();
        let cwd = cwd.to_path_buf();
        let timeout_ms = args.timeout_ms.map(std::num::NonZeroU64::get);
        let (mut child, started, timeout) = tokio::task::spawn_blocking(move || {
            let shell = bash_path()?;
            let started = Instant::now();
            let timeout = timeout_ms.map(|ms| (started + Duration::from_millis(ms), ms));
            let mut command = Command::new(&shell);
            command
                .args(["-c", &command_text])
                .current_dir(&cwd)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            // Windows Python defaults to the ANSI code page (e.g. CP936) on piped stdio; force UTF-8 unless overridden.
            if std::env::var_os("PYTHONUTF8").is_none() {
                command.env("PYTHONUTF8", "1");
            }
            let mut wrapped = CommandWrap::from(command);
            wrapped
                .wrap(CreationFlags(windows::Win32::System::Threading::CREATE_NO_WINDOW))
                .wrap(JobObject)
                .wrap(KillOnDrop);
            let child = wrapped
                .spawn()
                .map_err(|error| format!("failed to spawn shell {}: {error}", shell.display()))?;
            Ok::<_, String>((child, started, timeout))
        })
        .await
        .expect("bash process spawn completes while the runtime is running")?;
        let output = OutputStreams::new(
            child.stdout().take().expect("bash stdout is piped"),
            child.stderr().take().expect("bash stderr is piped"),
        );
        let mut progress = tokio::time::interval(Duration::from_millis(100));
        progress.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        Ok(Self {
            child,
            output,
            capture: CaptureState::new_in_memory(),
            model_capture: CaptureState::new(output_directory.to_path_buf()),
            progress,
            started,
            timeout,
        })
    }

    pub(super) async fn next(&mut self, signal: &CancellationToken) -> ProcessEvent {
        loop {
            tokio::select! {
                biased;
                () = signal.cancelled() => return ProcessEvent::Finished(Outcome::Stopped),
                ms = wait_deadline(self.timeout) => return ProcessEvent::Finished(Outcome::Failed(format!(
                    "Command timed out after {ms} ms and was terminated"
                ))),
                status = self.child.inner_mut().wait() => return ProcessEvent::Finished(match status {
                    Ok(status) => match status.code() {
                        Some(code) => Outcome::Exited(code),
                        None => Outcome::Failed("Command terminated without an exit code".to_string()),
                    },
                    Err(error) => Outcome::Failed(format!("failed to wait for the command process: {error}")),
                }),
                _ = self.progress.tick(), if self.capture.has_pending_output() => {},
                chunk = self.output.next(), if self.output.is_open() => match chunk {
                    Some(Ok(chunk)) => self.ingest(&chunk),
                    Some(Err(error)) => return ProcessEvent::Finished(Outcome::Failed(error.to_string())),
                    None => {},
                }
            }
            if let Some(output) = self.capture.current_output(self.model_capture.spill_note().as_deref()) {
                return ProcessEvent::Progress {
                    display: output,
                    model: self.model_output(),
                };
            }
        }
    }

    pub(super) fn display_output(&self) -> String {
        self.capture.display_output(self.model_capture.spill_note().as_deref())
    }

    pub(super) fn model_output(&self) -> CommandOutputSnapshot {
        self.model_capture.snapshot()
    }

    pub(super) async fn finish(
        mut self,
        mut outcome: Outcome,
    ) -> (ToolExecution, Outcome, CommandOutputSnapshot) {
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
                () = &mut drain => { truncated = true; break; }
                chunk = self.output.next() => match chunk {
                    Some(Ok(chunk)) => self.ingest(&chunk),
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
        let mut model_output = self.model_capture.snapshot();
        model_output.incomplete = truncated;
        let (display_content, _) = self.capture.finish();
        let (model_content, spill_note) = self.model_capture.finish();
        let display = finished_output(display_content, spill_note.as_deref(), &outcome, truncated);
        let mut execution =
            ToolExecution::text(finished_output(model_content, spill_note.as_deref(), &outcome, truncated));
        if display != execution.content {
            execution.display = Some(Box::new(ToolDisplay {
                output: display,
                background_task_id: None,
            }));
        }
        execution.is_error = !matches!(outcome, Outcome::Exited(0));
        execution.duration_ms = Some(app_core::duration_millis(self.started.elapsed()));
        (execution, outcome, model_output)
    }
}

fn finished_output(
    mut content: String,
    spill_note: Option<&str>,
    outcome: &Outcome,
    truncated: bool,
) -> String {
    match outcome {
        Outcome::Exited(0) => {}
        Outcome::Exited(code) => append_status(&mut content, &format!("Command exited with code {code}")),
        Outcome::Stopped => append_status(&mut content, ABORTED_MESSAGE),
        Outcome::Failed(message) | Outcome::CleanupFailed(message) => append_status(&mut content, message),
    }
    if content.is_empty() {
        content = "(no output)".to_string();
    }
    if truncated {
        append_status(&mut content, OUTPUT_TRUNCATED_BACKGROUND_NOTE);
    }
    if let Some(note) = spill_note {
        append_status(&mut content, note);
    }
    content
}

async fn wait_deadline(timeout: Option<(Instant, u64)>) -> u64 {
    match timeout {
        Some((deadline, ms)) => {
            tokio::time::sleep_until(deadline).await;
            ms
        }
        None => std::future::pending().await,
    }
}

fn append_status(content: &mut String, status: &str) {
    if !content.is_empty() {
        content.push_str("\n\n");
    }
    content.push_str(status);
}
