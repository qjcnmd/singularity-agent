//! 工具批次执行：同一批次内的工具调用并发执行，preflight 拒绝项不进入
//! worker；Started 事件按模型给定 source order 先行发出，Update/Ended
//! 按实际完成顺序发出，返回值恒按 source order 排列，供持久化与 provider
//! 回放使用。单个调用失败不影响其余调用。
//!
//! 文件修改互斥由工具执行入口拥有，覆盖本进程内全部任务。

use std::path::Path;
use std::sync::mpsc::{self, Sender};
use std::thread;

use singularity_core::CancellationToken;
use singularity_model::ModelToolCall;

use crate::agent::{AgentEvent, AgentEvents, emit};
use crate::tools::observe::ObservedFiles;
use crate::tools::{
    ExecuteContext, PreparedTool, ToolExecution, ToolPreflight, ToolRegistrySnapshot, error_result,
};

/// 单批同时执行的 worker 上限。工具执行是阻塞式 OS 线程（bash 还会派生
/// 子进程），无上限时模型一次返回大量调用会不受控地创建线程；窗口之间
/// 顺序推进，窗口之内全部并行。
const MAX_PARALLEL_TOOL_WORKERS: usize = 8;

/// 一次模型工具调用及其 preflight 判定与预分配的结果条目 id。
pub(crate) struct PreparedToolCall {
    pub call: ModelToolCall,
    pub prepared: ToolPreflight,
    pub result_entry_id: String,
}

/// worker 回传给主线程的事件。事件发布权只在主线程：AgentEvents 携带
/// &mut dyn FnMut，不可跨线程共享。
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

/// 一个批次内所有 worker 共享的执行环境：注册表快照、工作区、中断信号、
/// 会话观察表。
struct BatchScope<'a> {
    registry: &'a ToolRegistrySnapshot,
    cwd: &'a Path,
    cancellation: &'a CancellationToken,
    observed: &'a ObservedFiles,
}

/// 一个 worker 线程的完整体：以 catch_unwind 隔离工具 panic，最后把结果送回主线程。
/// panic 被就地转成模型可见失败，线程本身不会带着结果逃逸。
fn run_worker(
    batch: &BatchScope<'_>,
    index: usize,
    prepared: PreparedTool,
    sender: Sender<WorkerEvent>,
) {
    let started = std::time::Instant::now();
    let mut execution = {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut update = |text: &str| {
                let _ = sender.send(WorkerEvent::Update {
                    index,
                    text: text.to_string(),
                });
            };
            batch.registry.execute_prepared(
                prepared,
                ExecuteContext {
                    cwd: batch.cwd,
                    signal: batch.cancellation,
                    on_update: Some(&mut update),
                    observed: batch.observed,
                },
            )
        }))
        .unwrap_or_else(|_| error_result("tool execution failed: tool execution panicked"))
    };
    execution.duration_ms = Some(singularity_model::duration_millis(started.elapsed()));
    let _ = sender.send(WorkerEvent::Ended { index, execution });
}

/// 并发执行一批工具调用。preflight 拒绝项在主线程直接收尾；其余按至多
/// MAX_PARALLEL_TOOL_WORKERS 的窗口并行执行，事件由主线程统一发布。
/// 返回向量与 calls 同长同序：调用方按 source order 落盘。
pub(crate) fn execute_tool_batch(
    registry: &ToolRegistrySnapshot,
    calls: &[PreparedToolCall],
    cwd: &Path,
    cancellation: &CancellationToken,
    observed: &ObservedFiles,
    events: &mut AgentEvents<'_>,
) -> Vec<ToolExecution> {
    let mut settled = vec![None; calls.len()];
    let mut runnable: Vec<usize> = Vec::with_capacity(calls.len());
    for (index, item) in calls.iter().enumerate() {
        emit(
            events,
            AgentEvent::ToolExecutionStarted {
                tool_name: item.call.tool_name.clone(),
                tool_call_id: item.call.tool_call_id.clone(),
                arguments: item.call.arguments.clone(),
            },
        );
        match &item.prepared {
            ToolPreflight::Rejected(execution) => {
                emit(
                    events,
                    AgentEvent::ToolExecutionEnded {
                        tool_name: item.call.tool_name.clone(),
                        tool_call_id: item.call.tool_call_id.clone(),
                        execution: execution.clone(),
                    },
                );
                settled[index] = Some(execution.clone());
            }
            ToolPreflight::Ready(_) => runnable.push(index),
        }
    }

    let batch = BatchScope {
        registry,
        cwd,
        cancellation,
        observed,
    };
    // worker 只需共享环境的引用：move 闭包复制的是这个引用，不是结构本身。
    let shared = &batch;
    for window in runnable.chunks(MAX_PARALLEL_TOOL_WORKERS) {
        let (sender, receiver) = mpsc::channel::<WorkerEvent>();
        thread::scope(|scope| {
            for &index in window {
                let ToolPreflight::Ready(prepared) = &calls[index].prepared else {
                    continue;
                };
                let sender = sender.clone();
                let prepared = prepared.clone();
                scope.spawn(move || run_worker(shared, index, prepared, sender));
            }
            drop(sender);
            while let Ok(event) = receiver.recv() {
                match event {
                    WorkerEvent::Update { index, text } => emit(
                        events,
                        AgentEvent::ToolExecutionUpdate {
                            tool_name: calls[index].call.tool_name.clone(),
                            tool_call_id: calls[index].call.tool_call_id.clone(),
                            arguments: calls[index].call.arguments.clone(),
                            partial_result: text,
                        },
                    ),
                    WorkerEvent::Ended { index, execution } => {
                        emit(
                            events,
                            AgentEvent::ToolExecutionEnded {
                                tool_name: calls[index].call.tool_name.clone(),
                                tool_call_id: calls[index].call.tool_call_id.clone(),
                                execution: execution.clone(),
                            },
                        );
                        settled[index] = Some(execution);
                    }
                }
            }
        });
    }

    // 每个 Started 恰有一个 Ended：worker 若在送回结果前终止（线程创建失败等
    // 极端情形），这里补一条模型可见失败并补发 Ended，不留悬空事件。
    let mut results = Vec::with_capacity(calls.len());
    for (item, execution) in calls.iter().zip(settled) {
        let execution = match execution {
            Some(execution) => execution,
            None => {
                let execution = error_result(
                    "tool execution failed: tool worker terminated before reporting a result",
                );
                emit(
                    events,
                    AgentEvent::ToolExecutionEnded {
                        tool_name: item.call.tool_name.clone(),
                        tool_call_id: item.call.tool_call_id.clone(),
                        execution: execution.clone(),
                    },
                );
                execution
            }
        };
        results.push(execution);
    }
    results
}
