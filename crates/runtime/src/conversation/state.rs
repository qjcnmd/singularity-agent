use std::sync::{Arc, Mutex};

use singularity_agent::agent::{ControlRequest, SteeringInbox, SteeringInboxHandle, UserInput};
use singularity_agent::session::SessionWriter;
use singularity_protocol::{SessionPhase, Thread};
use tokio_util::sync::CancellationToken;

use super::ConversationControlError;

/// 停止接受窗口：接受一次停止和冻结「是否接受过停止」在同一个临界点完成；普通 turn 与独立
/// 压缩共用同一套规则——冻结边界之前接受的停止进入终态裁决，边界之后一律报告操作已结束。
/// 它不引入新的状态机，只是承载已有的取消令牌和接受标志。
pub(crate) struct CancelWindow {
    pub(crate) cancellation: CancellationToken,
    accepting: Mutex<bool>,
}

impl CancelWindow {
    pub(super) fn new() -> Self {
        Self {
            cancellation: CancellationToken::new(),
            accepting: Mutex::new(true),
        }
    }

    /// 接受一次停止；接受窗口一旦冻结就返回 NotRunning，冻结之前重复停止没有副作用。
    /// 写取消标记和读 `accepting` 在同一临界区内完成、guard 到函数结束才释放，所以冻结线程
    /// 看不到「已通过接受检查、取消标记还没写入」的中间状态：accept 返回 Ok 的停止必然进入
    /// 这次冻结的结果。
    pub(super) fn accept(&self) -> Result<(), ConversationControlError> {
        let accepting = self.accepting.lock().expect("cancel window lock poisoned");
        if !*accepting {
            return Err(ConversationControlError::NotRunning);
        }
        self.cancellation.cancel();
        Ok(())
    }

    /// 在提交终态之前冻结「是否接受过停止」，并返回这次操作的结果。
    pub(crate) fn freeze(&self) -> bool {
        let mut accepting = self.accepting.lock().expect("cancel window lock poisoned");
        *accepting = false;
        self.cancellation.is_cancelled()
    }
}

/// 一个活动 turn 的控制集合；只有在设置变更时才会共享它的写者。
pub(crate) struct TurnControls {
    pub(crate) turn_id: String,
    window: CancelWindow,
    pub(crate) inbox: SteeringInboxHandle,
    pub(crate) questions: Arc<singularity_agent::agent::UserQuestions>,
    writer: SessionWriter,
}

impl TurnControls {
    pub fn new(turn_id: impl Into<String>, inbox: SteeringInboxHandle, writer: SessionWriter) -> Self {
        Self {
            turn_id: turn_id.into(),
            window: CancelWindow::new(),
            inbox,
            questions: Arc::new(singularity_agent::agent::UserQuestions::default()),
            writer,
        }
    }

    pub(crate) fn cancellation(&self) -> &CancellationToken {
        &self.window.cancellation
    }

    pub(crate) fn inbox_handle(&self) -> SteeringInboxHandle {
        Arc::clone(&self.inbox)
    }

    /// 本轮共享的会话写者（runner 和协调器的控制路径共用）。
    pub(crate) fn writer(&self) -> SessionWriter {
        Arc::clone(&self.writer)
    }

    /// 把已经创建好的控制请求放进本轮注入箱；注入窗口已经关闭时拒绝。
    /// 请求的身份和接受序号由 Conversation 在生命周期的临界区内生成。
    pub(crate) fn enqueue(&self, request: ControlRequest) -> bool {
        self.lock_inbox().enqueue(request)
    }

    /// 接受一次停止：取消本轮，同时关闭新的注入窗口。停止之后到达的 steer 一律被
    /// 拒绝；已经排队的输入保持原位（它们属于下一轮，不属于本轮的取消集合）。
    /// 接受停止和关闭注入窗口在同一个受保护的边界内完成。
    pub(super) fn accept_cancel(&self) -> Result<(), ConversationControlError> {
        self.window.accept()?;
        self.lock_inbox().cancel();
        Ok(())
    }

    pub(crate) fn finish_cancel(&self) -> bool {
        self.window.freeze()
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
    /// 等待当前执行结束的唯一后续输入，与已发送的 steer 分开持有。
    pub(super) pending_input: Option<ControlRequest>,
    /// 会话持有尚未消费的 steer；输入箱跨执行保留，各轮只借用它的接受窗口。
    pub(super) steering_inbox: SteeringInboxHandle,
    /// steer 和 follow_up 共用的接受序号：控制身份和先进先出顺序都在这里统一推进。
    pub(super) control_sequence: u64,
    /// 最近一次执行冻结下来的上下文窗口；进程重启后就无从得知了。
    pub(super) last_context_window: Option<u64>,
}

impl ConversationState {
    pub(super) fn editable_pending_input(
        &self,
        control_id: &str,
    ) -> Result<&ControlRequest, ConversationControlError> {
        let input = self
            .pending_input
            .as_ref()
            .filter(|input| input.control_id() == control_id)
            .ok_or(ConversationControlError::ControlNotFound)?;
        match self.turn {
            TurnLifecycle::Reserved => Err(ConversationControlError::NotRunning),
            TurnLifecycle::Idle | TurnLifecycle::Running(_) | TurnLifecycle::Compacting { .. } => Ok(input),
        }
    }

    pub(super) fn is_occupied(&self) -> bool {
        self.turn.is_busy() || self.pending_input.is_some()
    }

    /// 返回唯一排队输入的只读投影。
    pub(super) fn pending_input(&self) -> Option<singularity_protocol::PendingInput> {
        self.pending_input.as_ref().map(ControlRequest::pending)
    }

    /// 生成下一个控制请求：接受序号在这里推进一次，身份由序号唯一确定，正文为空
    /// 不占用序号。
    pub(super) fn next_control(
        &mut self,
        input: UserInput,
    ) -> Result<ControlRequest, ConversationControlError> {
        super::validate_input(&input)?;
        let sequence = self.control_sequence;
        self.control_sequence = sequence + 1;
        Ok(ControlRequest { sequence, input })
    }

    /// 排队一条后续 turn 的输入，保留它的身份和接受序号；排队本身不需要写者。
    pub(super) fn queue_follow_up(&mut self, input: UserInput) -> Result<(), ConversationControlError> {
        if self.turn.active().is_none() {
            return Err(ConversationControlError::NotRunning);
        }
        if self.pending_input.is_some() {
            return Err(ConversationControlError::PendingInputExists);
        }
        let request = self.next_control(input)?;
        self.pending_input = Some(request);
        Ok(())
    }
}

pub(super) enum TurnLifecycle {
    Idle,
    Reserved,
    Running(Arc<TurnControls>),
    Compacting {
        writer: SessionWriter,
        window: Arc<CancelWindow>,
    },
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
            Self::Running(_) => SessionPhase::Running,
            Self::Compacting { window, .. } if window.cancellation.is_cancelled() => SessionPhase::Stopping,
            Self::Compacting { .. } => SessionPhase::Compacting,
        }
    }

    /// 借用当前活动 turn 的控制面；临界区内的读取和转发从这里取，只有确实要越过锁范围时才克隆句柄。
    pub(super) fn active(&self) -> Option<&TurnControls> {
        match self {
            Self::Running(controls) => Some(controls),
            Self::Idle | Self::Reserved | Self::Compacting { .. } => None,
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
