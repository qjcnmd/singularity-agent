//! 单个会话的重连快照：这个任务的受保护状态、活动事件折叠和终态归并。
//! 查找 slot、范围检查、启动操作和全局发布归 AppServer；本模块不读会话目录、不发事件、
//! 不持有工作台，状态字段只在这里读写。

use std::sync::{Arc, Mutex};

use super::activity::Activity;
use singularity_protocol::{SessionRuntime, SessionTerminalSnapshot, TurnEvent, TurnEventEnvelope};
use singularity_runtime::Conversation;

pub(super) struct ConversationSlot {
    conversation: Arc<Conversation>,
    state: Mutex<SlotState>,
}

pub(super) struct SlotState {
    archiving: bool,
    session_revision: u64,
    activity: Activity,
    terminal: Option<SessionTerminalSnapshot>,
    usage: Option<singularity_protocol::SessionModelUsage>,
}

impl ConversationSlot {
    pub(super) fn new(conversation: Arc<Conversation>) -> Self {
        Self {
            conversation,
            state: Mutex::new(SlotState {
                archiving: false,
                session_revision: 0,
                activity: Activity::default(),
                terminal: None,
                usage: None,
            }),
        }
    }

    pub(super) fn conversation(&self) -> &Arc<Conversation> {
        &self.conversation
    }

    pub(super) fn lock_state(&self) -> std::sync::MutexGuard<'_, SlotState> {
        self.state.lock().expect("conversation slot lock poisoned (fail-stop)")
    }

    pub(super) fn is_archiving(&self) -> bool {
        self.lock_state().archiving
    }

    /// 调用方持有生命周期锁；归档期间保留读取和事件投影，拒绝新的操作。
    pub(super) fn begin_archive(self: &Arc<Self>) -> ArchiveGuard {
        self.lock_state().archiving = true;
        self.conversation.set_closing(true);
        ArchiveGuard { slot: Arc::clone(self) }
    }

    pub(super) fn runtime_from(&self, state: &SlotState) -> SessionRuntime {
        // 会话侧的字段来自同一次读取；Slot 自己的状态由本方法补上。
        let conversation = self.conversation.snapshot();
        SessionRuntime {
            session_revision: state.session_revision,
            phase: conversation.phase,
            selector: conversation.selector,
            model_context_window: conversation.model_context_window,
            pending_question: conversation.pending_question,
            pending_messages: conversation.pending_messages,
            terminal: state.terminal.clone(),
            usage: state.usage.clone(),
            background_tasks: self.conversation.background_tasks(),
        }
    }
}

/// 归档失败时释放操作限制，原任务仍可继续使用或重试归档。
pub(super) struct ArchiveGuard {
    slot: Arc<ConversationSlot>,
}

impl Drop for ArchiveGuard {
    fn drop(&mut self) {
        self.slot.lock_state().archiving = false;
        self.slot.conversation.set_closing(false);
    }
}

impl SlotState {
    pub(super) fn background_error(&mut self, message: String) {
        self.terminal = Some(SessionTerminalSnapshot {
            source: singularity_protocol::SessionTerminalSource::Turn,
            status: singularity_protocol::TurnStatus::Failed,
            manually_stopped: false,
            error: Some(singularity_protocol::TurnErrorDetail {
                cause: singularity_protocol::TurnFailureCause::Store,
                message,
                item_id: None,
            }),
        });
    }

    /// 开始一次操作：读取已保存用量，清掉上一次显示状态。
    /// 调用方在同一次加锁内完成发布，事件和读取才看到同一个瞬间。
    pub(super) fn begin_turn(&mut self, usage: singularity_protocol::SessionModelUsage) {
        self.usage = Some(usage);
        self.activity = Activity::default();
        self.terminal = None;
    }

    /// 折叠一条执行事件：推进会话 revision，并用完整内容替换相应的进度记录。
    /// 返回要广播的 envelope，调用方只负责按自己的顺序发出去。
    pub(super) fn apply_turn_event(
        &mut self,
        event: TurnEvent,
        phase: singularity_protocol::SessionPhase,
    ) -> TurnEventEnvelope {
        self.bump_revision();
        let usage = if let TurnEvent::ProviderAttempt { observation, .. } = &event {
            let usage = self.usage.as_mut().expect("execution starts with saved usage");
            singularity_runtime::accumulate_request_usage(usage, observation);
            Some(usage.clone())
        } else {
            None
        };
        let envelope = TurnEventEnvelope {
            event,
            session_revision: self.session_revision,
            phase,
            usage,
        };
        if phase != singularity_protocol::SessionPhase::Idle {
            self.activity.apply(&envelope.event);
        }
        envelope
    }

    /// 结算：保存本次执行结果或错误反馈，清空当前操作条目，
    /// 让下一次读取使用会话内存记录；同时推进会话 revision。
    pub(super) fn settle(&mut self, terminal: Option<SessionTerminalSnapshot>) {
        self.terminal = terminal;
        self.activity = Activity::default();
        self.bump_revision();
    }

    /// 推进发布用的 revision：控制处置这类状态变化同样算一次会话投影更新。
    pub(super) fn bump_revision(&mut self) {
        self.session_revision += 1;
    }

    pub(super) fn revision(&self) -> u64 {
        self.session_revision
    }

    pub(super) fn merge_activity(&self, page: &mut singularity_protocol::ThreadReadPage) {
        self.activity.merge(page);
    }
}
