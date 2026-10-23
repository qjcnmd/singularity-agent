//! 当前操作中还没保存的正文、工具进度和压缩状态；读取任务时叠加到已保存的历史上，
//! 对应内容保存后就从这里移除。
use singularity_protocol::{
    HistoryItem, HistoryMessageRole, ThreadHistory, ToolStatus, TurnEvent, TurnStatus,
};

#[derive(Default)]
pub(super) struct Activity {
    turn_id: Option<String>,
    items: Vec<HistoryItem>,
}
impl Activity {
    pub(super) fn turn_id(&self) -> Option<String> {
        self.turn_id.clone()
    }
    pub(super) fn apply(&mut self, event: &TurnEvent) {
        match event {
            TurnEvent::TurnStarted { turn, .. } => self.turn_id = Some(turn.turn_id.clone()),
            TurnEvent::TextDelta { item_id, thinking, delta } => {
                if let Some(HistoryItem::Message { text, .. } | HistoryItem::Thinking { text, .. }) =
                    self.items.iter_mut().find(|item| item.id() == item_id)
                {
                    text.push_str(delta);
                } else {
                    self.items.push(if *thinking {
                        HistoryItem::Thinking {
                            id: item_id.clone(),
                            text: delta.clone(),
                            status: TurnStatus::Running,
                        }
                    } else {
                        HistoryItem::Message {
                            id: item_id.clone(),
                            text: delta.clone(),
                            role: HistoryMessageRole::Assistant,
                            images: Vec::new(),
                            status: TurnStatus::Running,
                        }
                    });
                }
            }
            TurnEvent::ItemUpdated {
                item: item @ (HistoryItem::Tool { .. } | HistoryItem::CompactionResult { .. }),
            } => {
                upsert(&mut self.items, item.clone());
            }
            TurnEvent::ItemUpdated { item: HistoryItem::Compaction { id, .. } }
            | TurnEvent::ToolFinished { item_id: id, .. } => self.items.retain(|item| item.id() != id),
            TurnEvent::MessageFinished { message_id, .. } | TurnEvent::MessageDiscarded { message_id } => {
                let text = singularity_agent::session::text_item_id(message_id, 0);
                let thinking = singularity_agent::session::thinking_item_id(message_id, 0);
                self.items.retain(|item| item.id() != text && item.id() != thinking);
            }
            TurnEvent::ToolOutput { item_id, output } => {
                if let Some(HistoryItem::Tool { result, .. }) =
                    self.items.iter_mut().find(|item| item.id() == item_id)
                {
                    *result = Some(singularity_protocol::ToolResult {
                        output: output.clone(),
                        images: Vec::new(),
                        read_source: None,
                        is_error: false,
                        duration_ms: None,
                    });
                }
            }
            _ => {}
        }
    }
    pub(super) fn merge(&self, page: &mut ThreadHistory) {
        if let Some(turn) = page.turns.last_mut() {
            if self.turn_id.as_ref() == Some(&turn.turn_id) && turn.finished_at.is_none() {
                turn.status = TurnStatus::Running;
                // 日志没有执行中的状态；当前回合尚无结果的调用仍在等待，运行中的进度随后覆盖它。
                for item in &mut turn.items {
                    if let HistoryItem::Tool { status, result: None, .. } = item {
                        *status = ToolStatus::Pending;
                    }
                }
            }
            for item in &self.items {
                upsert(&mut turn.items, item.clone());
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rereading_active_tools_distinguishes_waiting_from_interrupted_history() {
        let mut page = ThreadHistory {
            summary: singularity_protocol::ThreadSummary {
                thread_id: "session".into(),
                cwd: "C:/project".into(),
                created_at: String::new(),
                updated_at: String::new(),
                title: None,
                status: None,
                manually_stopped: false,
            },
            turns: vec![singularity_protocol::ThreadTurn {
                turn_id: "turn".into(),
                status: TurnStatus::Interrupted,
                started_at: "2026-10-23T00:00:00.000Z".into(),
                finished_at: None,
                error: None,
                manually_stopped: false,
                items: (0..9)
                    .map(|index| HistoryItem::Tool {
                        id: index.to_string(),
                        name: "read".into(),
                        args: serde_json::json!({}),
                        status: ToolStatus::Interrupted,
                        result: None,
                    })
                    .collect(),
            }],
        };
        let saved = page.clone();
        let mut activity = Activity {
            turn_id: Some("turn".into()),
            ..Default::default()
        };
        for item in &page.turns[0].items[..8] {
            let mut item = item.clone();
            if let HistoryItem::Tool { status, .. } = &mut item {
                *status = ToolStatus::Running;
            }
            activity.apply(&TurnEvent::ItemUpdated { item });
        }
        activity.merge(&mut page);
        for (index, item) in page.turns[0].items.iter().enumerate() {
            let HistoryItem::Tool { status, .. } = item else {
                unreachable!()
            };
            assert_eq!(
                *status,
                if index < 8 {
                    ToolStatus::Running
                } else {
                    ToolStatus::Pending
                }
            );
        }
        let mut reopened = saved.clone();
        Activity::default().merge(&mut reopened);
        assert_eq!(reopened, saved);
    }
}
