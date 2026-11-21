use std::sync::{Arc, Mutex};

use agent::agent::SteeringInboxHandle;
use protocol::{ModelSelection, SessionPhase};
use tokio_util::sync::CancellationToken;

use super::ConversationError;

struct CancelState {
    accepting: bool,
    rotating: bool,
    user_stopped: bool,
}

/// 一次操作（回合或独立轮换）的提问和停止控制；输入队列和写者由任务持有。
pub(super) struct TurnControls {
    pub(super) turn_id: String,
    cancellation: CancellationToken,
    cancel_state: Mutex<CancelState>,
    pub(super) questions: Arc<agent::agent::UserQuestions>,
}

impl TurnControls {
    pub(super) fn new(turn_id: String) -> Self {
        Self {
            turn_id,
            cancellation: CancellationToken::new(),
            cancel_state: Mutex::new(CancelState {
                accepting: true,
                rotating: false,
                user_stopped: false,
            }),
            questions: Arc::new(agent::agent::UserQuestions::default()),
        }
    }

    pub(super) fn set_rotating(&self, active: bool) {
        self.cancel_state.lock().expect("cancel window lock poisoned").rotating = active;
    }

    pub(super) fn is_rotating(&self) -> bool {
        self.cancel_state.lock().expect("cancel window lock poisoned").rotating
    }

    pub(super) fn cancellation(&self) -> &CancellationToken {
        &self.cancellation
    }

    /// 接受停止并取消本次操作；调用方在持有会话状态锁时同时清空排队的用户输入。
    pub(super) fn accept_cancel(&self) -> Result<(), ConversationError> {
        {
            let mut state = self.cancel_state.lock().expect("cancel window lock poisoned");
            if !state.accepting {
                return Err(ConversationError::Busy);
            }
            state.user_stopped = true;
            self.cancellation.cancel();
        }
        Ok(())
    }

    /// 保存最终状态前关闭停止入口，并返回用户是否停止过；已接受的停止一定计入本轮结果。
    pub(super) fn finish_cancel(&self) -> bool {
        let mut state = self.cancel_state.lock().expect("cancel window lock poisoned");
        state.accepting = false;
        state.user_stopped
    }
}

pub(super) struct ConversationState {
    pub(super) model: ModelSelection,
    pub(super) turn: TurnLifecycle,
    /// 首条和后续消息共用的输入队列，跨回合保留。
    pub(super) steering_inbox: SteeringInboxHandle,
    /// 当前任务模型的上下文窗口，打开任务时从配置读取，每轮开始时按实际解析结果更新。
    pub(super) last_context_window: Option<u64>,
    pub(super) auto_resume: bool,
    pub(super) closing: bool,
    /// 尚未完成的上下文预览；关闭和新执行取消它，关闭等待其收尾。
    pub(super) context_preview: Option<CancellationToken>,
}

impl ConversationState {
    pub(super) fn has_work(&self, bash_tasks: &agent::tools::BashTasks) -> bool {
        self.turn.is_busy()
            || self.context_preview.is_some()
            || bash_tasks.is_busy()
            || !self.steering_inbox.lock().expect("turn inbox lock poisoned").pending().is_empty()
    }

    /// 回合结束和后台通知共用的继续条件；暂停或关闭时不自动处理输入。
    pub(super) fn has_pending_resume(&self) -> bool {
        self.auto_resume
            && !self.closing
            && !self.steering_inbox.lock().expect("turn inbox lock poisoned").pending().is_empty()
    }
}

/// 回合与独立轮换共用同一种控制；轮换标记区分两者的显示阶段。
pub(super) enum TurnLifecycle {
    Idle,
    Running(Arc<TurnControls>),
}

impl TurnLifecycle {
    pub(super) fn is_busy(&self) -> bool {
        !matches!(self, Self::Idle)
    }

    /// 运行阶段由当前操作的停止和轮换标记决定，前端直接显示。
    pub(super) fn phase(&self) -> SessionPhase {
        match self {
            Self::Idle => SessionPhase::Idle,
            Self::Running(controls) if controls.cancellation().is_cancelled() => SessionPhase::Stopping,
            Self::Running(controls) if controls.is_rotating() => SessionPhase::Rotating,
            Self::Running(_) => SessionPhase::Running,
        }
    }
}
