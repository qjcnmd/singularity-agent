//! Event delivery metadata and lifecycle event projection.
//!
//! stdio 单连接传输下事件随请求响应全量发送；协议中不存在 `event/subscribe`
//! 或订阅状态，客户端把 matching response 之前的 notification 关联到本次请求。

use super::*;
use singularity_agent::{
    message::{AgentMessageRole, ContentBlock},
    session::{SessionEntry, SessionEntryType, SessionMetadataKind},
};
use std::collections::HashMap;

const SAFE_ASSISTANT_ITEM_FAILURE: &str = "assistant response failed";

/// 一次 AgentLoop 调用预分配的 assistant item 事件状态（只用于实时协议事件）。
pub(super) struct AssistantItemEventState {
    pub(super) thread_id: String,
    pub(super) turn_id: String,
    pub(super) item_id: String,
    first_delta_observed: bool,
    started_generated: bool,
    delta_generated: bool,
    assistant_terminal_generated: bool,
    pub(super) tool_items: HashMap<String, bool>,
}

impl AssistantItemEventState {
    pub(super) fn new(thread_id: String, turn_id: String, item_id: String) -> Self {
        Self {
            thread_id,
            turn_id,
            item_id,
            first_delta_observed: false,
            started_generated: false,
            delta_generated: false,
            assistant_terminal_generated: false,
            tool_items: HashMap::new(),
        }
    }

    pub(super) fn appeared(&self) -> bool {
        self.started_generated || self.delta_generated
    }

    pub(super) fn start_tool_item(&mut self, tool_call_id: &str) -> bool {
        if self.tool_items.contains_key(tool_call_id) {
            return false;
        }
        self.tool_items.insert(tool_call_id.to_string(), false);
        true
    }

    pub(super) fn open_tool_items(&self) -> Vec<String> {
        self.tool_items
            .iter()
            .filter_map(|(id, terminal)| (!*terminal).then_some(id.clone()))
            .collect()
    }
}

fn event_contract(event: &AppEvent) -> (EventClass, EventDelivery) {
    match event.method.as_str() {
        "item/agentMessage/delta" => (EventClass::Progress, EventDelivery::BestEffort),
        _ => (EventClass::State, EventDelivery::Reliable),
    }
}

/// 将内部 SessionEntry 转成稳定的公开 history item。该边界只复制用户可见的
/// message/thinking/tool/turn/settings/usage/compaction 字段，绝不序列化原始 entry
/// 或其 `provider_reasoning_replay`、parent/tree、迁移字段。
pub(crate) fn project_public_history(entry: &SessionEntry) -> Vec<HistoryItem> {
    match &entry.entry_type {
        SessionEntryType::Message(message) => match message.role {
            AgentMessageRole::User | AgentMessageRole::Assistant => {
                let role = match message.role {
                    AgentMessageRole::User => "user",
                    AgentMessageRole::Assistant => "assistant",
                    _ => unreachable!(),
                }
                .to_string();
                let mut items = Vec::new();
                let mut text_index = 0usize;
                let mut thinking_index = 0usize;
                for block in &message.content {
                    match block {
                        ContentBlock::Text { text } if !text.is_empty() => {
                            items.push(HistoryItem::Message {
                                id: format!("{}:text:{text_index}", entry.id),
                                role: role.clone(),
                                text: text.clone(),
                            });
                            text_index += 1;
                        }
                        ContentBlock::Thinking { thinking, .. } if !thinking.is_empty() => {
                            items.push(HistoryItem::Thinking {
                                id: format!("{}:thinking:{thinking_index}", entry.id),
                                text: thinking.clone(),
                            });
                            thinking_index += 1;
                        }
                        ContentBlock::ToolCall { id, name, args } => {
                            items.push(HistoryItem::ToolCall {
                                id: id.clone(),
                                name: name.clone(),
                                args: args.clone(),
                            });
                        }
                        _ => {}
                    }
                }
                items
            }
            AgentMessageRole::ToolResult => vec![HistoryItem::ToolResult {
                id: message
                    .tool_call_id
                    .clone()
                    .unwrap_or_else(|| entry.id.clone()),
                output: message.content_text(),
                is_error: message.is_error.unwrap_or(false),
            }],
        },
        SessionEntryType::Compaction(compaction) => vec![HistoryItem::Compaction {
            id: entry.id.clone(),
            summary: compaction.summary.clone(),
        }],
        SessionEntryType::Metadata(metadata) => match metadata.kind() {
            SessionMetadataKind::TurnStarted => metadata
                .turn_id()
                .map(|id| HistoryItem::Turn {
                    id: id.to_string(),
                    status: TurnStatus::Running,
                })
                .into_iter()
                .collect(),
            SessionMetadataKind::TurnCompleted
            | SessionMetadataKind::TurnFailed
            | SessionMetadataKind::TurnInterrupted => metadata
                .turn_id()
                .map(|id| HistoryItem::Turn {
                    id: id.to_string(),
                    status: match metadata.kind() {
                        SessionMetadataKind::TurnCompleted => TurnStatus::Completed,
                        SessionMetadataKind::TurnFailed => TurnStatus::Failed,
                        SessionMetadataKind::TurnInterrupted => TurnStatus::Interrupted,
                        _ => unreachable!(),
                    },
                })
                .into_iter()
                .collect(),
            SessionMetadataKind::ThreadSettings => vec![HistoryItem::Settings {
                id: entry.id.clone(),
                provider: metadata.field_string("provider").map(str::to_string),
                model: metadata.field_string("model").map(str::to_string),
                reasoning: metadata.field_string("reasoning").map(str::to_string),
            }],
            SessionMetadataKind::Usage => metadata
                .field("usage")
                .cloned()
                .map(|usage| HistoryItem::Usage {
                    id: entry.id.clone(),
                    usage,
                })
                .into_iter()
                .collect(),
        },
    }
}

impl AppServer {
    /// 将应用事件包装为带类型化元数据的 JSON-RPC notification。
    pub(super) fn event_notification(&self, event: AppEvent) -> AppServerResult<Value> {
        if self.consume_terminal_event_failure(&event.method) {
            return Err(AppServerError::TurnExecution {
                stage: TurnFailureStage::EventNotification,
                cause: TurnFailureCause::Internal,
                original: Some(format!("injected terminal event failure: {}", event.method)),
            });
        }
        let (class, delivery) = event_contract(&event);
        Ok(event
            .to_notification_with_metadata(EventMetadata { class, delivery })
            .to_wire_value())
    }

    /// 仅在 realtime item 已经出现过时构造脱敏失败事件。
    pub(super) fn realtime_item_failed_event(
        &self,
        assistant_events: &mut AssistantItemEventState,
    ) -> AppServerResult<Option<Value>> {
        if !assistant_events.appeared() || assistant_events.assistant_terminal_generated {
            return Ok(None);
        }
        let event = self.event_notification(AppEvent::item_failed(
            &assistant_events.thread_id,
            &assistant_events.turn_id,
            &assistant_events.item_id,
            SAFE_ASSISTANT_ITEM_FAILURE,
        ))?;
        assistant_events.assistant_terminal_generated = true;
        Ok(Some(event))
    }

    pub(super) fn realtime_item_completed_event(
        &self,
        assistant_events: &mut AssistantItemEventState,
    ) -> AppServerResult<Option<Value>> {
        if !assistant_events.appeared() || assistant_events.assistant_terminal_generated {
            return Ok(None);
        }
        let event = self.event_notification(AppEvent::item_completed(
            &assistant_events.thread_id,
            &assistant_events.turn_id,
            &assistant_events.item_id,
        ))?;
        assistant_events.assistant_terminal_generated = true;
        Ok(Some(event))
    }

    pub(super) fn realtime_tool_terminal_event(
        &self,
        assistant_events: &mut AssistantItemEventState,
        tool_call_id: &str,
        is_error: bool,
    ) -> AppServerResult<Option<Value>> {
        if assistant_events
            .tool_items
            .get(tool_call_id)
            .is_none_or(|terminal| *terminal)
        {
            return Ok(None);
        }
        let event = if is_error {
            AppEvent::item_failed(
                &assistant_events.thread_id,
                &assistant_events.turn_id,
                tool_call_id,
                "tool execution failed",
            )
        } else {
            AppEvent::item_completed(
                &assistant_events.thread_id,
                &assistant_events.turn_id,
                tool_call_id,
            )
        };
        let event = self.event_notification(event)?;
        if let Some(terminal) = assistant_events.tool_items.get_mut(tool_call_id) {
            *terminal = true;
        }
        Ok(Some(event))
    }

    /// 把 assistant response 的 delta 投影到同一预分配 item，并记录实际生成的部分。
    pub(super) fn project_assistant_delta(
        &self,
        assistant_events: &mut AssistantItemEventState,
        delta: &str,
    ) -> AppServerResult<Vec<Value>> {
        let mut messages = Vec::new();
        if !assistant_events.first_delta_observed {
            assistant_events.first_delta_observed = true;
            assistant_events.started_generated = true;
            messages.push(self.event_notification(AppEvent::item_started(
                &assistant_events.thread_id,
                &assistant_events.turn_id,
                &assistant_events.item_id,
            ))?);
        }
        assistant_events.delta_generated = true;
        messages.push(self.event_notification(AppEvent::item_agent_message_delta(
            &assistant_events.thread_id,
            &assistant_events.turn_id,
            &assistant_events.item_id,
            delta,
        ))?);
        Ok(messages)
    }
}
