//! 工具批次执行：同一批次内的工具调用并发执行，preflight 拒绝项不进入
//! worker；`Started` 事件按模型给定 source order 先行发出，`Update`/`Ended`
//! 按实际完成顺序发出，返回值恒按 source order 排列，供持久化与 provider
//! 回放使用。单个调用失败不影响其余调用。
//!
//! 并发带来的唯一写冲突面是同一文件的 `edit`/`write` 互相交叠，因此批次内
//! 按文件键持有互斥锁：同文件串行、不同文件并行。批次之间本就串行，锁表
//! 只需活在一个批次内。

use std::collections::HashMap;
use std::path::Path;
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;

use singularity_core::CancellationToken;
use singularity_model::ModelToolCall;

use crate::agent::{AgentEvent, AgentEvents, emit};
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

/// worker 回传给主线程的事件。事件发布权只在主线程：`AgentEvents` 携带
/// `&mut dyn FnMut`，不可跨线程共享。
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

/// 需要互斥的目标文件键：只有 `edit`/`write` 的写入面可以被静态判定。
/// 只读工具无副作用；`bash` 可以改写任意路径，但无法从参数推出集合，
/// 因此与参考实现的按路径队列一样不加锁——它的正确性不由本层负责。
fn mutation_path(prepared: &PreparedTool) -> Option<&str> {
    match prepared {
        PreparedTool::Edit(args) => Some(&args.path),
        PreparedTool::Write(args) => Some(&args.path),
        _ => None,
    }
}

/// 文件锁键：相对路径按批次 cwd 取词法绝对形，统一分隔符，Windows 上再
/// 折叠大小写，使 `a/b.txt`、`.\a\b.txt`、`A\B.TXT` 命中同一把锁。词法而
/// 非 `canonicalize`，因为同批次另一线程可能正在创建该文件：触盘结果会随
/// 时序变化，键就不稳定。符号链接两侧仍可能取到不同键，属已知的保守缺口。
fn mutation_lock_key(cwd: &Path, path: &str) -> String {
    let joined = cwd.join(path);
    let absolute = std::path::absolute(&joined).unwrap_or(joined);
    let text = absolute.to_string_lossy().replace('\\', "/");
    if cfg!(windows) {
        text.to_lowercase()
    } else {
        text
    }
}

/// 取锁。中毒 = 该锁保护的不变量已被破坏 → fail-stop，与 `lock_writer`、
/// `lock_inbox` 同一纪律。正常路径下工具 panic 被 `run_worker` 的
/// `catch_unwind` 在持锁区间内就地接住，unwind 不穿过 guard，这两把锁实际
/// 不会中毒。
#[allow(clippy::expect_used)]
fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().expect("tool batch lock poisoned (fail-stop)")
}

/// 取得（必要时创建）某文件的锁句柄。锁表只活在一个批次内：批次之间本就
/// 串行，只有同一批次内的 worker 才会竞争它。
fn lock_for(locks: &Mutex<HashMap<String, Arc<Mutex<()>>>>, key: &str) -> Arc<Mutex<()>> {
    let mut map = lock_unpoisoned(locks);
    map.entry(key.to_string())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}

/// 一个 worker 线程的完整体：先按目标文件取锁（同文件互斥，持锁跨越整个
/// 工具执行），再以 `catch_unwind` 隔离 panic，最后把最终结果送回主线程。
/// panic 被就地转成模型可见失败，线程本身不会带着结果逃逸。
fn run_worker(
    registry: &ToolRegistrySnapshot,
    index: usize,
    prepared: PreparedTool,
    cwd: &Path,
    cancellation: &CancellationToken,
    locks: &Mutex<HashMap<String, Arc<Mutex<()>>>>,
    sender: Sender<WorkerEvent>,
) {
    let key = mutation_path(&prepared).map(|path| mutation_lock_key(cwd, path));
    let file_lock: Option<Arc<Mutex<()>>> = key.as_deref().map(|key| lock_for(locks, key));
    let execution = {
        let _file_guard: Option<MutexGuard<'_, ()>> =
            file_lock.as_ref().map(|lock| lock_unpoisoned(lock));
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut update = |text: &str| {
                let _ = sender.send(WorkerEvent::Update {
                    index,
                    text: text.to_string(),
                });
            };
            registry.execute_prepared(
                prepared,
                ExecuteContext {
                    cwd,
                    signal: cancellation,
                    on_update: Some(&mut update),
                },
            )
        }))
        .unwrap_or_else(|_| error_result("tool execution failed: tool execution panicked"))
    };
    let _ = sender.send(WorkerEvent::Ended { index, execution });
}

/// 并发执行一批工具调用。preflight 拒绝项在主线程直接收尾；其余按至多
/// [`MAX_PARALLEL_TOOL_WORKERS`] 的窗口并行执行，事件由主线程统一发布。
/// 返回向量与 `calls` 同长同序：调用方按 source order 落盘。
pub(crate) fn execute_tool_batch(
    registry: &ToolRegistrySnapshot,
    calls: &[PreparedToolCall],
    cwd: &Path,
    cancellation: &CancellationToken,
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

    let lock_table: Mutex<HashMap<String, Arc<Mutex<()>>>> = Mutex::new(HashMap::new());
    let locks = &lock_table;
    for window in runnable.chunks(MAX_PARALLEL_TOOL_WORKERS) {
        let (sender, receiver) = mpsc::channel::<WorkerEvent>();
        thread::scope(|scope| {
            for &index in window {
                let ToolPreflight::Ready(prepared) = &calls[index].prepared else {
                    continue;
                };
                let sender = sender.clone();
                let prepared = prepared.clone();
                scope.spawn(move || {
                    run_worker(registry, index, prepared, cwd, cancellation, locks, sender);
                });
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
