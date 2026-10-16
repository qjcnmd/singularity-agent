use std::sync::{Arc, Mutex};

use singularity_agent::agent::{ControlRequest, SteeringInbox, SteeringInboxHandle, UserInput};
use singularity_agent::session::SessionWriter;
use singularity_protocol::{SessionPhase, Thread};
use tokio_util::sync::CancellationToken;

use super::ConversationControlError;

struct CancelState {
    accepting: bool,
    compacting: bool,
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
            cancel_state: Mutex::new(CancelState { accepting: true, compacting: false }),
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

    /// 把已经创建好的控制请求放进本轮注入箱；注入窗口已经关闭时拒绝。请求的身份和接受
    /// 序号由 Conversation 在生命周期的临界区内生成。
    pub(crate) fn enqueue(&self, request: ControlRequest) -> bool {
        self.lock_inbox().enqueue(request)
    }

    /// 接受一次停止：取消本轮并关闭注入窗口，两者在同一个受保护边界内完成。停止之后到达的
    /// steer 一律被拒绝，尚未消费的引导输入被清空。
    pub(super) fn accept_cancel(&self) -> Result<(), ConversationControlError> {
        {
            let state = self.cancel_state.lock().expect("cancel window lock poisoned");
            if state.compacting {
                return Err(ConversationControlError::Compacting);
            }
            if !state.accepting {
                return Err(ConversationControlError::NotRunning);
            }
            self.cancellation.cancel();
        }
        self.lock_inbox().cancel();
        Ok(())
    }

    /// 提交终态前关闭停止入口；已接受的停止必然计入本轮结果。
    pub(crate) fn finish_cancel(&self) -> bool {
        let mut state = self.cancel_state.lock().expect("cancel window lock poisoned");
        state.accepting = false;
        self.cancellation.is_cancelled()
    }

    /// 执行结束后关闭注入窗口，后续操作由会话的空闲状态处理。
    pub(crate) fn close_inbox(&self) {
        self.lock_inbox().close();
    }

    pub(super) fn lock_inbox(&self) -> std::sync::MutexGuard<'_, SteeringInbox> {
        self.inbox.lock().expect("turn inbox lock poisoned (fail-stop)")
    }
}

pub(super) struct ConversationState {
    pub(super) thread: Thread,
    pub(super) turn: TurnLifecycle,
    /// 会话持有尚未消费的 steer；输入箱跨执行保留，各轮只借用它的接受窗口。
    pub(super) steering_inbox: SteeringInboxHandle,
    /// 普通提交和 steer 共用的接受序号，保证继续执行时较早的 steer 先于新输入交付。
    pub(super) control_sequence: u64,
    /// 最近一次执行冻结下来的上下文窗口，进程重启后丢失。
    pub(super) last_context_window: Option<u64>,
}

impl ConversationState {
    /// 生成下一个控制请求。接受序号推进一次，消息身份在接受时分配；空输入不占用序号。
    pub(super) fn next_control(
        &mut self,
        input: UserInput,
    ) -> Result<ControlRequest, ConversationControlError> {
        super::validate_input(&input)?;
        let sequence = self.control_sequence;
        self.control_sequence = sequence + 1;
        Ok(ControlRequest::new(sequence, input))
    }
}

pub(super) enum TurnLifecycle {
    Idle,
    Reserved,
    Running(Arc<TurnControls>),
    Compacting { writer: SessionWriter },
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
            Self::Running(controls) if controls.is_compacting() => SessionPhase::Compacting,
            Self::Running(controls) if controls.cancellation().is_cancelled() => SessionPhase::Stopping,
            Self::Running(_) => SessionPhase::Running,
            Self::Compacting { .. } => SessionPhase::Compacting,
        }
    }

    /// 已经打开的会话写者；空闲和预订阶段没有写者，由调用方临时开一个。
    pub(super) fn writer(&self) -> Option<SessionWriter> {
        match self {
            Self::Running(controls) => Some(controls.writer()),
            Self::Compacting { writer, .. } => Some(Arc::clone(writer)),
            Self::Idle | Self::Reserved => None,
        }
    }
}
