//! 按模型给出的顺序准入工具。只读调用共享准入锁；命令和文件修改独占它。
//! 结果一完成就落盘，随后才发布完成事件。失败后排空已启动的工具，不派发后续调用。

use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use futures_util::FutureExt;
use singularity_model::ModelToolCall;
use tokio::sync::{RwLock, mpsc};
use tokio_util::sync::CancellationToken;

use crate::agent::AgentEvent;
use crate::tools::{PreparedTool, ToolExecution, error_result};

// 只读工具仍在线程池执行文件 I/O，限制同时运行的数量以控制资源竞争。
const MAX_PARALLEL_TOOL_WORKERS: u32 = 8;
const OUTPUT_QUEUE_CAPACITY: usize = 32;

pub(crate) struct PreparedToolCall {
    pub call: ModelToolCall,
    pub prepared: Result<PreparedTool, ToolExecution>,
    pub result_entry_id: String,
}

enum WorkerEvent {
    Update {
        index: usize,
        text: String,
    },
    Ended {
        index: usize,
        execution: ToolExecution,
        _admission: Admission,
    },
    HostFailure {
        message: String,
        _admission: Admission,
    },
}

#[derive(Debug)]
pub(crate) enum ToolDispatchError<E> {
    Commit(E),
    HostFailure(String),
}

pub(crate) trait ToolCommit: Send {
    type Error;

    fn commit(
        &mut self,
        item: &PreparedToolCall,
        execution: &ToolExecution,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;
}

/// 准入在派发者中按 source order 等待，确保后面的独占调用不会越过前面的读取。
/// 等待锁期间继续消费结果和进度，避免背压阻止已运行工具释放锁。
pub(crate) async fn dispatch_tools<E>(
    calls: &[PreparedToolCall],
    cwd: &Path,
    cancellation: &CancellationToken,
    on_event: &mut (dyn FnMut(AgentEvent) + Send),
    commit: &mut impl ToolCommit<Error = E>,
) -> Result<(), ToolDispatchError<E>> {
    let gate = Arc::new(RwLock::with_max_readers((), MAX_PARALLEL_TOOL_WORKERS));
    let (sender, mut receiver) = mpsc::channel(OUTPUT_QUEUE_CAPACITY);
    let mut active = 0usize;
    let mut failure = None;

    let dispatched = AssertUnwindSafe(async {
        // 原始 sender 归本轮派发所有；结束派发后先关闭它，才能识别任务 panic
        // 后未交还 terminal event 的情况。
        let sender = sender;
        for (index, item) in calls.iter().enumerate() {
            if failure.is_some() {
                break;
            }
            let parallel = matches!(&item.prepared, Ok(prepared) if prepared.supports_parallel());
            let admission = async {
                if parallel {
                    Admission::Read {
                        _guard: Arc::clone(&gate).read_owned().await,
                    }
                } else {
                    Admission::Write {
                        _guard: Arc::clone(&gate).write_owned().await,
                    }
                }
            };
            tokio::pin!(admission);
            let guard = loop {
                tokio::select! {
                    guard = &mut admission => break Some(guard),
                    event = receiver.recv(), if active > 0 => {
                        match event {
                            Some(event) => {
                                let ended = matches!(event, WorkerEvent::Ended { .. } | WorkerEvent::HostFailure { .. });
                                active -= usize::from(ended);
                                if let Err(error) = process_event(event, calls, on_event, commit).await {
                                    failure = Some(error);
                                    break None;
                                }
                            }
                            None => {
                                failure = Some(ToolDispatchError::HostFailure("tool task ended without a result".into()));
                                break None;
                            }
                        }
                    }
                }
            };
            let Some(guard) = guard else { break };
            on_event(AgentEvent::ToolExecutionStarted {
                item_id: item.result_entry_id.clone(),
                tool_name: item.call.tool_name.clone(),
                arguments: item.call.arguments.clone(),
            });
            let prepared = if cancellation.is_cancelled() {
                Err(error_result(super::registry::ABORTED_MESSAGE))
            } else {
                item.prepared.clone()
            };
            let prepared = match prepared {
                Ok(prepared) => prepared,
                Err(execution) => {
                    if let Err(error) = commit.commit(item, &execution).await {
                        failure = Some(ToolDispatchError::Commit(error));
                        break;
                    }
                    emit_completion(on_event, item, execution);
                    continue;
                }
            };
            let sender = sender.clone();
            let cwd = cwd.to_path_buf();
            let signal = cancellation.clone();
            active += 1;
            tokio::spawn(async move {
                let started = Instant::now();
                let updates = sender.clone();
                let outcome = prepared
                    .execute(cwd, signal, move |text| {
                        let _ = updates.blocking_send(WorkerEvent::Update { index, text });
                    })
                    .await;
                let event = match outcome {
                    Ok(mut execution) => {
                        execution.duration_ms =
                            Some(singularity_core::duration_millis(started.elapsed()));
                        WorkerEvent::Ended {
                            index,
                            execution,
                            _admission: guard,
                        }
                    }
                    Err(error) => WorkerEvent::HostFailure {
                        message: format!("tool execution failed: tool task failed: {error}"),
                        _admission: guard,
                    },
                };
                let _ = sender.send(event).await;
            });
        }
        drop(sender);
        while active > 0 {
            let Some(event) = receiver.recv().await else {
                failure.get_or_insert_with(|| {
                    ToolDispatchError::HostFailure("tool task ended without a result".into())
                });
                break;
            };
            let ended = matches!(
                event,
                WorkerEvent::Ended { .. } | WorkerEvent::HostFailure { .. }
            );
            if failure.is_none()
                && let Err(error) = process_event(event, calls, on_event, commit).await
            {
                failure = Some(error);
            }
            active -= usize::from(ended);
        }
        failure.map_or(Ok(()), Err)
    })
    .catch_unwind()
    .await;
    match dispatched {
        Ok(result) => result,
        Err(panic) => {
            // 宿主回调或持久化过程 panic 时，仍等已启动工具交还结果和准入锁。
            // 阻塞工具不能依赖 task abort 收尾，随后交由外层的宿主故障路径结算。
            while active > 0 {
                let Some(event) = receiver.recv().await else {
                    break;
                };
                if matches!(
                    event,
                    WorkerEvent::Ended { .. } | WorkerEvent::HostFailure { .. }
                ) {
                    active -= 1;
                }
            }
            std::panic::resume_unwind(panic)
        }
    }
}

enum Admission {
    Read {
        _guard: tokio::sync::OwnedRwLockReadGuard<()>,
    },
    Write {
        _guard: tokio::sync::OwnedRwLockWriteGuard<()>,
    },
}

async fn process_event<E>(
    event: WorkerEvent,
    calls: &[PreparedToolCall],
    on_event: &mut (dyn FnMut(AgentEvent) + Send),
    commit: &mut impl ToolCommit<Error = E>,
) -> Result<(), ToolDispatchError<E>> {
    match event {
        WorkerEvent::Update { index, text } => on_event(AgentEvent::ToolExecutionUpdate {
            item_id: calls[index].result_entry_id.clone(),
            partial_result: text,
        }),
        WorkerEvent::Ended {
            index,
            execution,
            _admission: _guard,
        } => {
            let item = &calls[index];
            commit
                .commit(item, &execution)
                .await
                .map_err(ToolDispatchError::Commit)?;
            emit_completion(on_event, item, execution);
        }
        WorkerEvent::HostFailure {
            message,
            _admission: _guard,
        } => return Err(ToolDispatchError::HostFailure(message)),
    }
    Ok(())
}

fn emit_completion(
    on_event: &mut (dyn FnMut(AgentEvent) + Send),
    item: &PreparedToolCall,
    execution: ToolExecution,
) {
    on_event(AgentEvent::ToolExecutionEnded {
        item_id: item.result_entry_id.clone(),
        execution,
    });
}
