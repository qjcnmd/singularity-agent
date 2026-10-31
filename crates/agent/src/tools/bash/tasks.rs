//! 后台命令统一从会话记录读取；运行中的输出和进程控制保留在内存中。

use std::collections::BTreeMap;
use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use singularity_protocol::{BackgroundTask, BackgroundTaskState};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::session::{LedgerRecord, SessionEntry, lock_writer};
use crate::tools::registry::{ABORTED_MESSAGE, ToolDisplay, ToolExecution, error_result};

use super::exec::{Outcome, Process, ProcessEvent};
use super::result::{state_text, task_result, task_summary};
use super::spec::BashArgs;

/// 已保存的开始、结束，或仅供显示的输出更新。
pub enum BashTaskUpdate {
    Updated(BackgroundTask),
    Failed { task: BackgroundTask, error: String },
}

struct Entry {
    task: Mutex<BackgroundTask>,
    cancel: CancellationToken,
    done: watch::Sender<Option<Result<(), CompletionError>>>,
}

enum CompletionError {
    Cleanup(String),
    Store(String),
}

impl CompletionError {
    fn message(&self) -> &str {
        match self {
            Self::Cleanup(message) | Self::Store(message) => message,
        }
    }
}

/// 一个会话的后台命令查询与进程所有权；成功收尾后释放内存条目，不接管历史 PID。
pub struct BashTasks {
    entries: Mutex<BTreeMap<String, Arc<Entry>>>,
    on_update: Arc<dyn Fn(BashTaskUpdate) + Send + Sync>,
    writer: crate::session::SessionWriter,
    inbox: crate::agent::SteeringInboxHandle,
}

impl BashTasks {
    /// 保存复用会话写者；每次更新直接发布，发布完成后才继续执行。
    pub fn new(
        writer: crate::session::SessionWriter,
        inbox: crate::agent::SteeringInboxHandle,
        on_update: Arc<dyn Fn(BashTaskUpdate) + Send + Sync>,
    ) -> Arc<Self> {
        Arc::new(Self {
            entries: Mutex::new(BTreeMap::new()),
            on_update,
            writer,
            inbox,
        })
    }

    /// 界面和工具共用的后台列表；实时输出覆盖运行记录，历史中未结束的命令标为中断。
    pub fn list(&self) -> Vec<BackgroundTask> {
        self.project_tasks(clone_task)
    }

    fn project_tasks<T>(&self, project: impl Fn(&BackgroundTask, &BackgroundTaskState) -> T) -> Vec<T> {
        let writer = lock_writer(&self.writer);
        let entries = self.entries.lock().expect("bash task entries lock");
        let mut tasks = BTreeMap::new();
        for record in writer.entries() {
            if let Some(task) = recorded_task(record) {
                tasks.insert(&task.task_id, task);
            }
        }
        tasks.into_values().map(|task| current_task(task, &entries, &project)).collect()
    }

    /// 命令执行、回收或保存终态期间仍占用会话；完成通知入队后才释放。
    pub fn is_busy(&self) -> bool {
        self.entries
            .lock()
            .expect("bash task entries lock")
            .values()
            .any(|entry| entry.done.borrow().is_none())
    }

    /// 给模型的列表只含 ID、命令、说明和状态，输出由 task_output 按需读取。
    pub(crate) fn tool_list(&self) -> ToolExecution {
        let tasks = self.project_tasks(|task, state| {
            (
                format!("{}: {} — {}\n{}", task.task_id, state_text(state), task.description, task.command),
                task_summary(task, state),
            )
        });
        let mut result = ToolExecution::text(if tasks.is_empty() {
            "(no background tasks)".into()
        } else {
            tasks.iter().map(|(text, _)| text.as_str()).collect::<Vec<_>>().join("\n\n")
        });
        result.display = Some(Box::new(ToolDisplay {
            output: serde_json::to_string(&tasks.into_iter().map(|(_, summary)| summary).collect::<Vec<_>>())
                .expect("task summaries serialize"),
            background_task_id: None,
        }));
        result
    }

    /// 从显式游标读取模型正文；等待与读取不消费输出，也不取消后台命令。
    pub(crate) async fn output(
        &self,
        task_id: &str,
        cursor: u64,
        wait_ms: u64,
        signal: &CancellationToken,
    ) -> ToolExecution {
        if let Some(entry) = self.entry(task_id) {
            let mut done = entry.done.subscribe();
            if wait_ms > 0 && done.borrow().is_none() {
                tokio::select! {
                    biased;
                    _ = signal.cancelled() => return error_result(ABORTED_MESSAGE),
                    _ = tokio::time::sleep(Duration::from_millis(wait_ms)) => {},
                    _ = wait_done(&mut done) => {},
                }
            }
            if let Some(Err(error)) = &*done.borrow() {
                return error_result(error.message());
            }
        }
        if signal.is_cancelled() {
            return error_result(ABORTED_MESSAGE);
        }
        self.task(task_id)
            .map(|task| task_result(&task, Some(cursor), true))
            .unwrap_or_else(error_result)
    }

    /// 取消后台命令并等待回收、保存终态；操作成功后返回命令结果。
    pub async fn stop(&self, task_id: &str) -> Result<ToolExecution, String> {
        if let Some(entry) = self.entry(task_id) {
            let mut done = entry.done.subscribe();
            if done.borrow().is_none() {
                entry.cancel.cancel();
                wait_done(&mut done).await;
            }
            if let Some(Err(error)) = &*done.borrow() {
                return Err(error.message().to_owned());
            }
        }
        self.task(task_id).map(|task| task_result(&task, None, false))
    }

    /// 停止并等待全部活进程；只返回回收错误，已结束命令的保存失败不阻止关闭或删除。
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
            if let Some(Err(CompletionError::Cleanup(error))) = &*done.borrow() {
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
        signal: CancellationToken,
        mut on_update: impl FnMut(String) -> F + Send,
    ) -> ToolExecution {
        if signal.is_cancelled() {
            return error_result(ABORTED_MESSAGE);
        }
        let mut process = match Process::spawn(&args, &cwd) {
            Ok(process) => process,
            Err(error) => return error_result(error),
        };
        let mut output = String::new();
        if args.yield_time_ms > 0 {
            let yield_at = tokio::time::sleep(Duration::from_millis(args.yield_time_ms));
            tokio::pin!(yield_at);
            loop {
                tokio::select! {
                    biased;
                    event = process.next(&signal) => match event {
                        ProcessEvent::Progress { display, .. } => { output = display.clone(); on_update(display).await; }
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
            command: args.command,
            description: args.description,
            output,
            model_output: process.model_output(),
            state: BackgroundTaskState::Running,
        };
        let task_id = task.task_id.clone();
        let (done, _) = watch::channel(None);
        let entry = Arc::new(Entry {
            task: Mutex::new(task.clone()),
            cancel: CancellationToken::new(),
            done,
        });
        let manager = Arc::clone(self);
        let started_task = task.clone();
        let started_task_id = task_id.clone();
        let started_entry = Arc::clone(&entry);
        let started = crate::session::with_writer_async(&self.writer, move |writer| {
            writer.append_record(LedgerRecord::BackgroundTaskUpdated { task: started_task })?;
            // 查询按同一顺序锁定写者和进程表，不会把刚保存的开始记录误认为历史中断。
            manager
                .entries
                .lock()
                .expect("bash task entries lock")
                .insert(started_task_id, started_entry);
            Ok(())
        })
        .await;
        if let Err(error) = started {
            return process.finish(Outcome::Failed(error.to_string())).await.0;
        }
        let result = task_result(&task, Some(0), false);
        (self.on_update)(BashTaskUpdate::Updated(task));
        // 保存开始记录期间可能已经收到停止；这时等进程回收、最终状态保存后再返回。
        if signal.is_cancelled() || entry.cancel.is_cancelled() {
            entry.cancel.cancel();
            self.clone().run_background(entry.clone(), process).await;
            let mut result = self
                .task(&task_id)
                .map(|task| task_result(&task, Some(0), false))
                .unwrap_or_else(error_result);
            result.is_error = true;
            return result;
        }
        let manager = self.clone();
        tokio::spawn(async move {
            manager.run_background(entry, process).await;
        });
        result
    }

    fn entry(&self, task_id: &str) -> Option<Arc<Entry>> {
        self.entries.lock().expect("bash task entries lock").get(task_id).cloned()
    }

    fn task(&self, task_id: &str) -> Result<BackgroundTask, String> {
        let writer = lock_writer(&self.writer);
        let entries = self.entries.lock().expect("bash task entries lock");
        writer
            .entries()
            .iter()
            .rev()
            .filter_map(recorded_task)
            .find(|task| task.task_id == task_id)
            .map(|task| current_task(task, &entries, clone_task))
            .ok_or_else(|| format!("Unknown background task: {task_id}"))
    }

    async fn run_background(self: Arc<Self>, entry: Arc<Entry>, mut process: Process) {
        let outcome = loop {
            match process.next(&entry.cancel).await {
                ProcessEvent::Progress { display, model } => {
                    let task = {
                        let mut task = entry.task.lock().expect("bash task lock");
                        task.output = display;
                        task.model_output = model;
                        task.clone()
                    };
                    (self.on_update)(BashTaskUpdate::Updated(task));
                }
                ProcessEvent::Finished(outcome) => break outcome,
            }
        };
        let (result, outcome, model_output) = process.finish(outcome).await;
        let mut task = entry.task.lock().expect("bash task lock").clone();
        task.output = result.display.map_or(result.content, |display| display.output);
        task.model_output = model_output;
        task.state = outcome.state(result.duration_ms.expect("process duration"));
        let task_id = task.task_id.clone();
        let saved_task = task.clone();
        let saved = crate::session::with_writer_async(&self.writer, move |writer| {
            writer.append_record(LedgerRecord::BackgroundTaskUpdated { task: saved_task })
        })
        .await;
        let completion = match (&outcome, &saved) {
            (Outcome::CleanupFailed(message), _) => Err(CompletionError::Cleanup(message.clone())),
            (_, Err(error)) => Err(CompletionError::Store(error.to_string())),
            _ => Ok(()),
        };
        match saved {
            Ok(_) => {
                let status = match &task.state {
                    BackgroundTaskState::Exited { exit_code, .. } => {
                        Some(format!("exited with code {exit_code}"))
                    }
                    BackgroundTaskState::Failed { message, .. } => Some(format!("failed: {message}")),
                    _ => None,
                };
                // 先把完成通知放进输入队列，再标记命令结束，避免 `has_work` 在两者之间误判为空闲。
                if let Some(status) = status {
                    self.inbox
                        .lock()
                        .expect("turn inbox lock poisoned")
                        .enqueue(crate::agent::QueuedInput::background(task.task_id.clone(), status));
                }
                *entry.task.lock().expect("bash task lock") = task.clone();
                (self.on_update)(BashTaskUpdate::Updated(task));
                let finished = completion.is_ok();
                entry.done.send_replace(Some(completion));
                if finished {
                    self.entries.lock().expect("bash task entries lock").remove(&task_id);
                }
            }
            Err(error) => {
                let error = error.to_string();
                task.state = BackgroundTaskState::Failed {
                    message: format!("保存后台命令结果失败：{error}"),
                    duration_ms: result.duration_ms.expect("process duration"),
                };
                // 未保存的结果不能从日志读回，保留它和错误，直到本次会话关闭。
                *entry.task.lock().expect("bash task lock") = task.clone();
                (self.on_update)(BashTaskUpdate::Failed { task, error });
                entry.done.send_replace(Some(completion));
            }
        }
    }
}

async fn wait_done(done: &mut watch::Receiver<Option<Result<(), CompletionError>>>) {
    while done.borrow().is_none() {
        if done.changed().await.is_err() {
            break;
        }
    }
}

fn recorded_task(entry: &SessionEntry) -> Option<&BackgroundTask> {
    match entry {
        SessionEntry::Record {
            record: LedgerRecord::BackgroundTaskUpdated { task },
            ..
        } => Some(task),
        _ => None,
    }
}

fn clone_task(task: &BackgroundTask, state: &BackgroundTaskState) -> BackgroundTask {
    BackgroundTask { state: state.clone(), ..task.clone() }
}

fn current_task<T>(
    task: &BackgroundTask,
    entries: &BTreeMap<String, Arc<Entry>>,
    project: impl FnOnce(&BackgroundTask, &BackgroundTaskState) -> T,
) -> T {
    if task.state.is_running() {
        if let Some(entry) = entries.get(&task.task_id) {
            let task = entry.task.lock().expect("bash task lock");
            return project(&task, &task.state);
        }
        return project(task, &BackgroundTaskState::Interrupted);
    }
    project(task, &task.state)
}
