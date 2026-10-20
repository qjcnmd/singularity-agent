//! 只保留当前操作的显示内容。重读会话时直接合并条目，不重放事件。

use singularity_protocol::{
    HistoryItem, HistoryMessageRole, ThreadReadPage, ThreadTurn, TurnEvent, TurnStatus,
};

#[derive(Default)]
pub(super) struct Activity {
    turn: Option<ThreadTurn>,
    standalone: Vec<HistoryItem>,
}

impl Activity {
    pub(super) fn apply(&mut self, event: &TurnEvent) {
        if let TurnEvent::TurnStarted { turn, started_at } = event {
            self.turn = Some(ThreadTurn {
                turn_id: Some(turn.turn_id.clone()),
                started_at: Some(started_at.clone()),
                finished_at: None,
                status: Some(TurnStatus::Running),
                manually_stopped: false,
                error: None,
                items: Vec::new(),
            });
            return;
        }
        let items = self.turn.as_mut().map_or(&mut self.standalone, |turn| &mut turn.items);
        match event {
            TurnEvent::UserMessage { item, text, images, .. } => upsert(
                items,
                HistoryItem::Message {
                    id: item.item_id.clone(),
                    role: HistoryMessageRole::User,
                    text: text.clone(),
                    images: images.clone(),
                    running: false,
                },
            ),
            TurnEvent::AssistantDelta { item, delta, .. }
            | TurnEvent::AssistantThinkingDelta { item, delta, .. } => {
                let previous = items.iter_mut().find(|entry| entry.id() == item.item_id);
                if let Some(HistoryItem::Message { text, .. } | HistoryItem::Thinking { text, .. }) = previous
                {
                    text.push_str(delta);
                } else {
                    upsert(
                        items,
                        if matches!(event, TurnEvent::AssistantDelta { .. }) {
                            HistoryItem::Message {
                                id: item.item_id.clone(),
                                role: HistoryMessageRole::Assistant,
                                text: delta.clone(),
                                images: Vec::new(),
                                running: true,
                            }
                        } else {
                            HistoryItem::Thinking {
                                id: item.item_id.clone(),
                                text: delta.clone(),
                                running: true,
                            }
                        },
                    );
                }
            }
            TurnEvent::ToolExecutionStart { item, tool_name, args, .. } => upsert(
                items,
                HistoryItem::Tool {
                    id: item.item_id.clone(),
                    name: tool_name.clone(),
                    args: args.clone(),
                    result: None,
                    running: true,
                },
            ),
            TurnEvent::ToolExecutionUpdate { item, partial_result, .. } => {
                if let Some(HistoryItem::Tool { result, .. }) =
                    items.iter_mut().find(|entry| entry.id() == item.item_id)
                {
                    *result = Some(singularity_protocol::ToolResult {
                        output: partial_result.clone(),
                        images: Vec::new(),
                        read_source: None,
                        is_error: false,
                        duration_ms: None,
                    });
                }
            }
            TurnEvent::ToolExecutionEnd { item, result: output, .. } => {
                if let Some(HistoryItem::Tool { result, running, .. }) =
                    items.iter_mut().find(|entry| entry.id() == item.item_id)
                {
                    *result = Some(output.clone());
                    *running = false;
                }
            }
            TurnEvent::ItemCompleted { content: Some(content), .. }
            | TurnEvent::ItemFailed { content: Some(content), .. }
            | TurnEvent::ContextInjected { content, .. }
            | TurnEvent::SettingsUpdated { content, .. }
            | TurnEvent::CompactionUpdated { content, .. } => upsert(items, content.clone()),
            TurnEvent::ItemDiscarded { item, .. } => items.retain(|entry| entry.id() != item.item_id),
            TurnEvent::ProviderAttempt { observation, .. } => {
                upsert(items, HistoryItem::Request { observation: observation.clone() })
            }
            TurnEvent::TurnCompleted { turn, manually_stopped, finished_at } => {
                if let Some(active) = &mut self.turn {
                    active.status = Some(turn.status);
                    active.manually_stopped = *manually_stopped;
                    active.finished_at = Some(finished_at.clone());
                }
            }
            TurnEvent::TurnFailed { error, finished_at, .. } => {
                if let Some(active) = &mut self.turn {
                    active.status = Some(TurnStatus::Failed);
                    active.error = Some(error.clone());
                    active.finished_at = Some(finished_at.clone());
                }
            }
            _ => {}
        }
    }

    pub(super) fn merge(&self, page: &mut ThreadReadPage) {
        if let Some(active) = &self.turn {
            if let Some(turn) = page.turns.iter_mut().find(|turn| turn.turn_id == active.turn_id) {
                *turn = active.clone();
            } else {
                page.turns.push(active.clone());
            }
        }
        if !self.standalone.is_empty() {
            if page.turns.is_empty() {
                page.turns.push(ThreadTurn {
                    turn_id: None,
                    started_at: None,
                    finished_at: None,
                    status: None,
                    manually_stopped: false,
                    error: None,
                    items: Vec::new(),
                });
            }
            let items = &mut page.turns.last_mut().expect("leading group exists").items;
            for item in &self.standalone {
                upsert(items, item.clone());
            }
        }
    }
}

fn upsert(items: &mut Vec<HistoryItem>, item: HistoryItem) {
    if let Some(previous) = items.iter_mut().find(|previous| previous.id() == item.id()) {
        *previous = item;
    } else {
        items.push(item);
    }
}
