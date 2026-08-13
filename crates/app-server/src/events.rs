//! Event subscription, delivery metadata, output sequencing, and recovery gaps.

use super::*;

impl AppServer {
    pub(super) fn sequence_outputs(
        &mut self,
        messages: Vec<Value>,
    ) -> AppServerResult<Vec<AppServerOutput>> {
        let mut outputs: Vec<AppServerOutput> = Vec::with_capacity(messages.len());
        let mut subscription_cursor = None;
        for message in messages {
            let output = match sequence_output(&self.output_order, message) {
                Ok(output) => output,
                Err(error) => {
                    for output in &outputs {
                        self.output_order.complete(output.reservation.order);
                    }
                    return Err(error);
                }
            };
            if output.message["method"] == "event/gap" {
                subscription_cursor = output.reservation.event_cursor;
            }
            outputs.push(output);
        }
        if let Some(cursor) = subscription_cursor {
            for output in &mut outputs {
                if output.message["result"]["subscriptionId"] == EVENT_SUBSCRIPTION_ID
                    && output.message["result"]["cursor"] == 0
                {
                    output.message["result"]["cursor"] = cursor.into();
                }
            }
        }
        Ok(outputs)
    }

    pub(super) fn event_subscribe(
        &mut self,
        message: JsonRpcMessage,
    ) -> AppServerResult<Vec<Value>> {
        let params: EventSubscribeParams = parse_params(&message)?;
        let current_cursor = self
            .output_order
            .current_event_cursor()
            .map_err(AppServerError::Workspace)?;
        let gap = {
            let mut state = self.event_filter.lock().map_err(|_| {
                AppServerError::Workspace("event subscription state poisoned".into())
            })?;
            if params.cursor == Some(0)
                || params.cursor.is_some_and(|cursor| cursor > current_cursor)
            {
                return Err(AppServerError::InvalidParams(
                    "event subscription cursor is outside the observed range".to_string(),
                ));
            }
            state.event_types = Some(params.event_types.clone());
            let from_cursor = params.cursor.map_or(1, |cursor| cursor.saturating_add(1));
            EventGap {
                reason: EventGapReason::CursorNotReplayed,
                from_cursor,
                to_cursor: 0,
            }
        };
        let mut messages = vec![self.event_gap_notification(gap)?];
        messages.extend(json_response(
            message.required_id(),
            EventSubscribeResult {
                subscription_id: EVENT_SUBSCRIPTION_ID.to_string(),
                event_types: params.event_types,
                cursor: 0,
            },
        )?);
        Ok(messages)
    }

    pub(super) fn event_notification(&self, event: AppEvent) -> AppServerResult<Option<Value>> {
        let state = self
            .event_filter
            .lock()
            .map_err(|_| AppServerError::Workspace("event subscription state poisoned".into()))?;
        let Some(event_types) = state.event_types.as_ref() else {
            return Ok(None);
        };
        if !event_types.iter().any(|method| method == event.method()) {
            return Ok(None);
        }
        let (class, delivery, recovery_query) = event_contract(&event);
        Ok(Some(
            event
                .to_notification_with_metadata(EventMetadata {
                    sequence: 0,
                    cursor: 0,
                    class,
                    delivery,
                    recovery_query,
                    gap: None,
                })
                .to_wire_value(),
        ))
    }

    pub(super) fn event_gap_notification(&self, gap: EventGap) -> AppServerResult<Value> {
        Ok(AppEvent {
            method: "event/gap".to_string(),
            params: json!({"gap": gap.clone()}),
        }
        .to_notification_with_metadata(EventMetadata {
            sequence: 0,
            cursor: 0,
            class: EventClass::Gap,
            delivery: EventDelivery::Gap,
            recovery_query: None,
            gap: Some(gap),
        })
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
        if let Some(event) = self.event_notification(AppEvent::turn_completed(&committed.turn))? {
            messages.push(event);
        }
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
            if let Some(message) = self.event_notification(event)? {
                messages.push(message);
            }
        }
        Ok(messages)
    }

    /// 仅在 realtime item 已经通过过滤器出现时构造脱敏失败事件。
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

    /// 把 assistant response 的 delta 投影到同一预分配 item，并记录过滤器实际生成的部分。
    pub(super) fn project_assistant_delta(
        &self,
        assistant_events: &mut AssistantItemEventState,
        delta: &str,
    ) -> AppServerResult<Vec<Value>> {
        let mut messages = Vec::new();
        if !assistant_events.first_delta_observed {
            assistant_events.first_delta_observed = true;
            if let Some(event) =
                self.event_notification(AppEvent::item_started(assistant_events.item_id.as_str()))?
            {
                assistant_events.started_generated = true;
                messages.push(event);
            }
        }
        if let Some(event) = self.event_notification(AppEvent::item_agent_message_delta(
            assistant_events.item_id.as_str(),
            delta,
        ))? {
            assistant_events.delta_generated = true;
            messages.push(event);
        }
        Ok(messages)
    }
}
