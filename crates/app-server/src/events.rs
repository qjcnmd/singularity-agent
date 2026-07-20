//! Event subscription, delivery metadata, output sequencing, and recovery gaps.

use super::*;

impl AppServer {
    pub(super) fn sequence_outputs(
        &self,
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

    pub(super) fn pending_approval_events_for_turn(
        &self,
        turn_id: &str,
    ) -> AppServerResult<Vec<Value>> {
        let approvals = self
            .store
            .list_pending_approvals()?
            .into_iter()
            .filter(|request| request.turn_id == turn_id);
        let mut messages = Vec::new();
        for request in approvals {
            if let Some(message) =
                self.event_notification(AppEvent::approval_requested(&request))?
            {
                messages.push(message);
            }
        }
        Ok(messages)
    }

    pub(super) fn committed_turn_events(
        &self,
        committed: &CommittedTurnOutcome,
    ) -> AppServerResult<Vec<Value>> {
        let mut messages = self.agent_terminal_item_events(committed.plan_item.as_ref())?;
        if let Some(plan_item) = committed.plan_item.as_ref()
            && let Some(event) = self.event_notification(AppEvent::turn_plan_updated(
                &committed.turn.turn_id,
                plan_item.payload.clone(),
            ))?
        {
            messages.push(event);
        }
        messages.extend(self.agent_terminal_item_events(committed.assistant_item.as_ref())?);
        if let Some(event) = self.event_notification(AppEvent::turn_completed(&committed.turn))? {
            messages.push(event);
        }
        Ok(messages)
    }

    pub(super) fn agent_terminal_item_events(
        &self,
        item: Option<&Item>,
    ) -> AppServerResult<Vec<Value>> {
        let Some(agent_item) = item else {
            return Ok(Vec::new());
        };
        let mut events = vec![AppEvent::item_started(agent_item.item_id.clone())];
        match &agent_item.kind {
            singularity_protocol::ItemKind::Plan => {}
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
                events.push(AppEvent::item_agent_message_delta(
                    agent_item.item_id.clone(),
                    agent_delta,
                ));
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
}
