//! 单会话重连快照：一个任务的受保护状态、活动事件折叠与终态归并。
//!
//! Workbench 负责查找 slot、范围检查、启动操作与全局发布；本模块只维护
//! 单个会话自己的状态及其投影，不读会话目录、不发事件，也不持有工作台。
//! 状态字段只在这里读写，调用方通过方法与捕获结构取得一致投影。

use std::sync::{Arc, Mutex};

use singularity_protocol::{
    ActiveCompactionSnapshot, ActiveTurnRuntimeSnapshot, SessionRuntime, SessionTerminalSnapshot,
    TurnEvent, WorkbenchTurnEvent,
};
use singularity_runtime::{Conversation, ThreadSnapshot};

pub(super) struct ConversationSlot {
    conversation: Arc<Conversation>,
    state: Mutex<SlotState>,
}

struct ActiveTurn {
    turn_id: String,
    events: Vec<WorkbenchTurnEvent>,
    started_at: String,
}

pub(super) struct SlotState {
    history: Option<Arc<ThreadSnapshot>>,
    session_revision: u64,
    active_turn: Option<ActiveTurn>,
    active_compaction: Option<ActiveCompactionSnapshot>,
    terminal: Option<SessionTerminalSnapshot>,
}

/// 一次会话读取的一致捕获：history 截止点、活动事件与运行态取自同一受保护
/// 状态，锁外只做分页。
pub(super) struct SessionCapture {
    pub(super) history: Arc<ThreadSnapshot>,
    pub(super) runtime: SessionRuntime,
    pub(super) active_events: Vec<WorkbenchTurnEvent>,
}

#[allow(clippy::expect_used)]
impl ConversationSlot {
    pub(super) fn new(conversation: Arc<Conversation>) -> Self {
        Self {
            conversation,
            state: Mutex::new(SlotState {
                history: None,
                session_revision: 0,
                active_turn: None,
                active_compaction: None,
                terminal: None,
            }),
        }
    }

    pub(super) fn conversation(&self) -> &Arc<Conversation> {
        &self.conversation
    }

    pub(super) fn lock_state(&self) -> std::sync::MutexGuard<'_, SlotState> {
        self.state
            .lock()
            .expect("conversation slot lock poisoned (fail-stop)")
    }

    /// 测试注入点：让状态锁中毒，模拟「panic 发生在持有 slot 锁时」。
    #[cfg(test)]
    pub(super) fn poison_state(&self) {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = self.state.lock().expect("slot lock");
            panic!("poison the conversation slot lock");
        }));
    }

    /// 一次读取的一致捕获：history 截止点、运行态与活动事件都取自同一受保护
    /// 状态，调用方不必再自行组合三项。
    pub(super) fn capture(
        &self,
        state: &SlotState,
        history: Arc<ThreadSnapshot>,
    ) -> SessionCapture {
        SessionCapture {
            history,
            runtime: self.runtime_from(state),
            active_events: state.active_events().to_vec(),
        }
    }

    pub(super) fn runtime_from(&self, state: &SlotState) -> SessionRuntime {
        // 会话侧字段来自同一次读取；Slot 自己的状态仍由本方法补充。
        let conversation = self.conversation.snapshot();
        SessionRuntime {
            session_revision: state.session_revision,
            phase: conversation.phase,
            selector: conversation.selector,
            model_context_window: conversation.model_context_window,
            pending_controls: conversation.pending_controls,
            active_turn: state
                .active_turn
                .as_ref()
                .map(|active| ActiveTurnRuntimeSnapshot {
                    turn_id: active.turn_id.clone(),
                    started_at: active.started_at.clone(),
                }),
            active_compaction: state.active_compaction.clone(),
            terminal: state.terminal.clone(),
        }
    }
}

impl SlotState {
    /// 开始一次回合：冻结最新持久化 history，清掉上一次的终态与活动回合。
    /// 调用方必须在同一次加锁内完成发布，事件与读取才看到同一个瞬间。
    pub(super) fn begin_turn(&mut self, history: Arc<ThreadSnapshot>) {
        self.history = Some(history);
        self.active_turn = None;
        self.terminal = None;
    }

    /// 开始一次独立压缩：与回合一样冻结 history，并留下活动压缩标记。
    pub(super) fn begin_compaction(&mut self, history: Arc<ThreadSnapshot>, started_at: String) {
        self.begin_turn(history);
        self.active_compaction = Some(ActiveCompactionSnapshot { started_at });
    }

    /// 折叠一条回合事件：推进会话 revision、维护活动回合并替换已完成内容。
    /// 返回需要广播的 envelope；调用方只负责按自己的顺序发出。
    pub(super) fn apply_turn_event(&mut self, event: TurnEvent) -> WorkbenchTurnEvent {
        self.session_revision += 1;
        if let TurnEvent::TurnStarted { turn, started_at } = &event {
            let active = self.active_turn.get_or_insert_with(|| ActiveTurn {
                turn_id: turn.turn_id.clone(),
                events: Vec::new(),
                started_at: started_at.clone(),
            });
            active.turn_id = turn.turn_id.clone();
            active.started_at = started_at.clone();
        }
        let envelope = WorkbenchTurnEvent {
            event,
            session_revision: self.session_revision,
        };
        if let Some(active) = self.active_turn.as_mut() {
            // 恢复快照中已完成内容替换其进度；实时广播仍为增量。
            let replaced = match &envelope.event {
                TurnEvent::ToolExecutionUpdate { turn_id, item, .. }
                | TurnEvent::ToolExecutionEnd { turn_id, item, .. }
                | TurnEvent::ItemCompleted {
                    turn_id,
                    item,
                    content: Some(_),
                    ..
                }
                | TurnEvent::ItemFailed {
                    turn_id,
                    item,
                    content: Some(_),
                    ..
                } => Some((turn_id, &item.item_id)),
                _ => None,
            };
            if let Some((turn, item_id)) = replaced {
                active.events.retain(|previous| {
                    let progress = match &previous.event {
                        TurnEvent::ToolExecutionUpdate { turn_id, item, .. }
                        | TurnEvent::AssistantDelta { turn_id, item, .. }
                        | TurnEvent::AssistantThinkingDelta { turn_id, item, .. }
                        | TurnEvent::ItemStarted { turn_id, item, .. } => {
                            Some((turn_id, &item.item_id))
                        }
                        _ => None,
                    };
                    progress != Some((turn, item_id))
                });
            }
            active.events.push(envelope.clone());
        }
        envelope
    }

    /// 结算：终态来自执行链的可信提交，同时清空活动回合与冻结 history
    /// （后者强制下一次读取重新取盘），并推进会话 revision。
    pub(super) fn settle(&mut self, terminal: Option<SessionTerminalSnapshot>) {
        self.active_compaction = None;
        self.terminal = terminal;
        self.active_turn = None;
        self.history = None;
        self.session_revision += 1;
    }

    /// 发布用 revision 推进：控制处置等状态变化也算一次会话投影更新。
    pub(super) fn bump_revision(&mut self) {
        self.session_revision = self.session_revision.saturating_add(1);
    }

    #[cfg(test)]
    pub(super) fn revision(&self) -> u64 {
        self.session_revision
    }

    pub(super) fn frozen_history(&self) -> Option<Arc<ThreadSnapshot>> {
        self.history.clone()
    }

    pub(super) fn active_events(&self) -> &[WorkbenchTurnEvent] {
        self.active_turn
            .as_ref()
            .map(|active| active.events.as_slice())
            .unwrap_or_default()
    }

    #[cfg(test)]
    pub(super) fn has_active_turn(&self) -> bool {
        self.active_turn.is_some()
    }
}
