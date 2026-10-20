use std::sync::{Arc, Mutex};

use singularity_agent::agent::{SteeringInbox, SteeringInboxHandle};
use singularity_agent::session::SessionWriter;
use singularity_protocol::{SessionPhase, Thread};
use tokio_util::sync::CancellationToken;

use super::ConversationError;

struct CancelState {
    accepting: bool,
    compacting: bool,
    user_stopped: bool,
}

/// 一轮执行的输入、问答和停止控制；接受停止与冻结结果共用同一把锁。
pub(crate) struct TurnControls {
    pub(crate) turn_id: String,
    cancellation: CancellationToken,
    cancel_state: Mutex<CancelState>,
    pub(crate) inbox: SteeringInboxHandle,
    pub(crate) questions: Arc<singularity_agent::agent::UserQuestions>,
    writer: SessionWriter,
}

impl TurnControls {
    pub fn new(turn_id: impl Into<String>, inbox: SteeringInboxHandle, writer: SessionWriter) -> Self {
        Self {
            turn_id: turn_id.into(),
            cancellation: CancellationToken::new(),
            cancel_state: Mutex::new(CancelState {
                accepting: true,
                compacting: false,
                user_stopped: false,
            }),
            inbox,
            questions: Arc::new(singularity_agent::agent::UserQuestions::default()),
            writer,
        }
    }

    pub(crate) fn set_compacting(&self, active: bool) {
        self.cancel_state.lock().expect("cancel window lock poisoned").compacting = active;
    }

    pub(crate) fn is_compacting(&self) -> bool {
        self.cancel_state.lock().expect("cancel window lock poisoned").compacting
    }

    pub(crate) fn cancellation(&self) -> &CancellationToken {
        &self.cancellation
    }

    pub(crate) fn inbox_handle(&self) -> SteeringInboxHandle {
        Arc::clone(&self.inbox)
    }

    /// 本轮共享的会话写者（runner 和协调器的控制路径共用）。
    pub(crate) fn writer(&self) -> SessionWriter {
        Arc::clone(&self.writer)
    }

    /// 接受一次停止：取消本轮并清空未消费的用户输入。停止之后到达的
    /// steer 一律被拒绝，尚未消费的引导输入被清空。
    pub(super) fn accept_cancel(&self) -> Result<(), ConversationError> {
        {
            let mut state = self.cancel_state.lock().expect("cancel window lock poisoned");
            if !state.accepting {
                return Err(ConversationError::Busy);
            }
            state.user_stopped = true;
            self.cancellation.cancel();
        }
        self.lock_inbox().cancel();
        Ok(())
    }

    /// 提交终态前关闭停止入口；已接受的停止必然计入本轮结果。
    pub(crate) fn finish_cancel(&self) -> bool {
        let mut state = self.cancel_state.lock().expect("cancel window lock poisoned");
        state.accepting = false;
        state.user_stopped
    }

    pub(super) fn lock_inbox(&self) -> std::sync::MutexGuard<'_, SteeringInbox> {
        self.inbox.lock().expect("turn inbox lock poisoned (fail-stop)")
    }
}

pub(super) struct ConversationState {
    pub(super) thread: Thread,
    pub(super) turn: TurnLifecycle,
    /// 首条和追加消息共用的输入箱；跨执行保留，各轮只借用它的接受窗口。
    pub(super) steering_inbox: SteeringInboxHandle,
    /// 最近一次执行冻结下来的上下文窗口，进程重启后丢失。
    pub(super) last_context_window: Option<u64>,
    pub(super) auto_resume: bool,
    pub(super) closing: bool,
}

impl ConversationState {
    /// 回合交接与后台唤醒共用续跑规则；暂停或关闭时不自动消费输入。
    pub(super) fn has_pending_resume(&self) -> bool {
        self.auto_resume
            && !self.closing
            && !self.steering_inbox.lock().expect("turn inbox lock poisoned").pending().is_empty()
    }
}

pub(super) enum TurnLifecycle {
    Idle,
    Reserved,
    Running(Arc<TurnControls>),
    Compacting { cancellation: CancellationToken },
}

impl TurnLifecycle {
    pub(super) fn is_busy(&self) -> bool {
        !matches!(self, Self::Idle)
    }

    /// 执行状态直接取自操作窗口和它的取消令牌，客户端只是把它投影出去。
    pub(super) fn phase(&self) -> SessionPhase {
        match self {
            Self::Idle => SessionPhase::Idle,
            Self::Reserved => SessionPhase::Reserved,
            Self::Running(controls) if controls.cancellation().is_cancelled() => SessionPhase::Stopping,
            Self::Running(controls) if controls.is_compacting() => SessionPhase::Compacting,
            Self::Running(_) => SessionPhase::Running,
            Self::Compacting { cancellation } if cancellation.is_cancelled() => SessionPhase::Stopping,
            Self::Compacting { .. } => SessionPhase::Compacting,
        }
    }
}
