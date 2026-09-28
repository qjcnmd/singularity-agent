//! 按模型给出的顺序准入工具。只读调用共享准入锁；命令和文件修改独占它。
//! 结果一完成就落盘，随后才发布完成事件。失败后排空已启动的工具，不派发后续调用。

use std::sync::Arc;
use std::time::Instant;

use singularity_model::ModelToolCall;
use tokio::sync::{RwLock, mpsc};
use tokio_util::sync::CancellationToken;

use super::{Agent, AgentEvent, Result};
use crate::message::tool_result_message;
use crate::session::lock_writer;
use crate::tools::{ToolExecution, error_result};

// 只读工具仍在线程池执行文件 I/O，限制同时运行的数量以控制资源竞争。
const MAX_PARALLEL_TOOL_WORKERS: u32 = 8;
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
        _admission: Admission,
    },
}

impl Agent {
    /// 准入在派发者中按 source order 等待，确保后面的独占调用不会越过前面的读取。
    /// 等待锁期间继续消费结果和进度，避免背压阻止已运行工具释放锁。
    pub(super) async fn dispatch_tools(
        &mut self,
        tool_calls: Vec<ModelToolCall>,
        assistant_result_entry_id: &str,
        length_truncated: bool,
        cancellation: &CancellationToken,
        on_event: &mut (dyn FnMut(AgentEvent) + Send),
    ) -> Result<()> {
        // 只为读 cwd 短暂持有会话写者锁；绝不跨工具执行持锁，否则会阻塞控制
        // 接受与终态落盘（工具 worker 与控制面共用同一写者）。
        let cwd = lock_writer(&self.session).cwd().to_path_buf();
        let gate = Arc::new(RwLock::with_max_readers((), MAX_PARALLEL_TOOL_WORKERS));
        let (sender, mut receiver) = mpsc::channel(OUTPUT_QUEUE_CAPACITY);
        let mut active = 0usize;
        let mut failure = None;

        for (index, call) in tool_calls.into_iter().enumerate() {
            if failure.is_some() {
                break;
            }
            let item_id = crate::session::tool_item_id(assistant_result_entry_id, index);
            let prepared = if length_truncated {
                Err(error_result(
                    "tool execution failed: model output was truncated before the tool call completed",
                ))
            } else {
                self.registry.preflight(&call.tool_name, &call.arguments)
            };
            let parallel = matches!(&prepared, Ok(prepared) if prepared.supports_parallel());
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
                        let event = event.expect("active tool retains its result sender");
                        active -= usize::from(matches!(event, WorkerEvent::Ended { .. }));
                        if let Err(error) = self.process_tool_event(event, on_event).await {
                            failure = Some(error);
                            break None;
                        }
                    }
                }
            };
            let Some(guard) = guard else { break };
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
            let prepared = match prepared {
                Ok(prepared) => prepared,
                Err(execution) => {
                    if let Err(error) = self
                        .commit_tool_result(&item_id, &call.tool_call_id, &execution)
                        .await
                    {
                        failure = Some(error);
                        break;
                    }
                    on_event(AgentEvent::ToolExecutionEnded { item_id, execution });
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
                    _admission: guard,
                };
                let _ = sender.send(event).await;
            });
        }
        drop(sender);
        while active > 0 {
            let event = receiver
                .recv()
                .await
                .expect("active tool retains its result sender");
            let ended = matches!(event, WorkerEvent::Ended { .. });
            if failure.is_none()
                && let Err(error) = self.process_tool_event(event, on_event).await
            {
                failure = Some(error);
            }
            active -= usize::from(ended);
        }
        failure.map_or(Ok(()), Err)
    }

    async fn process_tool_event(
        &mut self,
        event: WorkerEvent,
        on_event: &mut (dyn FnMut(AgentEvent) + Send),
    ) -> Result<()> {
        match event {
            WorkerEvent::Update { item_id, text } => on_event(AgentEvent::ToolExecutionUpdate {
                item_id,
                partial_result: text,
            }),
            WorkerEvent::Ended {
                item_id,
                tool_call_id,
                execution,
                _admission: _guard,
            } => {
                self.commit_tool_result(&item_id, &tool_call_id, &execution)
                    .await?;
                on_event(AgentEvent::ToolExecutionEnded { item_id, execution });
            }
        }
        Ok(())
    }

    async fn commit_tool_result(
        &mut self,
        item_id: &str,
        tool_call_id: &str,
        execution: &ToolExecution,
    ) -> Result<()> {
        self.append_message(Some(item_id), tool_result_message(tool_call_id, execution))
            .await
            .map(|_| ())
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
