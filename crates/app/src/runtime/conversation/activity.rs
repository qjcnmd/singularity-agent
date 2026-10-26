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
                            at: None,
                            stats: None,
                            role: HistoryMessageRole::Assistant,
                            images: Vec::new(),
                            files: Vec::new(),
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
                match turn.items.iter_mut().find(|saved| saved.id() == item.id()) {
                    // 保存先于完成事件；只给尚无结果的工具补运行进度，其他已保存内容优先。
                    Some(saved @ HistoryItem::Tool { result: None, .. }) => *saved = item.clone(),
                    Some(_) => {}
                    None => turn.items.push(item.clone()),
                }
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
