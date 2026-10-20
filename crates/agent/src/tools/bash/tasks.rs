//! 会话拥有的后台进程：保存开始与终态，实时发布输出。

use std::collections::BTreeMap;
use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use singularity_protocol::{BackgroundTask, BackgroundTaskState};
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::tools::registry::{ABORTED_MESSAGE, ToolExecution, error_result};

use super::exec::{Outcome, Process, ProcessEvent};
use super::spec::BashArgs;

/// 已保存的开始、结束，或仅供显示的输出更新。
pub enum BashTaskUpdate {
    Updated(BackgroundTask),
    Failed(String),
}

struct Entry {
    task: Mutex<BackgroundTask>,
    cancel: CancellationToken,
    done: watch::Sender<Option<Result<(), String>>>,
}

/// 一个会话的后台命令与其进程所有权；不接管历史 PID。
pub struct BashTasks {
    entries: Mutex<BTreeMap<String, Arc<Entry>>>,
    updates: mpsc::Sender<BashTaskUpdate>,
    writer: crate::session::SessionWriter,
    inbox: crate::agent::SteeringInboxHandle,
}

impl BashTasks {
    /// 创建管理器与状态事件队列，保存复用会话写者。
    pub fn new(
        writer: crate::session::SessionWriter,
        inbox: crate::agent::SteeringInboxHandle,
    ) -> (Arc<Self>, mpsc::Receiver<BashTaskUpdate>) {
        let (updates, receiver) = mpsc::channel(32);
        (
            Arc::new(Self {
                entries: Mutex::new(BTreeMap::new()),
                updates,
                writer,
                inbox,
            }),
            receiver,
        )
    }

    /// 返回后台任务的当前状态。
    pub fn list(&self) -> Vec<BackgroundTask> {
        self.entries
            .lock()
            .expect("bash task entries lock")
            .values()
            .map(|entry| entry.task.lock().expect("bash task lock").clone())
            .collect()
    }

    /// 读取累计输出；只等待指定窗口，不取消后台命令。
    pub(crate) async fn output(
        &self,
        task_id: &str,
        wait_ms: u64,
        signal: &CancellationToken,
    ) -> ToolExecution {
        let entry = match self.entry(task_id) {
            Ok(entry) => entry,
            Err(result) => return result,
        };
        let mut done = entry.done.subscribe();
        if wait_ms > 0 && done.borrow().is_none() {
            tokio::select! {
                biased;
                _ = signal.cancelled() => return error_result(ABORTED_MESSAGE),
                _ = tokio::time::sleep(Duration::from_millis(wait_ms)) => {},
                _ = wait_done(&mut done) => {},
            }
        }
        entry_result(&entry)
    }

    /// 取消后台命令，并等待回收及终态确认；重复停止返回原结果。
    pub async fn stop(&self, task_id: &str) -> ToolExecution {
        let entry = match self.entry(task_id) {
            Ok(entry) => entry,
            Err(result) => return result,
        };
        let mut done = entry.done.subscribe();
        if done.borrow().is_none() {
            entry.cancel.cancel();
            wait_done(&mut done).await;
        }
        entry_result(&entry)
    }

    /// 停止并等待全部活进程；此后仍可启动新命令。
    pub async fn shutdown(&self) -> Result<(), String> {
        let entries: Vec<_> =
            self.entries.lock().expect("bash task entries lock").values().cloned().collect();
        for entry in &entries {
            entry.cancel.cancel();
        }
        let mut failures = Vec::new();
        for entry in entries {
            let mut done = entry.done.subscribe();
            wait_done(&mut done).await;
            if let Some(Err(error)) = &*done.borrow() {
                failures.push(error.clone());
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(failures.join("; "))
        }
    }

    /// 执行一个真实命令；等待窗口结束后把同一进程转交给后台管理器。
    pub(crate) async fn execute<F: Future<Output = ()> + Send>(
        self: &Arc<Self>,
        args: BashArgs,
        cwd: PathBuf,
        turn_id: String,
        item_id: String,
        signal: CancellationToken,
        mut on_update: impl FnMut(String) -> F + Send,
    ) -> ToolExecution {
        if signal.is_cancelled() {
            return error_result(ABORTED_MESSAGE);
        }
        let started_at = singularity_core::now_iso();
        let mut process = match Process::spawn(&args, &cwd).await {
            Ok(process) => process,
            Err(error) => return error_result(error),
        };
        let mut output = String::new();
        if !args.run_in_background {
            let yield_at = tokio::time::sleep(Duration::from_millis(args.yield_time_ms.get()));
            tokio::pin!(yield_at);
            loop {
                tokio::select! {
                    biased;
                    event = process.next(&signal) => match event {
                        ProcessEvent::Progress(progress) => { output = progress.clone(); on_update(progress).await; }
                        ProcessEvent::Finished(outcome) => return process.finish(outcome).await.0,
                    },
                    _ = &mut yield_at => break,
                }
            }
        }
        if signal.is_cancelled() {
            return process.finish(Outcome::Stopped).await.0;
        }
        let task = BackgroundTask {
            task_id: Uuid::now_v7().to_string(),
            turn_id,
            item_id,
            command: args.command,
            description: args.description.unwrap_or_default(),
            started_at,
            output,
            state: BackgroundTaskState::Running,
        };
        let task_id = task.task_id.clone();
        let (done, _) = watch::channel(None);
        let entry = Arc::new(Entry {
            task: Mutex::new(task.clone()),
            cancel: CancellationToken::new(),
            done,
        });
        if let Err(error) = self.save(task.clone()).await {
            return process.finish(Outcome::Failed(error)).await.0;
        }
        self.entries.lock().expect("bash task entries lock").insert(task_id, entry.clone());
        let _ = self.updates.send(BashTaskUpdate::Updated(task)).await;
        // 保存期间仍可能收到停止，完成回收和落盘后再返回。
        if signal.is_cancelled() || entry.cancel.is_cancelled() {
            entry.cancel.cancel();
            self.clone().run_background(entry.clone(), process).await;
            let mut result = entry_result(&entry);
            result.is_error = true;
            return result;
        }
        let result = entry_result(&entry);
        let manager = self.clone();
        tokio::spawn(async move {
            manager.run_background(entry, process).await;
        });
        result
    }

    fn entry(&self, task_id: &str) -> Result<Arc<Entry>, ToolExecution> {
        self.entries
            .lock()
            .expect("bash task entries lock")
            .get(task_id)
            .cloned()
            .ok_or_else(|| error_result(format!("Unknown background task: {task_id}")))
    }

    async fn save(&self, task: BackgroundTask) -> Result<(), String> {
        crate::session::with_writer_async(&self.writer, move |writer| {
            writer.append_record(crate::session::LedgerRecord::BackgroundTaskUpdated { task })
        })
        .await
        .map(|_| ())
        .map_err(|error| error.to_string())
    }

    async fn run_background(self: Arc<Self>, entry: Arc<Entry>, mut process: Process) {
        let outcome = loop {
            match process.next(&entry.cancel).await {
                ProcessEvent::Progress(output) => {
                    let mut task = entry.task.lock().expect("bash task lock").clone();
                    task.output = output;
                    *entry.task.lock().expect("bash task lock") = task.clone();
                    let _ = self.updates.send(BashTaskUpdate::Updated(task)).await;
                }
                ProcessEvent::Finished(outcome) => break outcome,
            }
        };
        let (result, outcome) = process.finish(outcome).await;
        let mut task = entry.task.lock().expect("bash task lock").clone();
        task.output = result.content;
        task.state = outcome.state(result.duration_ms.expect("process duration"));
        match self.save(task.clone()).await {
            Ok(()) => {
                let status = match &task.state {
                    BackgroundTaskState::Exited { exit_code, .. } => {
                        Some(format!("exited with code {exit_code}"))
                    }
                    BackgroundTaskState::Failed { message, .. } => Some(format!("failed: {message}")),
                    _ => None,
                };
                // 通知先入箱，再公开终态；移除项目不会漏掉尚未消费的完成通知。
                if let Some(status) = status {
                    self.inbox
                        .lock()
                        .expect("turn inbox lock poisoned")
                        .enqueue(crate::agent::QueuedInput::background(task.task_id.clone(), status));
                }
                *entry.task.lock().expect("bash task lock") = task.clone();
                let _ = self.updates.send(BashTaskUpdate::Updated(task)).await;
                let completion = match outcome {
                    Outcome::CleanupFailed(message) => Err(message),
                    _ => Ok(()),
                };
                entry.done.send_replace(Some(completion));
            }
            Err(error) => {
                task.state = BackgroundTaskState::Failed {
                    message: format!("保存后台命令结果失败：{error}"),
                    finished_at: singularity_core::now_iso(),
                    duration_ms: result.duration_ms.expect("process duration"),
                };
                *entry.task.lock().expect("bash task lock") = task;
                let _ = self.updates.send(BashTaskUpdate::Failed(error.clone())).await;
                entry.done.send_replace(Some(Err(error)));
            }
        }
    }
}

async fn wait_done(done: &mut watch::Receiver<Option<Result<(), String>>>) {
    while done.borrow().is_none() {
        if done.changed().await.is_err() {
            break;
        }
    }
}

fn entry_result(entry: &Entry) -> ToolExecution {
    if let Some(Err(error)) = &*entry.done.borrow() {
        return error_result(error.clone());
    }
    let task = entry.task.lock().expect("bash task lock");
    let mut result = ToolExecution::text(serde_json::to_string(&*task).expect("background task serializes"));
    result.is_error = matches!(task.state,
        BackgroundTaskState::Exited { exit_code, .. } if exit_code != 0)
        || matches!(task.state, BackgroundTaskState::Failed { .. } | BackgroundTaskState::Interrupted);
    result
}
