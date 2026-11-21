//! 后台命令统一从会话记录读取；运行中的输出和进程控制保留在内存中。

use std::collections::BTreeMap;
use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use protocol::{BackgroundTask, BackgroundTaskState};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::session::{LedgerRecord, SessionEntry, with_writer};
use crate::tools::registry::{ABORTED_MESSAGE, ToolDisplay, ToolExecution, error_result};

use super::exec::{Outcome, Process, ProcessEvent};
use super::result::{output_text, state_text, task_result, task_summary};
use super::spec::BashArgs;

/// 已保存的开始、结束，或仅供显示的输出更新。
#[derive(Debug)]
pub enum BashTaskUpdate {
    Updated(BackgroundTask),
    Failed { task: BackgroundTask, error: String },
}

struct Entry {
    task: Mutex<BackgroundTask>,
    cancel: CancellationToken,
    done: watch::Sender<Option<Result<(), CompletionError>>>,
}

#[derive(Default, Clone, Copy)]
struct OutputDelivery {
    end: u64,
    terminal: bool,
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
    delivered: Mutex<BTreeMap<String, OutputDelivery>>,
    on_update: Arc<dyn Fn(BashTaskUpdate) + Send + Sync>,
    writer: crate::session::SessionWriter,
    inbox: crate::agent::SteeringInboxHandle,
}

impl std::fmt::Debug for BashTasks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut debug = f.debug_struct("BashTasks");
        if let Ok(entries) = self.entries.try_lock() {
            debug.field("active_entries", &entries.len());
        }
        debug.finish_non_exhaustive()
    }
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
            delivered: Mutex::new(BTreeMap::new()),
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
        with_writer(&self.writer, |writer| {
            let entries = self.entries.lock().expect("bash task entries lock");
            let mut tasks = BTreeMap::new();
            for record in writer.data().entries() {
                if let Some(task) = recorded_task(record) {
                    tasks.insert(&task.task_id, task);
                }
            }
            tasks.into_values().map(|task| current_task(task, &entries, &project)).collect()
        })
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

    /// 主动查看立即返回新增输出；完成通知共用读取位置，界面查询不消费它。
    pub(crate) fn output(&self, task_id: &str, signal: &CancellationToken) -> ToolExecution {
        if signal.is_cancelled() {
            return error_result(ABORTED_MESSAGE);
        }
        self.take_output(task_id).unwrap_or_else(error_result)
    }

    /// 准备后台完成通知但不推进游标；由调用方在消息落盘后再调用 `confirm_delivered`。
    pub(crate) fn prepare_completion_message(
        &self,
        task_id: &str,
    ) -> Result<Option<(String, u64, bool)>, String> {
        let delivered = self.delivered.lock().expect("bash output delivery lock");
        let task = self.task(task_id)?;
        let position = delivered.get(task_id).copied().unwrap_or_default();
        if position.terminal {
            return Ok(None);
        }
        let content = format!(
            "<background-task>\nCommand: {}\nTask {}: {}\n{}\n</background-task>",
            task.command,
            task.task_id,
            state_text(&task.state),
            output_text(&task.model_output, position.end),
        );
        Ok(Some((content, task.model_output.total_bytes, !task.state.is_running())))
    }

    /// 后台完成通知成功写入日志后提交已交付水位。
    pub(crate) fn confirm_delivered(&self, task_id: &str, end: u64, terminal: bool) {
        let mut delivered = self.delivered.lock().expect("bash output delivery lock");
        let position = delivered.entry(task_id.to_owned()).or_default();
        position.end = position.end.max(end);
        position.terminal |= terminal;
    }

    fn take_output(&self, task_id: &str) -> Result<ToolExecution, String> {
        let mut delivered = self.delivered.lock().expect("bash output delivery lock");
        let task = self.task(task_id)?;
        let position = delivered.entry(task_id.to_owned()).or_default();
        let result = task_result(&task, Some(position.end), true);
        position.end = task.model_output.total_bytes;
        position.terminal = !task.state.is_running();
        Ok(result)
    }

    /// 取消后台命令并等待回收、保存终态。
    pub async fn stop(self: &Arc<Self>, task_id: &str) -> Result<(), String> {
        self.stop_task(task_id).await.map(|_| ())
    }

    /// 取消后台命令并等待回收、保存终态；操作成功后返回模型可查看的命令结果。
    pub(crate) async fn tool_stop(self: &Arc<Self>, task_id: &str) -> ToolExecution {
        match self.stop_task(task_id).await {
            Ok(task) => task_result(&task, None, false),
            Err(error) => error_result(error),
        }
    }

    async fn stop_task(self: &Arc<Self>, task_id: &str) -> Result<BackgroundTask, String> {
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
        let manager = Arc::clone(self);
        let id = task_id.to_owned();
        tokio::task::spawn_blocking(move || manager.task(&id))
            .await
            .expect("bash task lookup completes while the runtime is running")
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
        output_directory: PathBuf,
        signal: CancellationToken,
        mut on_update: impl FnMut(String) -> F + Send,
    ) -> ToolExecution {
        if signal.is_cancelled() {
            return error_result(ABORTED_MESSAGE);
        }
        let mut process = match Process::spawn(&args, &cwd, &output_directory).await {
            Ok(process) => process,
            Err(error) => return error_result(error),
        };
        if args.yield_time_ms > 0 {
            let yield_at = tokio::time::sleep(Duration::from_millis(args.yield_time_ms));
            tokio::pin!(yield_at);
            loop {
                tokio::select! {
                    biased;
                    event = process.next(&signal) => match event {
                        ProcessEvent::Progress { display, .. } => on_update(display).await,
                        ProcessEvent::Finished(outcome) => return process.finish(outcome).await.0,
                    },
                    () = &mut yield_at => break,
                }
            }
        }
        if signal.is_cancelled() {
            return process.finish(Outcome::Stopped).await.0;
        }
        process.persist_output();
        let task = BackgroundTask {
            task_id: Uuid::now_v7().to_string(),
            command: args.command,
            description: args.description,
            output: process.display_output(),
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
        (self.on_update)(BashTaskUpdate::Updated(task));
        // 保存开始记录期间可能已经收到停止；这时等进程回收、最终状态保存后再返回。
        if signal.is_cancelled() || entry.cancel.is_cancelled() {
            entry.cancel.cancel();
            self.clone().run_background(entry, process).await;
            let manager = Arc::clone(self);
            let id = task_id.clone();
            let mut result = tokio::task::spawn_blocking(move || manager.take_output(&id))
                .await
                .expect("bash output read completes while the runtime is running")
                .unwrap_or_else(error_result);
            result.is_error = true;
            return result;
        }
        let manager = Arc::clone(self);
        let id = task_id.clone();
        let mut result = tokio::task::spawn_blocking(move || manager.take_output(&id))
            .await
            .expect("bash output read completes while the runtime is running")
            .expect("background command was registered");
        result.content.push_str("\n\nThe command continues in the background. Completion will deliver its status and new output automatically. Continue independent work; if nothing can proceed, give a brief status message and finish this response; you will resume when it finishes. Do not poll, sleep, or repeatedly read its log to wait. Use task_output only when current output helps you act now.");
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
        with_writer(&self.writer, |writer| {
            let entries = self.entries.lock().expect("bash task entries lock");
            writer
                .data()
                .entries()
                .iter()
                .rev()
                .filter_map(recorded_task)
                .find(|task| task.task_id == task_id)
                .map(|task| current_task(task, &entries, clone_task))
                .ok_or_else(|| format!("Unknown background task: {task_id}"))
        })
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
                // 先把完成通知放进输入队列，再标记命令结束，避免 `has_work` 在两者之间误判为空闲。
                if matches!(
                    task.state,
                    BackgroundTaskState::Exited { .. } | BackgroundTaskState::Failed { .. }
                ) {
                    self.inbox
                        .lock()
                        .expect("turn inbox lock poisoned")
                        .enqueue(crate::agent::QueuedInput::background(task.task_id.clone()));
                }
                *entry.task.lock().expect("bash task lock") = task.clone();
                let finished = completion.is_ok();
                entry.done.send_replace(Some(completion));
                if finished {
                    self.entries.lock().expect("bash task entries lock").remove(&task_id);
                }
                (self.on_update)(BashTaskUpdate::Updated(task));
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
