//! Event delivery metadata and lifecycle event projection.
//!
//! 单 worker 传输下事件不做类型过滤（全量发）、无 cursor/gap；`event/subscribe`
//! 保留为协议合同方法，只确认订阅并返回结果。

use super::*;

impl AppServer {
    pub(super) fn event_subscribe(
        &mut self,
        message: JsonRpcMessage,
    ) -> AppServerResult<Vec<Value>> {
        let params: EventSubscribeParams = parse_params(&message)?;
        json_response(
            message.required_id(),
            EventSubscribeResult {
                subscription_id: EVENT_SUBSCRIPTION_ID.to_string(),
                event_types: params.event_types,
                cursor: 0,
            },
        )
    }

    /// 将应用事件包装为带类型化元数据的 JSON-RPC notification。
    pub(super) fn event_notification(&self, event: AppEvent) -> AppServerResult<Value> {
        let (class, delivery) = event_contract(&event);
        Ok(event
            .to_notification_with_metadata(EventMetadata { class, delivery })
            .to_wire_value())
    }

    pub(super) fn committed_turn_events(
        &self,
        committed: &CommittedTurnOutcome,
        assistant_events: Option<&AssistantItemEventState>,
    ) -> AppServerResult<Vec<Value>> {
        let mut messages = Vec::new();
        messages.extend(
            self.agent_terminal_item_events(committed.assistant_item.as_ref(), assistant_events)?,
        );
        if committed.assistant_item.is_none()
            && let Some(assistant_events) = assistant_events
            && let Some(event) = self.realtime_item_failed_event(assistant_events)?
        {
            messages.push(event);
        }
        messages.push(self.event_notification(AppEvent::turn_completed(
            &self.turn_with_usage(committed.turn.clone()),
        ))?);
        Ok(messages)
    }

    pub(super) fn agent_terminal_item_events(
        &self,
        item: Option<&Item>,
        assistant_events: Option<&AssistantItemEventState>,
    ) -> AppServerResult<Vec<Value>> {
        let Some(agent_item) = item else {
            return Ok(Vec::new());
        };
        if let Some(assistant_events) = assistant_events
            && assistant_events.item_id.as_str() != agent_item.item_id
        {
            return Err(StoreError::InvalidState(
                "committed assistant item ID does not match its realtime allocation".to_string(),
            )
            .into());
        }
        let mut events = Vec::new();
        if !assistant_events.is_some_and(|events| events.started_generated) {
            events.push(AppEvent::item_started(agent_item.item_id.clone()));
        }
        match &agent_item.kind {
            singularity_protocol::ItemKind::AgentMessage => {
                let agent_delta = agent_item
                    .payload
                    .get("delta")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        StoreError::InvalidState(
                            "committed assistant item is missing its string delta".to_string(),
                        )
                    })?;
                if !assistant_events.is_some_and(|events| events.delta_generated) {
                    events.push(AppEvent::item_agent_message_delta(
                        agent_item.item_id.clone(),
                        agent_delta,
                    ));
                }
            }
            _ => {
                return Err(StoreError::InvalidState(format!(
                    "unsupported committed terminal item kind: {:?}",
                    agent_item.kind
                ))
                .into());
            }
        }
        events.push(AppEvent::item_completed(agent_item.item_id.clone()));
        let mut messages = Vec::new();
        for event in events {
            messages.push(self.event_notification(event)?);
        }
        Ok(messages)
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
            assistant_events.item_id.as_str(),
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
            messages.push(self.event_notification(AppEvent::item_started(
                assistant_events.item_id.as_str(),
            ))?);
        }
        assistant_events.delta_generated = true;
        messages.push(self.event_notification(AppEvent::item_agent_message_delta(
            assistant_events.item_id.as_str(),
            delta,
        ))?);
        Ok(messages)
    }
}
