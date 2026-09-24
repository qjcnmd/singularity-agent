use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use singularity_agent::agent::{ControlRequest, TurnInbox, TurnInboxHandle, control_id};
use singularity_agent::session::SessionWriter;
use singularity_protocol::{
    ControlChannel, ControlDisposition, ControlSnapshot, SessionPhase, Thread,
};
use tokio_util::sync::CancellationToken;

use super::ConversationControlError;

/// 停止接受窗口：接受一次停止和冻结「是否接受过停止」在同一个临界点完成；普通 turn 与独立
/// 压缩共用同一套规则——冻结边界之前接受的停止进入终态裁决，边界之后一律报告操作已结束。
/// 它不引入新的状态机，只是承载已有的取消令牌和接受标志。
pub(crate) struct CancelWindow {
    pub(crate) cancellation: CancellationToken,
    accepting: Mutex<bool>,
}

#[allow(clippy::expect_used)]
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
    pub(crate) inbox: TurnInboxHandle,
    writer: SessionWriter,
    /// 本轮冻结下来的模型有效上下文窗口；start_turn 解析之前是 None。
    context_window: std::sync::OnceLock<u64>,
}

#[allow(clippy::expect_used)]
impl TurnControls {
    pub fn new(turn_id: impl Into<String>, inbox: TurnInboxHandle, writer: SessionWriter) -> Self {
        Self {
            turn_id: turn_id.into(),
            window: CancelWindow::new(),
            inbox,
            writer,
            context_window: std::sync::OnceLock::new(),
        }
    }

    pub(crate) fn cancellation(&self) -> &CancellationToken {
        &self.window.cancellation
    }

    /// 记录本轮冻结模型的有效上下文窗口（由 runner 在解析完成后调用一次）。
    pub(crate) fn record_context_window(&self, window: u64) {
        let _ = self.context_window.set(window);
    }

    pub(crate) fn context_window(&self) -> Option<u64> {
        self.context_window.get().copied()
    }

    pub(crate) fn inbox_handle(&self) -> TurnInboxHandle {
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

    /// 在同一个临界区里整批放进本轮注入箱；窗口已关闭时整批都不交付。
    pub(crate) fn enqueue_all(&self, requests: Vec<ControlRequest>) -> bool {
        self.lock_inbox().enqueue_all(requests)
    }

    /// 接受一次停止：取消本轮，同时关闭新的注入窗口。停止之后到达的 steer 一律被
    /// 拒绝；已经排队的输入保持原位（它们属于下一轮，不属于本轮的取消集合）。
    /// 接受停止和关闭注入窗口在同一个受保护的边界内完成。
    pub(super) fn accept_cancel(&self) -> Result<(), ConversationControlError> {
        self.window.accept()?;
        self.lock_inbox().close();
        Ok(())
    }

    pub(crate) fn finish_cancel(&self) -> bool {
        self.window.freeze()
    }

    /// 关闭注入窗口，并把里面剩下的控制请求交给 Runner。
    pub(crate) fn finish_inbox(&self) -> Vec<ControlRequest> {
        let mut inbox = self.lock_inbox();
        inbox.close();
        inbox.drain()
    }

    pub(super) fn lock_inbox(&self) -> std::sync::MutexGuard<'_, TurnInbox> {
        self.inbox
            .lock()
            .expect("turn inbox lock poisoned (fail-stop)")
    }
}

/// 按 sequence 升序（先进先出）插入已接受的输入；同一个序号不会出现两次，所以插入位置唯一。
pub(super) fn insert_by_sequence(queue: &mut VecDeque<ControlRequest>, input: ControlRequest) {
    let position = queue
        .iter()
        .position(|existing| existing.sequence > input.sequence)
        .unwrap_or(queue.len());
    queue.insert(position, input);
}

/// 按 control_id 定位还没被消费的待执行输入；找不到这个身份时统一报 ControlNotFound。
pub(super) fn locate_pending_input(
    queue: &VecDeque<ControlRequest>,
    control_id: &str,
) -> Result<usize, ConversationControlError> {
    queue
        .iter()
        .position(|input| input.control_id == control_id)
        .ok_or(ConversationControlError::ControlNotFound)
}

pub(super) struct ConversationState {
    pub(super) thread: Thread,
    pub(super) turn: TurnLifecycle,
    /// 还没开始执行的待处理输入，按接受序号排队；channel 只记录输入从哪个入口被接受，不代表
    /// 它现在是否还在等待——普通提交、follow-up 和被 runner 归还的未消费 steer 都在这里。
    pub(super) pending_inputs: VecDeque<ControlRequest>,
    /// steer 和 follow_up 共用的接受序号：控制身份和先进先出顺序都在这里统一推进。
    pub(super) control_sequence: u64,
    /// 最近一次执行冻结下来的上下文窗口；进程重启后就无从得知了。
    pub(super) last_context_window: Option<u64>,
}

impl ConversationState {
    pub(super) fn editable_pending_position(
        &self,
        control_id: &str,
    ) -> Result<usize, ConversationControlError> {
        let position = locate_pending_input(&self.pending_inputs, control_id)?;
        match self.turn {
            TurnLifecycle::Reserved | TurnLifecycle::Compacting { .. } => {
                Err(ConversationControlError::NotRunning)
            }
            TurnLifecycle::Idle | TurnLifecycle::Running(_) => Ok(position),
        }
    }

    pub(super) fn is_occupied(&self) -> bool {
        self.turn.is_busy() || !self.pending_inputs.is_empty()
    }

    /// 当前执行（或最近一次执行）冻结的有效上下文窗口；空闲之后仍保留最近一次执行的事实。
    pub(super) fn model_context_window(&self) -> Option<u64> {
        let window = match &self.turn {
            TurnLifecycle::Running(controls) => controls.context_window(),
            _ => None,
        };
        window.or(self.last_context_window)
    }

    /// 返回还没开始执行的待处理输入，处置一律为 Pending；channel 原样保留在控制事实里，
    /// 待处理集合只由这份快照决定。
    pub(super) fn pending_controls(&self) -> Vec<ControlSnapshot> {
        self.pending_inputs
            .iter()
            .map(|request| request.snapshot(ControlDisposition::Pending))
            .collect()
    }

    /// 生成下一个控制请求：接受序号在这里推进一次，身份由 channel 和序号唯一确定，正文为空
    /// 不占用序号。`turn_id` 只在输入确实绑定到某个 turn 时给出（注入活动 turn 的 steer），
    /// 等待自己那一轮的排队输入不会借用当前活动 turn 的身份。
    pub(super) fn next_control(
        &mut self,
        channel: ControlChannel,
        turn_id: Option<String>,
        text: String,
    ) -> Result<ControlRequest, ConversationControlError> {
        if text.trim().is_empty() {
            return Err(ConversationControlError::InvalidInput);
        }
        let sequence = self.control_sequence;
        self.control_sequence = sequence + 1;
        Ok(ControlRequest {
            control_id: control_id(channel, sequence),
            turn_id,
            channel,
            sequence,
            text,
        })
    }

    /// 排队一条后续 turn 的输入，保留它的身份和接受序号；排队本身不需要写者。
    pub(super) fn queue_follow_up(
        &mut self,
        text: String,
    ) -> Result<ControlSnapshot, ConversationControlError> {
        if self.turn.active().is_none() {
            return Err(ConversationControlError::NotRunning);
        }
        let request = self.next_control(ControlChannel::FollowUp, None, text)?;
        let snapshot = request.snapshot(ControlDisposition::Pending);
        insert_by_sequence(&mut self.pending_inputs, request);
        Ok(snapshot)
    }
}

pub(super) enum TurnLifecycle {
    Idle,
    Reserved,
    Running(Arc<TurnControls>),
    Compacting {
        thread: Thread,
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
            Self::Running(controls) if controls.cancellation().is_cancelled() => {
                SessionPhase::Stopping
            }
            Self::Running(_) => SessionPhase::Running,
            Self::Compacting { window, .. } if window.cancellation.is_cancelled() => {
                SessionPhase::Stopping
            }
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

    /// 测试用的观察入口：活动 turn 的控制面句柄。
    #[cfg(test)]
    pub(super) fn controls(&self) -> Option<Arc<TurnControls>> {
        match self {
            Self::Running(controls) => Some(Arc::clone(controls)),
            _ => None,
        }
    }
}
