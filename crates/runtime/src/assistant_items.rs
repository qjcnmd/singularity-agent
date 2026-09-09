//! AgentLoop 事件 → typed item 事件的投影状态。
//!
//! 一次 AgentLoop 调用预分配的 assistant/tool item 事件状态：assistant 增量
//! 首见时开项、工具按调用 id 就地刷新、终态事件只发一次。AgentEvent 到
//! TurnEvent 的全部映射集中于此，实时发射与事实累积同源。attempt 观测的
//! 状态与分类词形来自 model/protocol 的单源类型与 Display 投影，本层不再
//! 维护第二份映射。

use crate::events::{ItemRef, ProviderAttemptStatus, ToolResultPayload, TurnEvent};
use singularity_agent::agent::{AgentDiagnostic, AgentEvent};
use singularity_model::ProviderAttemptEvent;

const SAFE_ASSISTANT_ITEM_FAILURE: &str = "assistant response failed";

/// 一次 AgentLoop 调用预分配的 assistant/tool item 事件状态。
pub(crate) struct AssistantItemEvents {
    thread_id: String,
    turn_id: String,
    open_assistant_items: std::collections::BTreeSet<String>,
    tool_items: std::collections::HashMap<String, bool>,
}

impl AssistantItemEvents {
    pub(crate) fn new(thread_id: String, turn_id: String) -> Self {
        Self {
            thread_id,
            turn_id,
            open_assistant_items: std::collections::BTreeSet::new(),
            tool_items: std::collections::HashMap::new(),
        }
    }

    pub(crate) fn start_tool_item(&mut self, tool_call_id: &str) {
        self.tool_items
            .entry(tool_call_id.to_string())
            .or_insert(false);
    }

    /// AgentEvent → TurnEvent 的唯一映射入口：实时投影 + item 生命周期
    /// 事实累积在同一处完成。
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
                tool_name,
                tool_call_id,
                arguments,
            } => {
                self.start_tool_item(&tool_call_id);
                sink(TurnEvent::ItemStarted {
                    thread_id: self.thread_id.clone(),
                    turn_id: self.turn_id.clone(),
                    item: ItemRef {
                        item_id: tool_call_id.clone(),
                    },
                });
                sink(TurnEvent::ToolExecutionStart {
                    thread_id: self.thread_id.clone(),
                    turn_id: self.turn_id.clone(),
                    tool_call_id,
                    tool_name,
                    args: arguments,
                    started_at: Some(singularity_agent::session::file::now_iso()),
                });
            }
            AgentEvent::ToolExecutionUpdate {
                tool_name,
                tool_call_id,
                arguments,
                partial_result,
            } => {
                sink(TurnEvent::ToolExecutionUpdate {
                    thread_id: self.thread_id.clone(),
                    turn_id: self.turn_id.clone(),
                    tool_call_id,
                    tool_name,
                    args: arguments,
                    partial_result,
                });
            }
            AgentEvent::ToolExecutionEnded {
                tool_name,
                tool_call_id,
                execution,
            } => {
                sink(TurnEvent::ToolExecutionEnd {
                    thread_id: self.thread_id.clone(),
                    turn_id: self.turn_id.clone(),
                    tool_call_id: tool_call_id.clone(),
                    tool_name,
                    result: ToolResultPayload::new(
                        execution.content,
                        execution.is_error,
                        execution.diff,
                    ),
                    duration_ms: execution.duration_ms,
                });
                self.emit_tool_terminal(sink, &tool_call_id, execution.is_error);
            }
            AgentEvent::Diagnostic(diagnostic) => {
                sink(self.diagnostic_event(diagnostic));
            }
            AgentEvent::ProviderAttempt {
                request_id,
                request_head,
                purpose,
                model_turn_ordinal,
                event,
            } => {
                sink(self.provider_attempt_event(
                    model_turn_ordinal,
                    &event,
                    purpose,
                    request_id,
                    request_head,
                ));
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

    fn provider_attempt_event(
        &self,
        model_turn_ordinal: u32,
        attempt: &ProviderAttemptEvent,
        purpose: singularity_protocol::RequestPurpose,
        request_id: String,
        request_head: Option<serde_json::Value>,
    ) -> TurnEvent {
        match attempt {
            ProviderAttemptEvent::Started(started) => TurnEvent::ProviderAttempt {
                request_id,
                request_head,
                purpose,
                thread_id: self.thread_id.clone(),
                turn_id: self.turn_id.clone(),
                attempt: started.attempt,
                model_turn_ordinal,
                provider: started.provider_name.clone(),
                model: started.model_name.clone(),
                protocol: started.actual_api_protocol.to_string(),
                status: ProviderAttemptStatus::Started,
                attempt_duration_ms: None,
                input_tokens: None,
                output_tokens: None,
                cached_input_tokens: None,
                error_category: None,
                diagnostic_code: None,
                retry_after_ms: None,
                retry_after_source: None,
            },
            ProviderAttemptEvent::Finished(occurrence) => TurnEvent::ProviderAttempt {
                request_id,
                request_head,
                purpose,
                thread_id: self.thread_id.clone(),
                turn_id: self.turn_id.clone(),
                attempt: occurrence.attempt,
                model_turn_ordinal,
                provider: occurrence.provider_name.clone(),
                model: occurrence.model_name.clone(),
                protocol: occurrence.actual_api_protocol.to_string(),
                status: occurrence.terminal_status,
                attempt_duration_ms: Some(occurrence.attempt_duration_ms),
                input_tokens: occurrence
                    .usage
                    .as_ref()
                    .filter(|usage| usage.usage_present)
                    .map(|usage| usage.input_tokens),
                output_tokens: occurrence
                    .usage
                    .as_ref()
                    .filter(|usage| usage.usage_present)
                    .map(|usage| usage.output_tokens),
                cached_input_tokens: occurrence
                    .usage
                    .as_ref()
                    .filter(|usage| usage.usage_present && usage.cached_input_tokens_present)
                    .map(|usage| usage.cached_input_tokens),
                error_category: occurrence.error_category.as_ref().map(ToString::to_string),
                diagnostic_code: occurrence.diagnostic_code.clone(),
                retry_after_ms: occurrence.retry_after_ms,
                retry_after_source: occurrence.retry_after_source,
            },
        }
    }

    pub(crate) fn open_tool_items(&self) -> Vec<String> {
        self.tool_items
            .iter()
            .filter_map(|(id, terminal)| (!*terminal).then_some(id.clone()))
            .collect()
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
        let item = ItemRef {
            item_id: item_id.to_string(),
        };
        sink(if failed {
            TurnEvent::ItemFailed {
                thread_id: self.thread_id.clone(),
                turn_id: self.turn_id.clone(),
                item,
                error: SAFE_ASSISTANT_ITEM_FAILURE.to_string(),
            }
        } else {
            TurnEvent::ItemCompleted {
                thread_id: self.thread_id.clone(),
                turn_id: self.turn_id.clone(),
                item,
            }
        });
    }

    pub(crate) fn emit_tool_terminal(
        &mut self,
        sink: &mut dyn FnMut(TurnEvent),
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
                        item: ItemRef {
                            item_id: tool_call_id.to_string(),
                        },
                        error: "tool execution failed".to_string(),
                    }
                } else {
                    TurnEvent::ItemCompleted {
                        thread_id: self.thread_id.clone(),
                        turn_id: self.turn_id.clone(),
                        item: ItemRef {
                            item_id: tool_call_id.to_string(),
                        },
                    }
                };
                sink(event);
            }
            None => {}
        }
    }

    pub(crate) fn emit_assistant_terminal_failed(&mut self, sink: &mut dyn FnMut(TurnEvent)) {
        for id in self.open_assistant_items.clone() {
            self.finish_assistant_item(sink, &id, true);
        }
    }

    pub(crate) fn emit_assistant_terminal_completed(&mut self, sink: &mut dyn FnMut(TurnEvent)) {
        for id in self.open_assistant_items.clone() {
            self.finish_assistant_item(sink, &id, false);
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
        projection.emit_assistant_terminal_completed(&mut sink);
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
