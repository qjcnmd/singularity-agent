//! Event delivery metadata and lifecycle event projection.
//!
//! stdio 单连接传输下事件随请求响应全量发送；协议中不存在 `event/subscribe`
//! 或订阅状态，客户端把 matching response 之前的 notification 关联到本次请求。

use super::*;

impl AppServer {
    /// 将应用事件包装为带类型化元数据的 JSON-RPC notification。
    pub(super) fn event_notification(&self, event: AppEvent) -> AppServerResult<Value> {
        let (class, delivery) = event_contract(&event);
        Ok(event
            .to_notification_with_metadata(EventMetadata { class, delivery })
            .to_wire_value())
    }

    /// 仅在 realtime item 已经出现过时构造脱敏失败事件。
    pub(super) fn realtime_item_failed_event(
        &self,
        assistant_events: &AssistantItemEventState,
    ) -> AppServerResult<Option<Value>> {
        if !assistant_events.appeared() {
            return Ok(None);
        }
        self.event_notification(AppEvent::item_failed(
            assistant_events.item_id.clone(),
            SAFE_ASSISTANT_ITEM_FAILURE,
        ))
        .map(Some)
    }

    /// 向当前有序输出路径追加可见 realtime item 的可靠失败终态。
    pub(super) fn emit_realtime_item_failure(
        &self,
        emit: &mut impl FnMut(Value),
        assistant_events: Option<&AssistantItemEventState>,
    ) -> AppServerResult<()> {
        if let Some(assistant_events) = assistant_events
            && let Some(event) = self.realtime_item_failed_event(assistant_events)?
        {
            emit(event);
        }
        Ok(())
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
