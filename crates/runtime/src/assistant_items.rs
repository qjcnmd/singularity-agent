//! Agent 事件到工作台条目的投影与生命周期。
//!
//! assistant 首个增量打开条目，工具条目复用持久结果 ID；
//! turn 终态落盘后关闭剩余条目，每个条目的终态只发布一次。

use crate::events::{ItemRef, ProviderAttemptStatus, ToolResultPayload, TurnEvent};
use singularity_agent::agent::{AgentDiagnostic, AgentEvent};

const SAFE_ASSISTANT_ITEM_FAILURE: &str = "assistant response failed";
const SAFE_TOOL_ITEM_FAILURE: &str = "tool execution failed";

/// 一次 AgentLoop 调用中的未结束条目。
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

    /// 将 AgentEvent 投影为公开事件，并更新未结束条目。
    pub(crate) fn project(&mut self, sink: &mut dyn FnMut(TurnEvent), event: AgentEvent) {
        match event {
            AgentEvent::MessageUpdate { message_id, delta } => {
                let item = self.start_assistant_item(sink, format!("{message_id}:text:0"));
                sink(TurnEvent::AssistantDelta {
                    thread_id: self.thread_id.clone(),
                    turn_id: self.turn_id.clone(),
                    item,
                    delta,
                });
            }
            AgentEvent::ThinkingUpdate { message_id, delta } => {
                let item = self.start_assistant_item(sink, format!("{message_id}:thinking:0"));
                sink(TurnEvent::AssistantThinkingDelta {
                    thread_id: self.thread_id.clone(),
                    turn_id: self.turn_id.clone(),
                    item,
                    delta,
                });
            }
            AgentEvent::Thinking { message_id, text } => {
                let item = self.start_assistant_item(sink, format!("{message_id}:thinking:0"));
                sink(TurnEvent::AssistantThinking {
                    thread_id: self.thread_id.clone(),
                    turn_id: self.turn_id.clone(),
                    item,
                    text,
                });
            }
            AgentEvent::MessageFinished { message_id, failed } => {
                for suffix in ["thinking:0", "text:0"] {
                    self.finish_assistant_item(sink, &format!("{message_id}:{suffix}"), failed);
                }
            }
            AgentEvent::ToolExecutionStarted {
                item_id,
                tool_name,
                arguments,
            } => {
                self.open_tool_items.insert(item_id.clone());
                sink(TurnEvent::ItemStarted {
                    thread_id: self.thread_id.clone(),
                    turn_id: self.turn_id.clone(),
                    item: ItemRef {
                        item_id: item_id.clone(),
                    },
                });
                sink(TurnEvent::ToolExecutionStart {
                    thread_id: self.thread_id.clone(),
                    turn_id: self.turn_id.clone(),
                    tool_call_id: item_id,
                    tool_name,
                    args: arguments,
                    started_at: Some(singularity_core::now_iso()),
                });
            }
            AgentEvent::ToolExecutionUpdate {
                item_id,
                tool_name,
                arguments,
                partial_result,
            } => {
                sink(TurnEvent::ToolExecutionUpdate {
                    thread_id: self.thread_id.clone(),
                    turn_id: self.turn_id.clone(),
                    tool_call_id: item_id,
                    tool_name,
                    args: arguments,
                    partial_result,
                });
            }
            AgentEvent::ToolExecutionEnded {
                item_id,
                tool_name,
                execution,
            } => {
                sink(TurnEvent::ToolExecutionEnd {
                    thread_id: self.thread_id.clone(),
                    turn_id: self.turn_id.clone(),
                    tool_call_id: item_id.clone(),
                    tool_name,
                    result: ToolResultPayload::new(
                        execution.content,
                        execution.is_error,
                        execution.diff,
                    ),
                    duration_ms: execution.duration_ms,
                });
                if self.open_tool_items.remove(&item_id) {
                    self.emit_item_terminal(
                        sink,
                        &item_id,
                        execution.is_error.then_some(SAFE_TOOL_ITEM_FAILURE),
                    );
                }
            }
            AgentEvent::Diagnostic(diagnostic) => {
                sink(self.diagnostic_event(diagnostic));
            }
            AgentEvent::ProviderAttempt {
                observation,
                protocol,
                diagnostic_code,
                retry_after_ms,
                retry_after_source,
            } => {
                sink(TurnEvent::ProviderAttempt {
                    request_id: observation.request_id,
                    request_head: observation.request_head,
                    purpose: observation.purpose,
                    thread_id: self.thread_id.clone(),
                    turn_id: self.turn_id.clone(),
                    attempt: observation.attempt,
                    model_turn_ordinal: observation.ordinal,
                    provider: observation.provider,
                    model: observation.model,
                    protocol,
                    status: observation.status,
                    attempt_duration_ms: (observation.status != ProviderAttemptStatus::Started)
                        .then_some(observation.duration_ms),
                    input_tokens: observation.input_tokens,
                    output_tokens: observation.output_tokens,
                    cached_input_tokens: observation.cached_input_tokens,
                    error_category: observation.error,
                    diagnostic_code,
                    retry_after_ms,
                    retry_after_source,
                });
            }
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
    ) {
        if !self.open_assistant_items.remove(item_id) {
            return;
        }
        self.emit_item_terminal(sink, item_id, failed.then_some(SAFE_ASSISTANT_ITEM_FAILURE));
    }

    fn emit_item_terminal(
        &self,
        sink: &mut dyn FnMut(TurnEvent),
        item_id: &str,
        error: Option<&str>,
    ) {
        let item = ItemRef {
            item_id: item_id.to_string(),
        };
        sink(if let Some(error) = error {
            TurnEvent::ItemFailed {
                thread_id: self.thread_id.clone(),
                turn_id: self.turn_id.clone(),
                item,
                error: error.to_string(),
            }
        } else {
            TurnEvent::ItemCompleted {
                thread_id: self.thread_id.clone(),
                turn_id: self.turn_id.clone(),
                item,
            }
        });
    }

    /// Close interrupted tools and remaining assistant items before the turn terminal.
    pub(crate) fn finish_open_items(&mut self, sink: &mut dyn FnMut(TurnEvent), failed: bool) {
        for id in std::mem::take(&mut self.open_tool_items) {
            self.emit_item_terminal(sink, &id, Some(SAFE_TOOL_ITEM_FAILURE));
        }
        for id in std::mem::take(&mut self.open_assistant_items) {
            self.emit_item_terminal(sink, &id, failed.then_some(SAFE_ASSISTANT_ITEM_FAILURE));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_items_keep_durable_identity_and_close_before_the_next_response() {
        let mut projection = AssistantItemEvents::new("thread".into(), "turn".into());
        let mut events = Vec::new();
        let mut sink = |event| events.push(event);
        for event in [
            AgentEvent::ThinkingUpdate {
                message_id: "m1".into(),
                delta: "think".into(),
            },
            AgentEvent::MessageUpdate {
                message_id: "m1".into(),
                delta: "before tool".into(),
            },
            AgentEvent::Thinking {
                message_id: "m1".into(),
                text: "think".into(),
            },
            AgentEvent::MessageFinished {
                message_id: "m1".into(),
                failed: false,
            },
            AgentEvent::MessageUpdate {
                message_id: "m2".into(),
                delta: "after tool".into(),
            },
            AgentEvent::MessageFinished {
                message_id: "m2".into(),
                failed: true,
            },
        ] {
            projection.project(&mut sink, event);
        }
        projection.finish_open_items(&mut sink, false);
        let starts: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                TurnEvent::ItemStarted { item, .. } => Some(item.item_id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(starts, vec!["m1:thinking:0", "m1:text:0", "m2:text:0"]);
        let closed: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                TurnEvent::ItemCompleted { item, .. } => Some((item.item_id.as_str(), false)),
                TurnEvent::ItemFailed { item, .. } => Some((item.item_id.as_str(), true)),
                _ => None,
            })
            .collect();
        assert_eq!(
            closed,
            vec![
                ("m1:thinking:0", false),
                ("m1:text:0", false),
                ("m2:text:0", true)
            ]
        );
        assert!(projection.open_assistant_items.is_empty());
    }
}
