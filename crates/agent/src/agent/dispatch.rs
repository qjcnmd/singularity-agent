//! 按模型给出的顺序准入工具。只读调用各占一个执行位；独占工具占用全部执行位。
//! 结果一完成就落盘，随后才发布完成事件。失败后排空已启动的工具，不派发后续调用。

use std::time::Instant;

use singularity_model::ModelToolCall;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::{Agent, AgentEvent, Result};
use crate::message::tool_result_message;
use crate::session::lock_writer;
use crate::tools::{PreparedTool, ToolExecution, error_result};

// 只读工具仍在线程池执行文件 I/O，限制同时运行的数量以控制资源竞争。
const MAX_PARALLEL_TOOL_WORKERS: usize = 8;
const OUTPUT_QUEUE_CAPACITY: usize = 32;

enum WorkerEvent {
    Update {
        item_id: String,
        text: String,
    },
    Ended {
        item_id: String,
        tool_call_id: String,
        execution: ToolExecution,
        occupied_slots: usize,
    },
}

impl Agent {
    /// 准入在派发者中按 source order 等待，后面的独占调用不会越过前面的读取。
    /// 派发者独占准入计数，等待空位时直接消费结果和进度；完成结果提交后才归还空位。
    pub(super) async fn dispatch_tools(
        &mut self,
        tool_calls: Vec<ModelToolCall>,
        assistant_result_entry_id: &str,
        length_truncated: bool,
        cancellation: &CancellationToken,
        on_event: &mut (dyn FnMut(AgentEvent) + Send),
    ) -> Result<()> {
        // 只为读 cwd 短暂持有会话写者锁，不跨工具执行持锁，否则会阻塞控制接受与
        // 终态落盘（工具 worker 与控制面共用同一写者）。
        let cwd = lock_writer(&self.session).cwd().to_path_buf();
        let (sender, mut receiver) = mpsc::channel(OUTPUT_QUEUE_CAPACITY);
        let mut occupied_slots = 0;
        let mut failure = None;

        for (index, call) in tool_calls.into_iter().enumerate() {
            let item_id = crate::session::tool_item_id(assistant_result_entry_id, index);
            // 参数形状合法不代表截断响应里的工具决策已经完成；整批不执行副作用。
            let prepared = if length_truncated {
                Err(error_result(
                    "tool execution failed: model output was truncated before the tool call completed",
                ))
            } else if !self.request_definitions().has_tool(&call.tool_name) {
                Err(error_result(format!("tool execution failed: unknown tool: {}", call.tool_name)))
            } else {
                self.registry.preflight(&call.tool_name, &call.arguments)
            };
            let parallel = matches!(&prepared, Ok(prepared) if prepared.supports_parallel());
            let required_slots = if parallel { 1 } else { MAX_PARALLEL_TOOL_WORKERS };
            while occupied_slots + required_slots > MAX_PARALLEL_TOOL_WORKERS {
                let event = receiver.recv().await.expect("active tool retains its result sender");
                match self.process_tool_event(event, on_event).await {
                    Ok(released_slots) => occupied_slots -= released_slots,
                    Err(error) => {
                        failure = Some(error);
                        break;
                    }
                }
            }
            if failure.is_some() {
                break;
            }
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
            let execution = match prepared {
                Err(execution) => execution,
                Ok(PreparedTool::Question(args)) => {
                    let started = Instant::now();
                    let mut execution = self
                        .questions
                        .as_ref()
                        .expect("question tool has a host")
                        .ask(
                            singularity_protocol::PendingQuestion {
                                item_id: item_id.clone(),
                                questions: args.questions,
                            },
                            cancellation,
                            on_event,
                        )
                        .await;
                    execution.duration_ms = Some(singularity_core::duration_millis(started.elapsed()));
                    execution
                }
                Ok(prepared) => {
                    let sender = sender.clone();
                    let cwd = cwd.to_path_buf();
                    let signal = cancellation.clone();
                    occupied_slots += required_slots;
                    tokio::spawn(async move {
                        let started = Instant::now();
                        let updates = sender.clone();
                        let progress_id = item_id.clone();
                        let mut execution = prepared
                            .execute(cwd, signal, move |text| {
                                let _ = updates.blocking_send(WorkerEvent::Update {
                                    item_id: progress_id.clone(),
                                    text,
                                });
                            })
                            .await;
                        execution.duration_ms = Some(singularity_core::duration_millis(started.elapsed()));
                        let event = WorkerEvent::Ended {
                            item_id,
                            tool_call_id: call.tool_call_id,
                            execution,
                            occupied_slots: required_slots,
                        };
                        let _ = sender.send(event).await;
                    });
                    continue;
                }
            };
            if let Err(error) = self.finish_tool(&item_id, &call.tool_call_id, execution, on_event).await {
                failure = Some(error);
                break;
            }
        }
        drop(sender);
        // worker 持有发送端直到结果交付；通道关闭即表示所有已派发工具都已收尾。
        while let Some(event) = receiver.recv().await {
            if failure.is_none()
                && let Err(error) = self.process_tool_event(event, on_event).await
            {
                failure = Some(error);
            }
        }
        failure.map_or(Ok(()), Err)
    }

    /// 发布进度或提交完成结果，返回可供后续调用使用的执行位数量。
    async fn process_tool_event(
        &mut self,
        event: WorkerEvent,
        on_event: &mut (dyn FnMut(AgentEvent) + Send),
    ) -> Result<usize> {
        match event {
            WorkerEvent::Update { item_id, text } => {
                on_event(AgentEvent::ToolExecutionUpdate { item_id, partial_result: text });
                Ok(0)
            }
            WorkerEvent::Ended {
                item_id,
                tool_call_id,
                execution,
                occupied_slots,
            } => {
                self.finish_tool(&item_id, &tool_call_id, execution, on_event).await?;
                Ok(occupied_slots)
            }
        }
    }

    async fn finish_tool(
        &mut self,
        item_id: &str,
        tool_call_id: &str,
        execution: ToolExecution,
        on_event: &mut (dyn FnMut(AgentEvent) + Send),
    ) -> Result<()> {
        self.append_message(item_id, tool_result_message(tool_call_id, &execution), &execution.images)
            .await?;
        on_event(AgentEvent::ToolExecutionEnded { item_id: item_id.to_string(), execution });
        Ok(())
    }
}
