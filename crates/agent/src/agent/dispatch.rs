//! 全批工具均允许并行时，每组最多运行八个；包含独占工具时整批顺序执行。
//! 进度实时发布，结果按模型调用顺序保存，随后开始下一组。

use super::{Agent, AgentEvent, Result};
use crate::message::tool_result_message;
use crate::session::lock_writer;
use crate::tools::{PreparedTool, ToolExecution, error_result};
use singularity_model::ModelToolCall;
use std::{sync::Arc, time::Instant};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

const MAX_PARALLEL_TOOLS: usize = 8;

enum WorkerEvent {
    Update { item_id: String, text: String },
    Ended { index: usize, execution: ToolExecution },
}

impl Agent {
    pub(super) async fn dispatch_tools(
        &mut self,
        tool_calls: Vec<ModelToolCall>,
        assistant_result_entry_id: &str,
        length_truncated: bool,
        cancellation: &CancellationToken,
        on_event: &mut (dyn FnMut(AgentEvent) + Send),
    ) -> Result<()> {
        let cwd = lock_writer(&self.session).cwd().to_path_buf();
        let prepared: Vec<_> = tool_calls
            .into_iter()
            .enumerate()
            .map(|(index, call)| {
                let prepared = if length_truncated {
                    Err(error_result(
                        "tool execution failed: model output was truncated before the tool call completed",
                    ))
                } else {
                    self.registry.preflight(&call.tool_name, &call.arguments)
                };
                (crate::session::tool_item_id(assistant_result_entry_id, index), call, prepared)
            })
            .collect();
        let parallelism = if prepared
            .iter()
            .all(|(_, _, tool)| tool.as_ref().map_or(true, PreparedTool::supports_parallel))
        {
            MAX_PARALLEL_TOOLS
        } else {
            1
        };
        let mut pending = prepared.into_iter();
        loop {
            let batch: Vec<_> = pending.by_ref().take(parallelism).collect();
            if batch.is_empty() {
                break;
            }
            let (sender, mut receiver) = mpsc::channel(32);
            let mut results: Vec<Option<ToolExecution>> = (0..batch.len()).map(|_| None).collect();
            let mut identities = Vec::with_capacity(batch.len());
            for (index, (item_id, call, prepared)) in batch.into_iter().enumerate() {
                identities.push((item_id.clone(), call.tool_call_id));
                on_event(AgentEvent::ToolExecutionStarted {
                    item_id: item_id.clone(),
                    tool_name: call.tool_name,
                    arguments: call.arguments,
                });
                let prepared = if cancellation.is_cancelled() {
                    Err(error_result(crate::tools::ABORTED_MESSAGE))
                } else {
                    prepared
                };
                match prepared {
                    Err(execution) => results[index] = Some(execution),
                    Ok(PreparedTool::Question(args)) => {
                        let started = Instant::now();
                        let mut execution = self
                            .questions
                            .ask(
                                singularity_protocol::PendingQuestion { item_id, questions: args.questions },
                                cancellation,
                                on_event,
                            )
                            .await;
                        execution.duration_ms = Some(singularity_core::duration_millis(started.elapsed()));
                        results[index] = Some(execution);
                    }
                    Ok(tool) => {
                        let sender = sender.clone();
                        let context = crate::tools::ToolContext {
                            cwd: cwd.clone(),
                            signal: cancellation.clone(),
                            bash_tasks: Arc::clone(&self.execution.bash_tasks),
                            turn_id: self.execution.turn_id.clone(),
                            item_id: item_id.clone(),
                        };
                        tokio::spawn(async move {
                            let started = Instant::now();
                            let progress = sender.clone();
                            let mut execution = tool
                                .execute(context, move |text| {
                                    let progress = progress.clone();
                                    let item_id = item_id.clone();
                                    async move {
                                        let _ = progress.send(WorkerEvent::Update { item_id, text }).await;
                                    }
                                })
                                .await;
                            execution.duration_ms =
                                Some(singularity_core::duration_millis(started.elapsed()));
                            let _ = sender.send(WorkerEvent::Ended { index, execution }).await;
                        });
                    }
                }
            }
            drop(sender);
            while let Some(event) = receiver.recv().await {
                match event {
                    WorkerEvent::Update { item_id, text } => {
                        on_event(AgentEvent::ToolExecutionUpdate { item_id, partial_result: text })
                    }
                    WorkerEvent::Ended { index, execution } => results[index] = Some(execution),
                }
            }
            for ((item_id, tool_call_id), execution) in identities.into_iter().zip(results) {
                let execution = execution.expect("each completed worker returns its result");
                self.append_message(
                    &item_id,
                    tool_result_message(&tool_call_id, &execution),
                    &execution.images,
                )
                .await?;
                on_event(AgentEvent::ToolExecutionEnded { item_id, execution });
            }
        }
        Ok(())
    }
}
