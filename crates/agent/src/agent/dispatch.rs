//! 最多同时运行八个工具，完成一个就保存并补入下一个；提问等待其他工具结束。

use super::{Agent, Result, TurnEvent};
use crate::message::tool_result_message;
use crate::session::lock_writer;
use crate::tools::{PreparedTool, ToolExecution, error_result};
use singularity_model::ModelToolCall;
use std::{sync::Arc, time::Instant};
use tokio::{sync::mpsc, task::JoinSet};
use tokio_util::sync::CancellationToken;

const MAX_PARALLEL_TOOLS: usize = 8;

impl Agent {
    pub(super) async fn dispatch_tools(
        &mut self,
        tool_calls: Vec<ModelToolCall>,
        assistant_result_entry_id: &str,
        length_truncated: bool,
        cancellation: &CancellationToken,
        on_event: &mut (dyn FnMut(TurnEvent) + Send),
    ) -> Result<()> {
        let cwd = lock_writer(&self.requests.session).cwd().to_path_buf();
        let mut pending = tool_calls
            .into_iter()
            .enumerate()
            .map(|(index, call)| {
                let tool = if length_truncated {
                    Err(error_result(
                        "tool execution failed: model output was truncated before the tool call completed",
                    ))
                } else {
                    self.requests.registry.preflight(&call.tool_name, &call.arguments)
                };
                (crate::session::tool_item_id(assistant_result_entry_id, index), call, tool)
            })
            .peekable();
        let mut workers = JoinSet::new();
        let (progress, mut updates) = mpsc::channel::<(String, String)>(32);
        let signal = cancellation.child_token();
        let mut save_error = None;
        loop {
            while save_error.is_none() && workers.len() < MAX_PARALLEL_TOOLS {
                let Some((_, _, next)) = pending.peek() else { break };
                if matches!(next, Ok(PreparedTool::Question(_))) && !workers.is_empty() {
                    break;
                }
                let (item_id, call, tool) = pending.next().expect("pending tool exists");
                on_event(TurnEvent::ItemUpdated {
                    item: singularity_protocol::HistoryItem::Tool {
                        id: item_id.clone(),
                        name: call.tool_name,
                        args: call.arguments,
                        result: None,
                        status: singularity_protocol::ToolStatus::Running,
                    },
                });
                let tool = if signal.is_cancelled() {
                    Err(error_result(crate::tools::ABORTED_MESSAGE))
                } else {
                    tool
                };
                if let Ok(PreparedTool::Question(args)) = tool {
                    let started = Instant::now();
                    let mut execution = self
                        .questions
                        .ask(
                            singularity_protocol::PendingQuestion {
                                item_id: item_id.clone(),
                                questions: args.questions,
                            },
                            &signal,
                            on_event,
                        )
                        .await;
                    execution.duration_ms = Some(singularity_core::duration_millis(started.elapsed()));
                    if let Err(error) =
                        self.finish_tool(item_id, call.tool_call_id, execution, on_event).await
                    {
                        save_error = Some(error);
                        signal.cancel();
                    }
                    continue;
                }
                let context = crate::tools::ToolContext {
                    cwd: cwd.clone(),
                    signal: signal.clone(),
                    bash_tasks: Arc::clone(&self.bash_tasks),
                };
                let progress = progress.clone();
                workers.spawn(async move {
                    let started = Instant::now();
                    let execution = match tool {
                        Err(execution) => execution,
                        Ok(tool) => {
                            let id = item_id.clone();
                            let mut execution = tool
                                .execute(context, move |text| {
                                    let progress = progress.clone();
                                    let id = id.clone();
                                    async move {
                                        let _ = progress.send((id, text)).await;
                                    }
                                })
                                .await;
                            execution.duration_ms =
                                Some(singularity_core::duration_millis(started.elapsed()));
                            execution
                        }
                    };
                    (item_id, call.tool_call_id, execution)
                });
            }
            if workers.is_empty() {
                break;
            }
            tokio::select! {
                Some((item_id, partial_result)) = updates.recv() => {
                    on_event(TurnEvent::ToolOutput { item_id, output: partial_result });
                }
                completed = workers.join_next() => {
                    let (item_id, call_id, execution) = completed.expect("workers are running")
                        .expect("tool worker completes while the runtime is running");
                    // 完成前发出的进度先交付，防止旧进度覆盖已完成结果。
                    while let Ok((item_id, partial_result)) = updates.try_recv() {
                        on_event(TurnEvent::ToolOutput { item_id, output: partial_result });
                    }
                    if save_error.is_none()
                        && let Err(error) = self.finish_tool(item_id, call_id, execution, on_event).await
                    {
                        save_error = Some(error);
                        signal.cancel();
                    }
                }
            }
        }
        // 写日志失败后取消并等待已启动的工具收尾，跳过还未启动的工具调用。
        save_error.map_or(Ok(()), Err)
    }

    async fn finish_tool(
        &self,
        item_id: String,
        call_id: String,
        execution: ToolExecution,
        on_event: &mut (dyn FnMut(TurnEvent) + Send),
    ) -> Result<()> {
        self.append_message(&item_id, tool_result_message(&call_id, &execution), &execution.images)
            .await?;
        on_event(TurnEvent::ToolFinished {
            item_id,
            result: singularity_protocol::ToolResult {
                output: execution.content,
                images: execution.images.into_iter().map(|image| image.attachment).collect(),
                is_error: execution.is_error,
                duration_ms: execution.duration_ms,
                read_source: execution.read_source,
            },
        });
        Ok(())
    }
}
