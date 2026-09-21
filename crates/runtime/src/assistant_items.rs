//! 把 Agent 事件投影成工作台条目，并管理条目的生命周期。
//!
//! assistant 的第一个增量会打开条目，工具条目复用持久结果的 ID；
//! turn 终态落盘后关闭剩下的条目，每个条目的终态只发布一次。

use singularity_agent::agent::{AgentDiagnostic, AgentEvent};
use singularity_protocol::{HistoryItem, ItemRef, TurnEvent};

const SAFE_ASSISTANT_ITEM_FAILURE: &str = "assistant response failed";
const SAFE_TOOL_ITEM_FAILURE: &str = "tool execution failed";

/// 一次 AgentLoop 调用期间还没结束的条目。
pub(crate) struct AssistantItemEvents {
    thread_id: String,
    turn_id: String,
    open_assistant_items: std::collections::BTreeSet<String>,
    open_tool_items: std::collections::BTreeSet<String>,
}

impl AssistantItemEvents {
    pub(crate) fn new(thread_id: String, turn_id: String) -> Self {
        Self {
            thread_id,
            turn_id,
            open_assistant_items: std::collections::BTreeSet::new(),
            open_tool_items: std::collections::BTreeSet::new(),
        }
    }

    pub(crate) fn project(&mut self, sink: &mut dyn FnMut(TurnEvent), event: AgentEvent) {
        match event {
            AgentEvent::MessageUpdate { message_id, delta } => {
                let item = self.start_assistant_item(
                    sink,
                    singularity_agent::session::text_item_id(&message_id, 0),
                );
                sink(TurnEvent::AssistantDelta {
                    thread_id: self.thread_id.clone(),
                    turn_id: self.turn_id.clone(),
                    item,
                    delta,
                });
            }
            AgentEvent::ThinkingUpdate { message_id, delta } => {
                let item = self.start_assistant_item(
                    sink,
                    singularity_agent::session::thinking_item_id(&message_id, 0),
                );
                sink(TurnEvent::AssistantThinkingDelta {
                    thread_id: self.thread_id.clone(),
                    turn_id: self.turn_id.clone(),
                    item,
                    delta,
                });
            }
            AgentEvent::MessageDiscarded { message_id } => {
                for item_id in [
                    singularity_agent::session::thinking_item_id(&message_id, 0),
                    singularity_agent::session::text_item_id(&message_id, 0),
                ] {
                    if self.open_assistant_items.remove(&item_id) {
                        sink(TurnEvent::ItemDiscarded {
                            thread_id: self.thread_id.clone(),
                            turn_id: self.turn_id.clone(),
                            item: ItemRef { item_id },
                        });
                    }
                }
            }
            AgentEvent::MessageFinished {
                message_id,
                items,
                failed,
            } => {
                // 完成事件里只有正文和思考（生产侧已按 ItemScope::Completion 物化过），
                // 这里不必再筛一次它自己的产物。
                for content in items {
                    let item = self.start_assistant_item(sink, content.id().to_string());
                    self.finish_assistant_item(sink, &item.item_id, failed, Some(content));
                }
                for item_id in [
                    singularity_agent::session::thinking_item_id(&message_id, 0),
                    singularity_agent::session::text_item_id(&message_id, 0),
                ] {
                    self.finish_assistant_item(sink, &item_id, failed, None);
                }
            }
            AgentEvent::ToolExecutionStarted {
                item_id,
                tool_name,
                arguments,
            } => {
                self.open_tool_items.insert(item_id.clone());
                sink(TurnEvent::ToolExecutionStart {
                    thread_id: self.thread_id.clone(),
                    turn_id: self.turn_id.clone(),
                    item: ItemRef { item_id },
                    tool_name,
                    args: arguments,
                    started_at: singularity_core::now_iso(),
                });
            }
            AgentEvent::ToolExecutionUpdate {
                item_id,
                partial_result,
            } => {
                sink(TurnEvent::ToolExecutionUpdate {
                    thread_id: self.thread_id.clone(),
                    turn_id: self.turn_id.clone(),
                    item: ItemRef { item_id },
                    partial_result,
                });
            }
            AgentEvent::ToolExecutionEnded { item_id, execution } => {
                sink(TurnEvent::ToolExecutionEnd {
                    thread_id: self.thread_id.clone(),
                    turn_id: self.turn_id.clone(),
                    item: ItemRef {
                        item_id: item_id.clone(),
                    },
                    output: execution.content,
                    is_error: execution.is_error,
                    diff: execution.diff,
                    duration_ms: execution.duration_ms,
                    read_source: execution.read_source,
                });
                self.open_tool_items.remove(&item_id);
            }
            AgentEvent::Diagnostic(diagnostic) => {
                sink(self.diagnostic_event(diagnostic));
            }
            AgentEvent::ProviderAttempt {
                observation,
                protocol,
                retry_after_ms,
            } => {
                sink(TurnEvent::ProviderAttempt {
                    observation,
                    thread_id: self.thread_id.clone(),
                    turn_id: self.turn_id.clone(),
                    protocol,
                    retry_after_ms,
                });
            }
            AgentEvent::UserMessage { entry_id, text } => {
                sink(TurnEvent::UserMessage {
                    thread_id: self.thread_id.clone(),
                    turn_id: self.turn_id.clone(),
                    item: ItemRef {
                        item_id: singularity_agent::session::text_item_id(&entry_id, 0),
                    },
                    text,
                });
            }
            // ControlChanged 在 runner 的执行循环里就被截获并更新控制投影，
            // 不会走到条目投影。
            AgentEvent::ControlChanged(_) => {}
        }
    }

    fn diagnostic_event(&self, diagnostic: AgentDiagnostic) -> TurnEvent {
        let AgentDiagnostic {
            severity,
            code,
            message,
        } = diagnostic;
        TurnEvent::Diagnostic {
            thread_id: self.thread_id.clone(),
            turn_id: self.turn_id.clone(),
            severity,
            code,
            message,
        }
    }

    fn start_assistant_item(
        &mut self,
        sink: &mut dyn FnMut(TurnEvent),
        item_id: String,
    ) -> ItemRef {
        let item = ItemRef { item_id };
        if self.open_assistant_items.insert(item.item_id.clone()) {
            sink(TurnEvent::ItemStarted {
                thread_id: self.thread_id.clone(),
                turn_id: self.turn_id.clone(),
                item: item.clone(),
            });
        }
        item
    }

    fn finish_assistant_item(
        &mut self,
        sink: &mut dyn FnMut(TurnEvent),
        item_id: &str,
        failed: bool,
        content: Option<HistoryItem>,
    ) {
        if !self.open_assistant_items.remove(item_id) {
            return;
        }
        self.emit_item_terminal(
            sink,
            item_id,
            failed.then_some(SAFE_ASSISTANT_ITEM_FAILURE),
            content,
        );
    }

    fn emit_item_terminal(
        &self,
        sink: &mut dyn FnMut(TurnEvent),
        item_id: &str,
        error: Option<&str>,
        content: Option<HistoryItem>,
    ) {
        let item = ItemRef {
            item_id: item_id.to_string(),
        };
        sink(if let Some(error) = error {
            TurnEvent::ItemFailed {
                thread_id: self.thread_id.clone(),
                turn_id: self.turn_id.clone(),
                item,
                content,
                error: error.to_string(),
            }
        } else {
            TurnEvent::ItemCompleted {
                thread_id: self.thread_id.clone(),
                turn_id: self.turn_id.clone(),
                item,
                content,
            }
        });
    }

    /// 在 turn 终态之前，关掉被中断的 tool item 以及剩下的 assistant item。
    pub(crate) fn finish_open_items(&mut self, sink: &mut dyn FnMut(TurnEvent), failed: bool) {
        for id in std::mem::take(&mut self.open_tool_items) {
            self.emit_item_terminal(sink, &id, Some(SAFE_TOOL_ITEM_FAILURE), None);
        }
        for id in std::mem::take(&mut self.open_assistant_items) {
            self.emit_item_terminal(
                sink,
                &id,
                failed.then_some(SAFE_ASSISTANT_ITEM_FAILURE),
                None,
            );
        }
    }
}
