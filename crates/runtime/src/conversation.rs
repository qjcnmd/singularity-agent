//! 一个 session 的内存队列，以及它唯一的活动执行窗口。
//! 已经被消费的输入由 Agent 落盘；还在等待处理的输入只活在进程里，进程结束就没了。
//!
//! # 锁
//!
//! 两个锁各管一件事，加锁顺序固定为「写者窗口 → 状态」，不会互相反向等待：
//!
//! - `writer_window`：会话写者的打开、Running→Reserved 的交接和设置写盘都在这里互斥；
//!   打开写者要解析整份会话并做崩溃修复，所以这一段不占着状态锁。
//! - `state`：线程设置、活动阶段、控制接受顺序和待处理输入；控制面的读取
//!   （steer/abort/snapshot/phase）只取它，不会被写者的 I/O 挡住。
//!
//! # 锁失效策略
//!
//! 锁中毒表示共享状态已经不可信，直接 panic 结束进程，不降级继续运行。

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use singularity_agent::agent::{ControlRequest, TurnInbox, TurnInboxHandle, control_id};
use singularity_agent::session::{SessionWriter, lock_writer};
use singularity_core::CancellationToken;
use singularity_protocol::{ControlChannel, ControlDisposition, ControlSnapshot, SessionPhase};
use uuid::Uuid;

use crate::error::TurnRunError;
use crate::runner::{TurnOutcome, TurnRunResult, TurnRunner};
use singularity_protocol::Thread;
use singularity_protocol::TurnEvent;

/// 停止接受窗口：接受一次停止和冻结「是否接受过停止」在同一个临界点完成；普通 turn 与独立
/// 压缩共用同一套规则——冻结边界之前接受的停止进入终态裁决，边界之后一律报告操作已结束。
/// 它不引入新的状态机，只是承载已有的取消令牌和接受标志。
pub(crate) struct CancelWindow {
    pub(crate) cancellation: CancellationToken,
    accepting: Mutex<bool>,
}

#[allow(clippy::expect_used)]
impl CancelWindow {
    fn new() -> Self {
        Self {
            cancellation: CancellationToken::new(),
            accepting: Mutex::new(true),
        }
    }

    /// 接受一次停止；接受窗口一旦冻结就返回 NotRunning，冻结之前重复停止没有副作用。
    /// 写取消标记和读 `accepting` 在同一临界区内完成、guard 到函数结束才释放，所以冻结线程
    /// 看不到「已通过接受检查、取消标记还没写入」的中间状态：accept 返回 Ok 的停止必然进入
    /// 这次冻结的结果。
    fn accept(&self) -> Result<(), ConversationControlError> {
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
    fn accept_cancel(&self) -> Result<(), ConversationControlError> {
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

    fn lock_inbox(&self) -> std::sync::MutexGuard<'_, TurnInbox> {
        self.inbox
            .lock()
            .expect("turn inbox lock poisoned (fail-stop)")
    }
}

/// 按 sequence 升序（先进先出）插入已接受的输入；同一个序号不会出现两次，所以插入位置唯一。
fn insert_by_sequence(queue: &mut VecDeque<ControlRequest>, input: ControlRequest) {
    let position = queue
        .iter()
        .position(|existing| existing.sequence > input.sequence)
        .unwrap_or(queue.len());
    queue.insert(position, input);
}

/// 按 control_id 定位还没被消费的待执行输入；找不到这个身份时统一报 ControlNotFound。
fn locate_pending_input(
    queue: &VecDeque<ControlRequest>,
    control_id: &str,
) -> Result<usize, ConversationControlError> {
    queue
        .iter()
        .position(|input| input.control_id == control_id)
        .ok_or(ConversationControlError::ControlNotFound)
}

struct ConversationState {
    thread: Thread,
    turn: TurnLifecycle,
    /// 还没开始执行的待处理输入，按接受序号排队；channel 只记录输入从哪个入口被接受，不代表
    /// 它现在是否还在等待——普通提交、follow-up 和被 runner 归还的未消费 steer 都在这里。
    pending_inputs: VecDeque<ControlRequest>,
    /// steer 和 follow_up 共用的接受序号：控制身份和先进先出顺序都在这里统一推进。
    control_sequence: u64,
    /// 最近一次执行冻结下来的上下文窗口；进程重启后就无从得知了。
    last_context_window: Option<u64>,
}

impl ConversationState {
    fn editable_pending_position(
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

    fn is_occupied(&self) -> bool {
        self.turn.is_busy() || !self.pending_inputs.is_empty()
    }

    /// 当前执行（或最近一次执行）冻结的有效上下文窗口；空闲之后仍保留最近一次执行的事实。
    fn model_context_window(&self) -> Option<u64> {
        let window = match &self.turn {
            TurnLifecycle::Running(controls) => controls.context_window(),
            _ => None,
        };
        window.or(self.last_context_window)
    }

    /// 返回还没开始执行的待处理输入，处置一律为 Pending；channel 原样保留在控制事实里，
    /// 待处理集合只由这份快照决定。
    fn pending_controls(&self) -> Vec<ControlSnapshot> {
        self.pending_inputs
            .iter()
            .map(|request| request.snapshot(ControlDisposition::Pending))
            .collect()
    }

    /// 生成下一个控制请求：接受序号在这里推进一次，身份由 channel 和序号唯一确定，正文为空
    /// 不占用序号。`turn_id` 只在输入确实绑定到某个 turn 时给出（注入活动 turn 的 steer），
    /// 等待自己那一轮的排队输入不会借用当前活动 turn 的身份。
    fn next_control(
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
    fn queue_follow_up(
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

enum TurnLifecycle {
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
    fn is_busy(&self) -> bool {
        !matches!(self, Self::Idle)
    }

    /// 执行状态直接取自操作窗口和它的取消令牌，客户端只是把它投影出去。
    fn phase(&self) -> SessionPhase {
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
    fn active(&self) -> Option<&TurnControls> {
        match self {
            Self::Running(controls) => Some(controls),
            Self::Idle | Self::Reserved | Self::Compacting { .. } => None,
        }
    }

    /// 已经打开的会话写者；空闲和预订阶段没有写者，由调用方临时开一个。
    fn writer(&self) -> Option<SessionWriter> {
        match self {
            Self::Running(controls) => Some(controls.writer()),
            Self::Compacting { writer, .. } => Some(Arc::clone(writer)),
            Self::Idle | Self::Reserved => None,
        }
    }

    /// 测试用的观察入口：活动 turn 的控制面句柄。
    #[cfg(test)]
    fn controls(&self) -> Option<Arc<TurnControls>> {
        match self {
            Self::Running(controls) => Some(Arc::clone(controls)),
            _ => None,
        }
    }
}

/// 在同一个 state 临界区里读出的会话侧事实：生命周期、模型选择、冻结的上下文窗口
/// 和待处理控制。它是不可变投影，不缓存、不跨调用复用，也不是新的事实来源。
pub struct ConversationSnapshot {
    pub phase: SessionPhase,
    pub selector: Option<String>,
    /// 本轮冻结的有效上下文窗口：用来解释最近请求的用量，不会因为之后编辑配置而
    /// 改变；进程内还没有执行过，或进程重启之后，都是 None。
    pub model_context_window: Option<u64>,
    pub pending_controls: Vec<ControlSnapshot>,
}

/// 一个 Thread 的长驻协调器。
pub struct Conversation {
    runner: Arc<TurnRunner>,
    /// Thread 设置、活动阶段、控制接受顺序和待处理输入由同一把锁协调。
    state: Mutex<ConversationState>,
    /// 会话写者窗口：写者打开、turn 交接和设置写盘的唯一互斥点，见模块文档「锁」。
    writer_window: Mutex<()>,
}

/// 一次执行预订；drop 时把还没用掉的已提升输入还回队列。
pub struct TurnReservation {
    conversation: Arc<Conversation>,
    promoted_input: Option<ControlRequest>,
}

impl TurnReservation {
    /// 执行本轮输入以及后续队列，直到链条结束；窗口一直保持到预订 drop。
    /// 控制处置的变化通过同一个事件出口带类型发布。本轮输入在这里取得控制身份，
    /// 和排队的后续输入共用同一套身份与序号规则。
    pub fn run(
        &mut self,
        input: &str,
        sink: &mut dyn FnMut(TurnEvent),
    ) -> Result<TurnOutcome, ConversationError> {
        // 提升出来的输入有独立的执行入口；这里用错了就直接失败，不悄悄把它丢掉。
        if self.promoted_input.is_some() {
            return Err(ConversationError::Configuration(
                "turn reservation carries a promoted follow-up; run_promoted executes it"
                    .to_string(),
            ));
        }
        self.conversation
            .accept_submission(input.to_string())
            .and_then(|request| self.conversation.run_chain(request, false, sink))
    }

    /// 执行从 pending follow-up 里原子提升出来的输入。它排在队列中其他 follow-up
    /// 之前，并沿用原来的控制身份和接受序号。
    pub fn run_promoted(
        &mut self,
        sink: &mut dyn FnMut(TurnEvent),
    ) -> Result<TurnOutcome, ConversationError> {
        let input = self.promoted_input.take().ok_or_else(|| {
            ConversationError::Configuration(
                "turn reservation does not carry a promoted follow-up".to_string(),
            )
        })?;
        self.conversation.run_chain(input, true, sink)
    }

    /// 在已经预订好的压缩窗口里执行；预订一直持有到调用方完成投影收尾。
    pub fn compact(&mut self) -> Result<crate::CompactionOutcome, ConversationError> {
        let (thread, writer, window) = match &self.conversation.lock_state().turn {
            TurnLifecycle::Compacting {
                thread,
                writer,
                window,
            } => (thread.clone(), Arc::clone(writer), Arc::clone(window)),
            _ => return Err(ConversationError::TurnAlreadyActive),
        };
        self.conversation
            .runner
            .compact_thread(&thread, &window, writer)
            .map_err(ConversationError::Compaction)
    }
}

impl Drop for TurnReservation {
    fn drop(&mut self) {
        let mut state = self.conversation.lock_state();
        if let Some(input) = self.promoted_input.take() {
            insert_by_sequence(&mut state.pending_inputs, input);
        }
        state.turn = TurnLifecycle::Idle;
    }
}

/// 一次「立即发送」原子提升的结果。
pub enum FollowUpPromotion {
    /// 目标集合为空：没有需要交接的输入（「全部发送」遇到空队列）。
    Empty,
    /// 输入已经进入当前 turn 的注入箱，沿用原来的 control 身份。
    Injected,
    /// Session 已经空闲；队首输入从队列转到了独占预订里，其余的按原顺序留在队列中。
    Reserved { reservation: TurnReservation },
}

/// 协调层错误。
#[derive(Debug, thiserror::Error)]
pub enum ConversationError {
    #[error("thread already has an active turn")]
    TurnAlreadyActive,
    #[error("{0}")]
    Configuration(String),
    #[error(transparent)]
    Compaction(#[from] crate::runner::CompactionRunError),
    #[error(transparent)]
    Turn(#[from] TurnRunError),
    #[error(transparent)]
    Session(#[from] singularity_agent::session::SessionError),
}

#[derive(Debug, thiserror::Error)]
pub enum ConversationControlError {
    #[error("session is not running")]
    NotRunning,
    #[error("control text must not be empty")]
    InvalidInput,
    #[error("pending control was not found")]
    ControlNotFound,
}

#[allow(clippy::expect_used)]
impl Conversation {
    /// 建立任务协调器。
    pub fn new(runner: Arc<TurnRunner>, thread: Thread) -> Arc<Self> {
        Arc::new(Self {
            runner,
            state: Mutex::new(ConversationState {
                thread,
                turn: TurnLifecycle::Idle,
                pending_inputs: VecDeque::new(),
                control_sequence: 0,
                last_context_window: None,
            }),
            writer_window: Mutex::new(()),
        })
    }

    /// 原子地预订活动 turn 的链窗口：窗口期间其他预订和 run_turn 立刻被拒绝，窗口可以交给
    /// TurnReservation::run 执行整条链，也可以在 drop 时释放；它属于写者窗口，与写者打开和
    /// 交接在同一处串行。
    pub fn reserve_start(self: &Arc<Self>) -> Result<TurnReservation, ConversationError> {
        let _window = self.lock_writer_window();
        let mut state = self.lock_state();
        if state.turn.is_busy() {
            return Err(ConversationError::TurnAlreadyActive);
        }
        state.turn = TurnLifecycle::Reserved;
        Ok(TurnReservation {
            conversation: Arc::clone(self),
            promoted_input: None,
        })
    }

    #[cfg(test)]
    pub(crate) fn runner_handle(&self) -> Arc<TurnRunner> {
        Arc::clone(&self.runner)
    }

    /// 当前 Thread 的投影快照。
    pub fn thread(&self) -> Thread {
        self.lock_state().thread.clone()
    }

    /// 向活动 turn 注入即时引导输入；没有活动 turn，或注入窗口已经关闭时返回错误。接受检查、
    /// 生成身份和输入入箱都在同一个生命周期临界区内完成，避免跨过收尾窗口。
    pub fn steer(
        &self,
        text: impl Into<String>,
    ) -> Result<ControlSnapshot, ConversationControlError> {
        let mut state = self.lock_state();
        // 正文校验放在确认可以注入之后。
        let Some(turn_id) = state.turn.active().map(|controls| controls.turn_id.clone()) else {
            return Err(ConversationControlError::NotRunning);
        };
        let request = state.next_control(ControlChannel::Steer, Some(turn_id), text.into())?;
        let snapshot = request.snapshot(ControlDisposition::Pending);
        if !state
            .turn
            .active()
            .is_some_and(|controls| controls.enqueue(request))
        {
            return Err(ConversationControlError::NotRunning);
        }
        Ok(snapshot)
    }

    /// 在活动回合之后按先进先出执行输入；空闲时应当直接开始回合。
    pub fn submit_follow_up(
        &self,
        text: impl Into<String>,
    ) -> Result<ControlSnapshot, ConversationControlError> {
        self.lock_state().queue_follow_up(text.into())
    }

    /// 接受一次普通提交：它和排队的后续输入进同一个队列，在这里取得同一套控制身份和接受
    /// 序号，等它开始自己那一轮时才和 turn 关联。
    fn accept_submission(&self, text: String) -> Result<ControlRequest, ConversationError> {
        self.lock_state()
            .next_control(ControlChannel::Submit, None, text)
            .map_err(|error| ConversationError::Configuration(error.to_string()))
    }

    /// 修改还没被消费的输入，保留它的身份、接受序号和队列位置。
    pub fn replace_follow_up(
        &self,
        control_id: &str,
        text: impl Into<String>,
    ) -> Result<ControlSnapshot, ConversationControlError> {
        let text = text.into();
        if text.trim().is_empty() {
            return Err(ConversationControlError::InvalidInput);
        }
        let mut state = self.lock_state();
        let position = state.editable_pending_position(control_id)?;
        let request = &mut state.pending_inputs[position];
        request.text = text;
        Ok(request.snapshot(ControlDisposition::Pending))
    }

    /// 立即发送：把目标 pending 输入原子地提升为当前 turn 的输入，空闲时提升为下一条独占
    /// 执行预订；省略 `target` 表示全部待处理输入。读取目标、判定注入窗口和转移所有权共用
    /// 状态锁与当前 turn 的 inbox 锁，调用方不必按自己读到的快照逐条请求。注入窗口已关闭时
    /// 整批保持原位；空闲预订未执行就被销毁时，预订守卫会把同一条输入放回队列。
    pub fn promote_pending(
        self: &Arc<Self>,
        target: Option<&str>,
    ) -> Result<FollowUpPromotion, ConversationControlError> {
        // 空闲分支会发布预订窗口，因此要和写者窗口串行。
        let _window = self.lock_writer_window();
        let mut state = self.lock_state();
        // 指定了目标就先定位：control 不存在时，无论 turn 处于什么状态都报同一个错误。
        let positions = match target {
            Some(control_id) => {
                let position = locate_pending_input(&state.pending_inputs, control_id)?;
                position..position + 1
            }
            None => 0..state.pending_inputs.len(),
        };
        if positions.is_empty() {
            return Ok(FollowUpPromotion::Empty);
        }

        match &state.turn {
            TurnLifecycle::Running(controls) => {
                // 转交之后这些输入绑定到本次注入的 turn；注入窗口拒绝时整批保持原位。
                let requests = state
                    .pending_inputs
                    .range(positions.clone())
                    .map(|pending| pending.bound_to(&controls.turn_id))
                    .collect();
                if !controls.enqueue_all(requests) {
                    return Err(ConversationControlError::NotRunning);
                }
                state.pending_inputs.drain(positions);
                Ok(FollowUpPromotion::Injected)
            }
            TurnLifecycle::Idle => {
                // 空闲时提升：把队首（或指定目标）交给预订守卫；其余的按原顺序留在
                // 队列里，由这个预订的链条在自然交接点继续消费。
                let input = state
                    .pending_inputs
                    .remove(positions.start)
                    .expect("located pending input remains present under the state lock");
                state.turn = TurnLifecycle::Reserved;
                Ok(FollowUpPromotion::Reserved {
                    reservation: TurnReservation {
                        conversation: Arc::clone(self),
                        promoted_input: Some(input),
                    },
                })
            }
            TurnLifecycle::Reserved | TurnLifecycle::Compacting { .. } => {
                Err(ConversationControlError::NotRunning)
            }
        }
    }

    /// 撤回还没被消费的输入，不写入对话历史。
    pub fn withdraw_follow_up(
        &self,
        control_id: &str,
    ) -> Result<ControlSnapshot, ConversationControlError> {
        let mut state = self.lock_state();
        let position = state.editable_pending_position(control_id)?;
        let snapshot = state.pending_inputs[position].snapshot(ControlDisposition::Cancelled);
        state.pending_inputs.remove(position);
        Ok(snapshot)
    }

    /// 为独立压缩预订唯一的操作窗口，并公开共享写者，供设置立即保存；写者打开在状态锁
    /// 之外完成，见模块文档「锁」。
    pub fn reserve_compaction(self: &Arc<Self>) -> Result<TurnReservation, ConversationError> {
        let _window = self.lock_writer_window();
        let thread = {
            let state = self.lock_state();
            if state.is_occupied() {
                return Err(ConversationError::TurnAlreadyActive);
            }
            state.thread.clone()
        };
        let writer = self.runner.open_turn_writer(&thread)?;
        self.lock_state().turn = TurnLifecycle::Compacting {
            thread,
            writer,
            window: Arc::new(CancelWindow::new()),
        };
        Ok(TurnReservation {
            conversation: Arc::clone(self),
            promoted_input: None,
        })
    }

    /// 是否正在执行，或者还有待处理输入；这两个事实在同一次状态读取里一起判断。
    pub fn is_occupied(&self) -> bool {
        self.lock_state().is_occupied()
    }

    pub fn phase(&self) -> SessionPhase {
        self.lock_state().turn.phase()
    }

    /// 在同一个 state 临界区里取出会话侧投影，不复制整份 Thread。
    pub fn snapshot(&self) -> ConversationSnapshot {
        let state = self.lock_state();
        ConversationSnapshot {
            phase: state.turn.phase(),
            selector: state.thread.model.clone(),
            model_context_window: state.model_context_window(),
            pending_controls: state.pending_controls(),
        }
    }

    /// 停止当前回合或独立压缩，尚未执行的队列保留下来。
    pub fn abort(&self) -> Result<(), ConversationControlError> {
        match &self.lock_state().turn {
            TurnLifecycle::Running(controls) => controls.accept_cancel(),
            TurnLifecycle::Compacting { window, .. } => window.accept(),
            _ => Err(ConversationControlError::NotRunning),
        }
    }

    /// 宿主故障（执行 worker panic）之后交还输入：把本轮已接受但没交付的输入按接受
    /// 序号放回队列，并让生命周期回到空闲。正常的结果路径不走这里——那条路靠 Runner
    /// 的返回值完成同一交接。已经接受的停止同样取消未交付输入，不会因为 panic 让它们
    /// 复活。中毒的共享状态仍然 fail-stop 直接失败，不另造一套恢复状态。
    pub fn abandon_turn(&self) {
        let controls = {
            let mut state = self.lock_state();
            match std::mem::replace(&mut state.turn, TurnLifecycle::Idle) {
                TurnLifecycle::Running(controls) => controls,
                other => {
                    state.turn = other;
                    return;
                }
            }
        };
        let undelivered = controls.finish_inbox();
        if !controls.finish_cancel() {
            self.requeue_inputs(undelivered);
        }
    }

    /// 校验并立即保存下一轮要用的设置。运行或压缩期间复用当前的会话写者，空闲和预订阶段
    /// 临时开一个写者；写入成功之后才改变内存里的选择。写盘在状态锁之外完成：写者窗口把
    /// 打开和写盘串行化，状态锁只用来读取阶段和提交选择；临时开的写者在本函数返回前释放，
    /// 后续预订在同一个窗口里看到的是已经释放的写者。
    pub fn update_settings(&self, selector: &str) -> Result<(), ConversationError> {
        self.runner
            .validate_model_selector(selector)
            .map_err(ConversationError::Configuration)?;
        let updated = {
            let state = self.lock_state();
            if state.thread.model.as_deref().is_some_and(|current| {
                singularity_model::split_model_selector(current)
                    == singularity_model::split_model_selector(selector)
            }) {
                return Ok(());
            }
            let mut updated = state.thread.clone();
            updated.model = Some(selector.to_string());
            updated
        };
        let _window = self.lock_writer_window();
        // 写者从哪里来按当前阶段在一处决定，状态锁只覆盖这一次读取。写者打开只依赖会话身份和
        // cwd，与这次选择无关，所以用更新后的 Thread 打开。
        let existing = { self.lock_state().turn.writer() };
        let writer = match existing {
            Some(writer) => writer,
            None => self.runner.open_turn_writer(&updated)?,
        };
        crate::thread_catalog::record_thread_settings_metadata(&mut lock_writer(&writer), &updated)
            .map_err(ConversationError::Session)?;
        drop(writer);
        self.lock_state().thread = updated;
        Ok(())
    }

    /// 执行一轮 turn 直到终态，随后自动按先进先出消费已接受的后续输入：本轮显式输入的那一轮
    /// 先行（此前残留的已接受 followUp 更早执行），turn 到达可信终态
    /// （completed/failed/interrupted）后更新 Thread 投影，再把已接受的 followUp 启动成各自
    /// 有独立 turn id 的新 turn，直到队列清空；执行期间新提交的 followUp 同样会被消费。
    /// 同一时刻只允许一个活动 turn，执行期间客户端从其他线程通过共享的 TurnControls 做
    /// steer 和取消。
    ///
    /// 失败语义：只要终态已经落盘就返回 Ok（失败终态携带
    /// singularity_protocol::TurnErrorDetail，不阻断队列里其余的 followUp）；终态化失败
    /// （没有可信终态）或准备阶段失败返回 Err 并中止链条，没执行的 followUp 原样保留。
    /// 返回值是最后一个到达终态的 turn 结果。
    pub fn run_turn(
        self: &Arc<Self>,
        input: &str,
        sink: &mut dyn FnMut(TurnEvent),
    ) -> Result<TurnOutcome, ConversationError> {
        let mut reservation = self.reserve_start()?;
        reservation.run(input, sink)
    }

    fn run_chain(
        &self,
        input: ControlRequest,
        input_first: bool,
        sink: &mut dyn FnMut(TurnEvent),
    ) -> Result<TurnOutcome, ConversationError> {
        {
            let mut state = self.lock_state();
            if input_first {
                state.pending_inputs.push_front(input);
            } else {
                state.pending_inputs.push_back(input);
            }
        }
        let mut last = None;
        while let Some(current) = self.take_one_pending_input() {
            let TurnRunResult {
                result,
                undelivered,
                cancel_accepted,
            } = self.run_single_turn(current, sink);
            // 归还输入和分类结果都用 Runner 冻结的停止事实：停止会取消本轮未交付的
            // 输入，而先前明确排队的输入仍留在队列里。准备或存储失败会中止链条；
            // 可信的 Failed 终态只要没有停止，就继续消费后续输入。
            if !cancel_accepted {
                self.requeue_inputs(undelivered);
            }
            last = Some(result?);
            if cancel_accepted {
                break;
            }
        }
        #[allow(clippy::expect_used)]
        Ok(last.expect("run_turn executes at least one turn"))
    }

    /// 在预订窗口里执行一次输入，并把 Runner 的完整交接原样返回。写者打开失败和 Runner
    /// 准备失败不做区分：两者都用同一个 TurnRunResult 表达，未被消费的已接受输入随之归还。
    fn run_single_turn(
        &self,
        current: ControlRequest,
        sink: &mut dyn FnMut(TurnEvent),
    ) -> TurnRunResult {
        let (thread_snapshot, controls) = {
            let _window = self.lock_writer_window();
            let thread = {
                let state = self.lock_state();
                // 链执行期间始终由 TurnReservation 持有预订窗口；这是内部不变量，
                // 不再另造一套面向并发用户的失败路径。
                assert!(
                    matches!(state.turn, TurnLifecycle::Reserved),
                    "turn chain runs under its reservation"
                );
                state.thread.clone()
            };
            let writer = match self.runner.open_turn_writer(&thread) {
                Ok(writer) => writer,
                Err(error) => {
                    // 写者还没打开：本轮还不存在能接受停止的控制面，所以没有已接受的
                    // 停止事实，输入按既有规则归还。
                    return TurnRunResult {
                        result: Err(error),
                        undelivered: vec![current.unbound()],
                        cancel_accepted: false,
                    };
                }
            };
            let controls = Arc::new(TurnControls::new(
                Uuid::new_v4().to_string(),
                TurnInbox::default_handle(),
                writer,
            ));
            self.lock_state().turn = TurnLifecycle::Running(Arc::clone(&controls));
            (thread, controls)
        };
        let result = self.runner.run(current, &thread_snapshot, &controls, sink);
        {
            // Running → Reserved 的交接在同一个写者窗口内完成：先替换生命周期、释放
            // 本函数持有的控制句柄，旧写者的守卫随之在窗口内关闭。之后任何写者打开
            // （下一轮 turn，或空闲时临时打开）都在本窗口之后观察到已经释放的写者；
            // 控制命令经状态锁串行，不可能跨过交接点还持有旧句柄。
            let _window = self.lock_writer_window();
            let mut state = self.lock_state();
            state.turn = TurnLifecycle::Reserved;
            state.last_context_window = controls.context_window();
            drop(controls);
        }
        result
    }
    /// 测试用的观察入口：生产的控制路径一律在生命周期临界区里借用当前控制面。
    #[cfg(test)]
    pub(crate) fn active_controls(&self) -> Option<Arc<TurnControls>> {
        self.lock_state().turn.controls()
    }

    /// 在状态锁里取下一条待执行输入。
    fn take_one_pending_input(&self) -> Option<ControlRequest> {
        self.lock_state().pending_inputs.pop_front()
    }

    /// 把没执行的输入放回队列，维持「每条待执行输入恰好执行一次」的不变量。归还的输入保留
    /// 原来的 channel、身份和接受序号，但解除 turn 关联：它不再属于任何已开始的 turn，而是
    /// 在下一轮开始时和新的 turn 关联，因此这里也可能包含未交付的 steer。插入在一次状态锁内
    /// 按接受序号完成，channel 不决定等待位置。
    fn requeue_inputs(&self, inputs: Vec<ControlRequest>) {
        if inputs.is_empty() {
            return;
        }
        let mut state = self.lock_state();
        for input in inputs {
            insert_by_sequence(&mut state.pending_inputs, input.unbound());
        }
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, ConversationState> {
        self.state
            .lock()
            .expect("conversation state lock poisoned (fail-stop)")
    }

    /// 写者窗口：打开写者、交接写者和写盘都在这里串行。持有本窗口时只取状态锁做
    /// 短暂的读写，绝不反向等待状态锁的持有者，见模块文档「锁」。
    fn lock_writer_window(&self) -> std::sync::MutexGuard<'_, ()> {
        self.writer_window
            .lock()
            .expect("conversation writer window poisoned (fail-stop)")
    }
}
