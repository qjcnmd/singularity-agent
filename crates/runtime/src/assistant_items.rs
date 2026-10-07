//! 把 Agent 事件投影成工作台条目，并管理条目的生命周期。
//!
//! assistant 的第一个增量会打开条目，工具条目复用持久结果的 ID；
//! assistant 完成或丢弃事件闭合已打开条目，工具结果由 Agent 直接发布。

use singularity_agent::agent::{AgentDiagnostic, AgentEvent};
use singularity_protocol::{HistoryItem, ItemRef, TurnEvent};

const SAFE_ASSISTANT_ITEM_FAILURE: &str = "assistant response failed";

/// 一次 Agent 调用期间还没结束的条目。
pub(crate) struct AssistantItemEvents {
    thread_id: String,
    turn_id: String,
    open_assistant_items: std::collections::BTreeSet<String>,
}

impl AssistantItemEvents {
    pub(crate) fn new(thread_id: String, turn_id: String) -> Self {
        Self {
            thread_id,
            turn_id,
            open_assistant_items: std::collections::BTreeSet::new(),
        }
    }

    pub(crate) fn project(&mut self, sink: &mut dyn FnMut(TurnEvent), event: AgentEvent) {
        match event {
            AgentEvent::MessageUpdate { message_id, delta } => {
                let item =
                    self.start_assistant_item(singularity_agent::session::text_item_id(&message_id, 0));
                sink(TurnEvent::AssistantDelta {
                    thread_id: self.thread_id.clone(),
                    turn_id: self.turn_id.clone(),
                    item,
                    delta,
                });
            }
            AgentEvent::ThinkingUpdate { message_id, delta } => {
                let item =
                    self.start_assistant_item(singularity_agent::session::thinking_item_id(&message_id, 0));
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
            AgentEvent::MessageFinished { message_id, items, failed } => {
                // 完成事件里只有正文和思考（生产侧已按 ItemScope::Completion 物化过），
                // 这里不必再筛一次它自己的产物。
                for content in items {
                    let item = self.start_assistant_item(content.id().to_string());
                    self.finish_assistant_item(sink, &item.item_id, failed, Some(content));
                }
                for item_id in [
                    singularity_agent::session::thinking_item_id(&message_id, 0),
                    singularity_agent::session::text_item_id(&message_id, 0),
                ] {
                    self.finish_assistant_item(sink, &item_id, failed, None);
                }
            }
            AgentEvent::ToolExecutionStarted { item_id, tool_name, arguments } => {
                sink(TurnEvent::ToolExecutionStart {
                    thread_id: self.thread_id.clone(),
                    turn_id: self.turn_id.clone(),
                    item: ItemRef { item_id },
                    tool_name,
                    args: arguments,
                    started_at: singularity_core::now_iso(),
                });
            }
            AgentEvent::ToolExecutionUpdate { item_id, partial_result } => {
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
                    item: ItemRef { item_id },
                    output: execution.content,
                    images: execution.images.into_iter().map(|image| image.attachment).collect(),
                    is_error: execution.is_error,
                    duration_ms: execution.duration_ms,
                    read_source: execution.read_source,
                });
            }
            AgentEvent::Diagnostic(diagnostic) => {
                sink(self.diagnostic_event(diagnostic));
            }
            AgentEvent::ProviderAttempt { observation } => {
                sink(TurnEvent::ProviderAttempt {
                    observation,
                    thread_id: self.thread_id.clone(),
                    turn_id: Some(self.turn_id.clone()),
                });
            }
            AgentEvent::UserMessage { entry_id, text, images } => {
                sink(TurnEvent::UserMessage {
                    thread_id: self.thread_id.clone(),
                    turn_id: self.turn_id.clone(),
                    item: ItemRef {
                        item_id: singularity_agent::session::text_item_id(&entry_id, 0),
                    },
                    text,
                    images,
                });
            }
            // ControlChanged 在 runner 的执行循环里就被截获并更新控制投影，
            // 不会走到条目投影。
            AgentEvent::ControlChanged => {}
        }
    }

    fn diagnostic_event(&self, diagnostic: AgentDiagnostic) -> TurnEvent {
        let AgentDiagnostic { severity, code, message } = diagnostic;
        TurnEvent::Diagnostic {
            thread_id: self.thread_id.clone(),
            turn_id: Some(self.turn_id.clone()),
            severity,
            code,
            message,
        }
    }

    fn start_assistant_item(&mut self, item_id: String) -> ItemRef {
        self.open_assistant_items.insert(item_id.clone());
        ItemRef { item_id }
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
        let item = ItemRef { item_id: item_id.to_string() };
        sink(if failed {
            TurnEvent::ItemFailed {
                thread_id: self.thread_id.clone(),
                turn_id: self.turn_id.clone(),
                item,
                content,
                error: SAFE_ASSISTANT_ITEM_FAILURE.to_string(),
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
}
