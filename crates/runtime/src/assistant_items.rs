//! AgentLoop 事件 → typed item 事件的投影状态。
//!
//! 一次 AgentLoop 调用预分配的 assistant/tool item 事件状态：assistant 增量
//! 首见时开项、工具按调用 id 就地刷新、终态事件只发一次。

use crate::events::{TurnEvent, TurnEventSink};

const SAFE_ASSISTANT_ITEM_FAILURE: &str = "assistant response failed";

/// 一次 AgentLoop 调用预分配的 assistant/tool item 事件状态。
pub(crate) struct AssistantItemEvents {
    thread_id: String,
    turn_id: String,
    item_id: String,
    first_delta_observed: bool,
    assistant_terminal_generated: bool,
    tool_items: std::collections::HashMap<String, bool>,
}

impl AssistantItemEvents {
    pub(crate) fn new(thread_id: String, turn_id: String, item_id: String) -> Self {
        Self {
            thread_id,
            turn_id,
            item_id,
            first_delta_observed: false,
            assistant_terminal_generated: false,
            tool_items: std::collections::HashMap::new(),
        }
    }

    pub(crate) fn start_tool_item(&mut self, tool_call_id: &str) {
        self.tool_items
            .entry(tool_call_id.to_string())
            .or_insert(false);
    }

    pub(crate) fn open_tool_items(&self) -> Vec<String> {
        self.tool_items
            .iter()
            .filter_map(|(id, terminal)| (!*terminal).then_some(id.clone()))
            .collect()
    }

    pub(crate) fn project_assistant_delta(&mut self, sink: &mut dyn TurnEventSink, delta: &str) {
        if !self.first_delta_observed {
            self.first_delta_observed = true;
            sink.emit(TurnEvent::ItemStarted {
                thread_id: self.thread_id.clone(),
                turn_id: self.turn_id.clone(),
                item_id: self.item_id.clone(),
            });
        }
        sink.emit(TurnEvent::AssistantDelta {
            thread_id: self.thread_id.clone(),
            turn_id: self.turn_id.clone(),
            item_id: self.item_id.clone(),
            delta: delta.to_string(),
        });
    }

    pub(crate) fn emit_tool_terminal(
        &mut self,
        sink: &mut dyn TurnEventSink,
        tool_call_id: &str,
        is_error: bool,
    ) {
        let terminal = self.tool_items.get_mut(tool_call_id);
        match terminal {
            Some(already) if *already => {}
            Some(already) => {
                *already = true;
                let event = if is_error {
                    TurnEvent::ItemFailed {
                        thread_id: self.thread_id.clone(),
                        turn_id: self.turn_id.clone(),
                        item_id: tool_call_id.to_string(),
                        error: "tool execution failed".to_string(),
                    }
                } else {
                    TurnEvent::ItemCompleted {
                        thread_id: self.thread_id.clone(),
                        turn_id: self.turn_id.clone(),
                        item_id: tool_call_id.to_string(),
                    }
                };
                sink.emit(event);
            }
            None => {}
        }
    }

    pub(crate) fn emit_assistant_terminal_failed(&mut self, sink: &mut dyn TurnEventSink) {
        if !self.first_delta_observed || self.assistant_terminal_generated {
            return;
        }
        self.assistant_terminal_generated = true;
        sink.emit(TurnEvent::ItemFailed {
            thread_id: self.thread_id.clone(),
            turn_id: self.turn_id.clone(),
            item_id: self.item_id.clone(),
            error: SAFE_ASSISTANT_ITEM_FAILURE.to_string(),
        });
    }

    pub(crate) fn emit_assistant_terminal_completed(&mut self, sink: &mut dyn TurnEventSink) {
        if !self.first_delta_observed || self.assistant_terminal_generated {
            return;
        }
        self.assistant_terminal_generated = true;
        sink.emit(TurnEvent::ItemCompleted {
            thread_id: self.thread_id.clone(),
            turn_id: self.turn_id.clone(),
            item_id: self.item_id.clone(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_appearance_does_not_create_an_assistant_terminal_item() {
        let mut item_events =
            AssistantItemEvents::new("thread".into(), "turn".into(), "assistant".into());
        let mut events = Vec::new();

        item_events.start_tool_item("tool");
        item_events.emit_tool_terminal(&mut |event| events.push(event), "tool", false);
        item_events.emit_assistant_terminal_completed(&mut |event| events.push(event));
        item_events.emit_assistant_terminal_failed(&mut |event| events.push(event));

        assert!(matches!(
            events.as_slice(),
            [TurnEvent::ItemCompleted { item_id, .. }] if item_id == "tool"
        ));
    }
}
