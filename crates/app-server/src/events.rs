//! Event delivery metadata and lifecycle event projection.
//!
//! stdio 单连接传输下事件随请求响应全量发送；协议中不存在 `event/subscribe`
//! 或订阅状态，客户端把 matching response 之前的 notification 关联到本次请求。

use super::*;

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
            assistant_events.item_id.clone(),
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
        let event =
            self.event_notification(AppEvent::item_completed(assistant_events.item_id.clone()))?;
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
            AppEvent::item_failed(tool_call_id, "tool execution failed")
        } else {
            AppEvent::item_completed(tool_call_id)
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
            messages.push(
                self.event_notification(AppEvent::item_started(assistant_events.item_id.clone()))?,
            );
        }
        assistant_events.delta_generated = true;
        messages.push(self.event_notification(AppEvent::item_agent_message_delta(
            assistant_events.item_id.clone(),
            delta,
        ))?);
        Ok(messages)
    }
}
