//! Tool execution and result commit boundary. Adjacent read-only calls may run
//! concurrently; mutations and commands run in source order as barriers.
//! Results commit as they finish, before completion is published. A commit
//! failure stops new dispatch.

use std::path::Path;
use std::sync::mpsc::{self, SyncSender};
use std::thread;

use singularity_core::CancellationToken;
use singularity_model::ModelToolCall;

use crate::agent::{AgentEvent, AgentEvents, emit};
use crate::tools::{
    ExecuteContext, PreparedTool, ToolExecution, ToolPreflight, ToolRegistrySnapshot, error_result,
};

const MAX_PARALLEL_TOOL_WORKERS: usize = 8;
// Backpressure bounds in-flight output even when storage or UI is slow.
const OUTPUT_QUEUE_CAPACITY: usize = 32;

pub(crate) struct PreparedToolCall {
    pub call: ModelToolCall,
    pub prepared: ToolPreflight,
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

struct BatchScope<'a> {
    registry: &'a ToolRegistrySnapshot,
    cwd: &'a Path,
    cancellation: &'a CancellationToken,
}

fn run_worker(
    batch: &BatchScope<'_>,
    index: usize,
    prepared: PreparedTool,
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
        batch.registry.execute_prepared(
            prepared,
            ExecuteContext {
                cwd: batch.cwd,
                signal: batch.cancellation,
                on_update: Some(&mut update),
            },
        )
    }))
    .unwrap_or_else(|_| error_result("tool execution failed: tool execution panicked"));
    execution.duration_ms = Some(singularity_model::duration_millis(started.elapsed()));
    let _ = sender.send(WorkerEvent::Ended { index, execution });
}

/// Commit each result before its completion event. A commit failure suppresses
/// that completion and prevents later dispatch. Already-running read workers
/// are drained and joined; no tool side effect is retried.
pub(crate) fn execute_tool_batch<E>(
    registry: &ToolRegistrySnapshot,
    calls: &[PreparedToolCall],
    cwd: &Path,
    cancellation: &CancellationToken,
    events: &mut AgentEvents<'_>,
    commit: &mut impl FnMut(&PreparedToolCall, &ToolExecution) -> Result<(), E>,
) -> Result<(), E> {
    let batch = BatchScope {
        registry,
        cwd,
        cancellation,
    };
    let mut cursor = 0;
    while cursor < calls.len() {
        let parallel = |item: &PreparedToolCall| matches!(&item.prepared, ToolPreflight::Ready(tool) if tool.supports_parallel());
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
        let mut runnable = Vec::new();
        for (index, item) in calls.iter().enumerate().take(end).skip(cursor) {
            emit(
                events,
                AgentEvent::ToolExecutionStarted {
                    item_id: item.result_entry_id.clone(),
                    tool_name: item.call.tool_name.clone(),
                    arguments: item.call.arguments.clone(),
                },
            );
            let skipped = if cancellation.is_cancelled() {
                Some(error_result(super::registry::ABORTED_MESSAGE))
            } else if let ToolPreflight::Rejected(result) = &item.prepared {
                Some(result.clone())
            } else {
                None
            };
            if let Some(execution) = skipped {
                commit(item, &execution)?;
                emit_completion(events, item, &execution);
            } else {
                runnable.push(index);
            }
        }
        let result = thread::scope(|scope| {
            // Drop the receiver before scope joins if a consumer callback panics.
            let (sender, receiver) = mpsc::sync_channel(OUTPUT_QUEUE_CAPACITY);
            for index in runnable {
                let ToolPreflight::Ready(prepared) = &calls[index].prepared else {
                    continue;
                };
                let worker_sender = sender.clone();
                let prepared = prepared.clone();
                let shared = &batch;
                if let Err(error) = thread::Builder::new().spawn_scoped(scope, move || {
                    run_worker(shared, index, prepared, worker_sender);
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
                    WorkerEvent::Update { index, text } => emit(
                        events,
                        AgentEvent::ToolExecutionUpdate {
                            item_id: calls[index].result_entry_id.clone(),
                            tool_name: calls[index].call.tool_name.clone(),
                            arguments: calls[index].call.arguments.clone(),
                            partial_result: text,
                        },
                    ),
                    WorkerEvent::Ended { index, execution } => {
                        if let Err(error) = commit(&calls[index], &execution) {
                            failure = Some(error);
                            continue;
                        }
                        emit_completion(events, &calls[index], &execution);
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
    events: &mut AgentEvents<'_>,
    item: &PreparedToolCall,
    execution: &ToolExecution,
) {
    emit(
        events,
        AgentEvent::ToolExecutionEnded {
            item_id: item.result_entry_id.clone(),
            tool_name: item.call.tool_name.clone(),
            execution: execution.clone(),
        },
    );
}
