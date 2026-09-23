//! 工具执行与结果提交的边界。相邻的只读调用可以并发执行；变更类调用和命令按源码顺序
//! 充当屏障，串行执行。结果在完成时就提交，早于完成事件发布；提交一旦失败就停止派发。

use std::path::Path;
use std::sync::mpsc::{self, SyncSender};
use std::thread;

use singularity_model::ModelToolCall;
use tokio_util::sync::CancellationToken;

use crate::agent::AgentEvent;
use crate::tools::{ExecuteContext, PreparedTool, ToolExecution, error_result};

const MAX_PARALLEL_TOOL_WORKERS: usize = 8;
// 用背压限制在途的输出量，即使存储或 UI 变慢也不会堆积。
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
    /// worker 没能产出可提交的结果，原因是宿主故障（panic 或 worker 创建失败）。
    /// 这不属于工具的业务失败，不能伪装成 ToolExecution 交给模型继续尝试。
    HostFailure {
        message: String,
    },
}

/// 工具批次的两类失败出口。提交失败仍由调用方自己的错误类型表达；宿主故障是
/// 另一类事实，调用方据此停止整条执行链，而不是继续派发或交给模型。
#[derive(Debug)]
pub(crate) enum ToolBatchError<E> {
    /// 结果落盘失败；后续派发和完成事件一并停止。
    Commit(E),
    /// worker panic 或 worker 创建失败；原因保留在消息里。
    HostFailure(String),
}

fn run_worker(
    index: usize,
    prepared: &PreparedTool,
    cwd: &Path,
    cancellation: &CancellationToken,
    sender: SyncSender<WorkerEvent>,
) {
    let started = std::time::Instant::now();
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut update = |text: String| {
            let _ = sender.send(WorkerEvent::Update { index, text });
        };
        prepared.execute(ExecuteContext {
            cwd,
            signal: cancellation,
            on_update: &mut update,
        })
    }));
    match outcome {
        Ok(mut execution) => {
            execution.duration_ms = Some(singularity_core::duration_millis(started.elapsed()));
            let _ = sender.send(WorkerEvent::Ended { index, execution });
        }
        // panic 不是业务失败：不生成 ToolExecution，也不让后面的派发继续。
        Err(payload) => {
            let _ = sender.send(WorkerEvent::HostFailure {
                message: format!(
                    "tool execution failed: tool worker panicked: {}",
                    singularity_core::panic_message(payload.as_ref())
                ),
            });
        }
    }
}

/// 每个结果都先提交，再发它的完成事件。提交失败会压掉那条完成事件，并阻止后续
/// 派发。已经在运行的读取 worker 会被排空并 join；工具的副作用一律不重试。
pub(crate) fn execute_tool_batch<E>(
    calls: &[PreparedToolCall],
    cwd: &Path,
    cancellation: &CancellationToken,
    on_event: &mut dyn FnMut(AgentEvent),
    commit: &mut impl FnMut(&PreparedToolCall, &ToolExecution) -> Result<(), E>,
) -> Result<(), ToolBatchError<E>> {
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
        // 可运行列表直接借用已解析好的 prepared：取消与参数失败在这里就地提交结果，
        // 其余调用不必再保留一份「筛过又要重判」的第二次匹配。
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
                    commit(item, &execution).map_err(ToolBatchError::Commit)?;
                    emit_completion(on_event, item, execution);
                }
            }
        }
        let result = thread::scope(|scope| {
            // 如果消费者回调 panic，就在 scope join 之前先丢掉 receiver。
            let (sender, receiver) = mpsc::sync_channel(OUTPUT_QUEUE_CAPACITY);
            for (index, prepared) in runnable {
                let worker_sender = sender.clone();
                if let Err(error) = thread::Builder::new().spawn_scoped(scope, move || {
                    run_worker(index, prepared, cwd, cancellation, worker_sender);
                }) {
                    // 创建 worker 失败属于宿主资源故障，不是能交给模型重试的工具失败。
                    let _ = sender.send(WorkerEvent::HostFailure {
                        message: format!(
                            "tool execution failed: cannot start tool worker: {error}"
                        ),
                    });
                }
            }
            drop(sender);
            let mut failure: Option<ToolBatchError<E>> = None;
            while let Ok(event) = receiver.recv() {
                // 已经失败：后续事件只排空，不再提交或发布完成事件。
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
                            failure = Some(ToolBatchError::Commit(error));
                            continue;
                        }
                        emit_completion(on_event, &calls[index], execution);
                    }
                    WorkerEvent::HostFailure { message } => {
                        failure = Some(ToolBatchError::HostFailure(message));
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

/// 落盘提交借用结束之后，同一份 owned 结果直接移进完成事件：调用方不再保留
/// 第二份副本，事件里的输出和 diff 就是刚提交的那一份。
fn emit_completion(
    on_event: &mut dyn FnMut(AgentEvent),
    item: &PreparedToolCall,
    execution: ToolExecution,
) {
    on_event(AgentEvent::ToolExecutionEnded {
        item_id: item.result_entry_id.clone(),
        execution,
    });
}
