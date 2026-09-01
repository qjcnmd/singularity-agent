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

    /// AgentEvent → TurnEvent 的唯一映射入口：实时投影 + item 生命周期
    /// 事实累积在同一处完成。
    pub(crate) fn project(&mut self, sink: &mut dyn FnMut(TurnEvent), event: AgentEvent) {
        match event {
            AgentEvent::MessageUpdate { delta } => {
                self.project_assistant_delta(sink, &delta);
            }
            AgentEvent::Thinking { text } => {
                sink(TurnEvent::AssistantThinking {
                    thread_id: self.thread_id.clone(),
                    turn_id: self.turn_id.clone(),
                    text,
                });
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
                    result: ToolResultPayload::text(execution.content, execution.is_error),
                });
                self.emit_tool_terminal(sink, &tool_call_id, execution.is_error);
            }
            AgentEvent::Diagnostic(diagnostic) => {
                sink(self.diagnostic_event(diagnostic));
            }
            AgentEvent::ProviderAttempt {
                model_turn_ordinal,
                event,
            } => {
                sink(self.provider_attempt_event(model_turn_ordinal, &event));
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
    ) -> TurnEvent {
        match attempt {
            ProviderAttemptEvent::Started(started) => TurnEvent::ProviderAttempt {
                thread_id: self.thread_id.clone(),
                turn_id: self.turn_id.clone(),
                attempt: started.attempt,
                model_turn_ordinal,
                provider: started.provider_name.clone(),
                model: started.model_name.clone(),
                protocol: started.actual_api_protocol.to_string(),
                status: ProviderAttemptStatus::Started,
                attempt_duration_ms: None,
                error_category: None,
                diagnostic_code: None,
                retry_after_ms: None,
                retry_after_source: None,
            },
            ProviderAttemptEvent::Finished(occurrence) => TurnEvent::ProviderAttempt {
                thread_id: self.thread_id.clone(),
                turn_id: self.turn_id.clone(),
                attempt: occurrence.attempt,
                model_turn_ordinal,
                provider: occurrence.provider_name.clone(),
                model: occurrence.model_name.clone(),
                protocol: occurrence.actual_api_protocol.to_string(),
                status: occurrence.terminal_status,
                attempt_duration_ms: Some(occurrence.attempt_duration_ms),
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

    pub(crate) fn project_assistant_delta(&mut self, sink: &mut dyn FnMut(TurnEvent), delta: &str) {
        if !self.first_delta_observed {
            self.first_delta_observed = true;
            sink(TurnEvent::ItemStarted {
                thread_id: self.thread_id.clone(),
                turn_id: self.turn_id.clone(),
                item: ItemRef {
                    item_id: self.item_id.clone(),
                },
            });
        }
        sink(TurnEvent::AssistantDelta {
            thread_id: self.thread_id.clone(),
            turn_id: self.turn_id.clone(),
            item: ItemRef {
                item_id: self.item_id.clone(),
            },
            delta: delta.to_string(),
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
        if !self.first_delta_observed || self.assistant_terminal_generated {
            return;
        }
        self.assistant_terminal_generated = true;
        sink(TurnEvent::ItemFailed {
            thread_id: self.thread_id.clone(),
            turn_id: self.turn_id.clone(),
            item: ItemRef {
                item_id: self.item_id.clone(),
            },
            error: SAFE_ASSISTANT_ITEM_FAILURE.to_string(),
        });
    }

    pub(crate) fn emit_assistant_terminal_completed(&mut self, sink: &mut dyn FnMut(TurnEvent)) {
        if !self.first_delta_observed || self.assistant_terminal_generated {
            return;
        }
        self.assistant_terminal_generated = true;
        sink(TurnEvent::ItemCompleted {
            thread_id: self.thread_id.clone(),
            turn_id: self.turn_id.clone(),
            item: ItemRef {
                item_id: self.item_id.clone(),
            },
        });
    }
}
