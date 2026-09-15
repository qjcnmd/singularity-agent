//! 工具执行与结果提交边界。相邻只读调用可并发执行；
//! 变更与命令按源码顺序作为屏障串行执行。
//! 结果在完成时即提交，早于完成事件的发布。提交
//! 失败即停止新的派发。

use std::path::Path;
use std::sync::mpsc::{self, SyncSender};
use std::thread;

use singularity_core::CancellationToken;
use singularity_model::ModelToolCall;

use crate::agent::AgentEvent;
use crate::tools::{ExecuteContext, PreparedTool, ToolExecution, error_result};

const MAX_PARALLEL_TOOL_WORKERS: usize = 8;
// 背压限制在途输出，即使存储或 UI 变慢也是如此。
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
    },
}

fn run_worker(
    index: usize,
    prepared: &PreparedTool,
    cwd: &Path,
    cancellation: &CancellationToken,
    sender: SyncSender<WorkerEvent>,
) {
    let started = std::time::Instant::now();
    let mut execution = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut update = |text: &str| {
            let _ = sender.send(WorkerEvent::Update {
                index,
                text: text.to_string(),
            });
        };
        prepared.execute(ExecuteContext {
            cwd,
            signal: cancellation,
            on_update: Some(&mut update),
        })
    }))
    .unwrap_or_else(|_| error_result("tool execution failed: tool execution panicked"));
    execution.duration_ms = Some(singularity_core::duration_millis(started.elapsed()));
    let _ = sender.send(WorkerEvent::Ended { index, execution });
}

/// 每个结果先于其完成事件提交。提交失败会抑制
/// 该完成事件并阻止后续派发。已在运行的读取 worker
/// 会被排空并 join；工具副作用一律不重试。
pub(crate) fn execute_tool_batch<E>(
    calls: &[PreparedToolCall],
    cwd: &Path,
    cancellation: &CancellationToken,
    on_event: &mut dyn FnMut(AgentEvent),
    commit: &mut impl FnMut(&PreparedToolCall, &ToolExecution) -> Result<(), E>,
) -> Result<(), E> {
    let mut cursor = 0;
    while cursor < calls.len() {
        let parallel = |item: &PreparedToolCall| matches!(&item.prepared, Ok(tool) if tool.supports_parallel());
        let count = if parallel(&calls[cursor]) {
            calls[cursor..]
                .iter()
                .take(MAX_PARALLEL_TOOL_WORKERS)
                .take_while(|item| parallel(item))
                .count()
        } else {
            1
        };
        let end = cursor + count;
        // 可运行列表直接携带已解析的 prepared 借用：取消与参数失败在此就地提交
        // 结果，其余调用不再保留“已筛过又重判”的第二次匹配。
        let mut runnable: Vec<(usize, &PreparedTool)> = Vec::new();
        for (index, item) in calls.iter().enumerate().take(end).skip(cursor) {
            on_event(AgentEvent::ToolExecutionStarted {
                item_id: item.result_entry_id.clone(),
                tool_name: item.call.tool_name.clone(),
                arguments: item.call.arguments.clone(),
            });
            let dispatched = if cancellation.is_cancelled() {
                Err(error_result(super::registry::ABORTED_MESSAGE))
            } else {
                item.prepared.as_ref().map_err(ToolExecution::clone)
            };
            match dispatched {
                Ok(prepared) => runnable.push((index, prepared)),
                Err(execution) => {
                    commit(item, &execution)?;
                    emit_completion(on_event, item, &execution);
                }
            }
        }
        let result = thread::scope(|scope| {
            // 若消费者回调 panic，在 scope join 前先丢弃 receiver。
            let (sender, receiver) = mpsc::sync_channel(OUTPUT_QUEUE_CAPACITY);
            for (index, prepared) in runnable {
                let worker_sender = sender.clone();
                if let Err(error) = thread::Builder::new().spawn_scoped(scope, move || {
                    run_worker(index, prepared, cwd, cancellation, worker_sender);
                }) {
                    let _ = sender.send(WorkerEvent::Ended {
                        index,
                        execution: error_result(format!("failed to start tool worker: {error}")),
                    });
                }
            }
            drop(sender);
            let mut failure = None;
            while let Ok(event) = receiver.recv() {
                if failure.is_some() {
                    continue;
                }
                match event {
                    WorkerEvent::Update { index, text } => {
                        on_event(AgentEvent::ToolExecutionUpdate {
                            item_id: calls[index].result_entry_id.clone(),
                            partial_result: text,
                        })
                    }
                    WorkerEvent::Ended { index, execution } => {
                        if let Err(error) = commit(&calls[index], &execution) {
                            failure = Some(error);
                            continue;
                        }
                        emit_completion(on_event, &calls[index], &execution);
                    }
                }
            }
            failure.map_or(Ok(()), Err)
        });
        result?;
        cursor = end;
    }
    Ok(())
}

fn emit_completion(
    on_event: &mut dyn FnMut(AgentEvent),
    item: &PreparedToolCall,
    execution: &ToolExecution,
) {
    on_event(AgentEvent::ToolExecutionEnded {
        item_id: item.result_entry_id.clone(),
        execution: execution.clone(),
    });
}
