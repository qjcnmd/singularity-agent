//! 把 Agent 事件投影成工作台条目，并管理条目的生命周期。
//!
//! assistant 的第一个增量打开条目；完整内容直接完成条目，其余已打开条目在完成或丢弃时
//! 关闭。工具条目复用持久结果的 ID，工具结果由 Agent 直接发布。

use singularity_agent::agent::AgentEvent;
use singularity_protocol::{HistoryItem, ItemRef, TurnEvent};

/// 普通回合与独立压缩共用的事件转换；只有消息和工具要求回合身份。
pub(crate) struct AgentEventProjector {
    turn_id: Option<String>,
    open_assistant_items: std::collections::BTreeSet<String>,
}

impl AgentEventProjector {
    pub(crate) fn new(turn_id: Option<String>) -> Self {
        Self {
            turn_id,
            open_assistant_items: std::collections::BTreeSet::new(),
        }
    }

    pub(crate) fn project(&mut self, sink: &mut dyn FnMut(TurnEvent), event: AgentEvent) {
        match event {
            AgentEvent::ContextInjected { item } => sink(TurnEvent::ContextInjected {
                turn_id: self.turn_id.clone(),
                content: item,
            }),
            AgentEvent::MessageUpdate { message_id, delta } => {
                let item =
                    self.start_assistant_item(singularity_agent::session::text_item_id(&message_id, 0));
                sink(TurnEvent::AssistantDelta {
                    turn_id: self.message_turn_id(),
                    item,
                    delta,
                });
            }
            AgentEvent::ThinkingUpdate { message_id, delta } => {
                let item =
                    self.start_assistant_item(singularity_agent::session::thinking_item_id(&message_id, 0));
                sink(TurnEvent::AssistantThinkingDelta {
                    turn_id: self.message_turn_id(),
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
                            turn_id: self.message_turn_id(),
                            item: ItemRef { item_id },
                        });
                    }
                }
            }
            AgentEvent::MessageFinished { message_id, items, failed } => {
                // 完成事件只含正文和思考，生产侧已按 ItemScope::Completion 物化过，
                // 投影里无需再筛。
                for content in items {
                    let item_id = content.id().to_string();
                    self.finish_assistant_item(sink, &item_id, failed, Some(content));
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
                    turn_id: self.message_turn_id(),
                    item: ItemRef { item_id },
                    tool_name,
                    args: arguments,
                });
            }
            AgentEvent::ToolExecutionUpdate { item_id, partial_result } => {
                sink(TurnEvent::ToolExecutionUpdate {
                    turn_id: self.message_turn_id(),
                    item: ItemRef { item_id },
                    partial_result,
                });
            }
            AgentEvent::ToolExecutionEnded { item_id, execution } => {
                sink(TurnEvent::ToolExecutionEnd {
                    turn_id: self.message_turn_id(),
                    item: ItemRef { item_id },
                    result: singularity_protocol::ToolResult {
                        output: execution.content,
                        images: execution.images.into_iter().map(|image| image.attachment).collect(),
                        is_error: execution.is_error,
                        duration_ms: execution.duration_ms,
                        read_source: execution.read_source,
                    },
                });
            }
            AgentEvent::ProviderAttempt { observation } => {
                sink(TurnEvent::ProviderAttempt {
                    observation,
                    turn_id: self.turn_id.clone(),
                });
            }
            AgentEvent::CompactionUpdated { item } => {
                sink(TurnEvent::CompactionUpdated {
                    turn_id: self.turn_id.clone(),
                    content: item,
                });
            }
            AgentEvent::UserMessage { entry_id, text, images } => {
                sink(TurnEvent::UserMessage {
                    turn_id: self.message_turn_id(),
                    item: ItemRef {
                        item_id: singularity_agent::session::text_item_id(&entry_id, 0),
                    },
                    text,
                    images,
                });
            }
            // runner 的执行循环先截获问题变化，到不了条目投影。
            AgentEvent::PendingQuestionChanged => {}
        }
    }

    fn message_turn_id(&self) -> String {
        self.turn_id.clone().expect("messages and tools belong to a conversation turn")
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
        // 完整内容可能没有流式增量；只有补发空终态时才要求条目已打开。
        if !self.open_assistant_items.remove(item_id) && content.is_none() {
            return;
        }
        let item = ItemRef { item_id: item_id.to_string() };
        sink(if failed {
            TurnEvent::ItemFailed {
                turn_id: self.message_turn_id(),
                item,
                content,
            }
        } else {
            TurnEvent::ItemCompleted {
                turn_id: self.message_turn_id(),
                item,
                content,
            }
        });
    }
}
