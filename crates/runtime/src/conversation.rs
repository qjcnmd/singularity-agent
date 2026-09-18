//! 一个 session 的内存队列与唯一活动执行窗口。
//! 已消费的输入由 Agent 持久化；待处理输入随进程过期。
//!
//! # 锁
//!
//! 两个锁各司一职，锁序固定为「写者窗口 → 状态」，不存在反向等待：
//!
//! - `writer_window`：会话写者的打开、Running→Reserved 交接与设置写盘的互斥点。
//!   打开含整份会话解析与崩溃修复，耗时随会话文件增长，因此这一段不占用状态锁。
//! - `state`：线程设置、活动阶段、控制接受顺序与待处理输入。控制面读取
//!   （steer/abort/snapshot/phase）只取它，不被写者 I/O 挡住。
//!
//! # 锁失效策略
//!
//! 中毒表示共享状态不可信，直接 panic 结束进程，不降级继续运行。

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

/// 停止接受窗口：接受一次停止与冻结「是否接受过停止」的唯一临界点。
///
/// 普通 turn 与独立压缩共用同一规则：冻结边界之前接受的停止进入终态裁决，
/// 边界之后操作终态已经确定，stop 一律报告操作已结束。窗口不新增状态机，
/// 它只承载既有的取消令牌与接受标志。
pub(crate) struct CancelWindow {
    pub(crate) cancellation: CancellationToken,
    accepting: Mutex<bool>,
}

// Mutex 中毒表示共享状态不可信，直接报告失败。
#[allow(clippy::expect_used)]
impl CancelWindow {
    fn new() -> Self {
        Self {
            cancellation: CancellationToken::new(),
            accepting: Mutex::new(true),
        }
    }

    /// 接受一次停止；接受窗口冻结后返回 NotRunning。重复停止在冻结前幂等。
    fn accept(&self) -> Result<(), ConversationControlError> {
        let accepting = self.accepting.lock().expect("cancel window lock poisoned");
        if !*accepting {
            return Err(ConversationControlError::NotRunning);
        }
        drop(accepting);
        self.cancellation.cancel();
        Ok(())
    }

    /// 在提交终态前冻结接受事实，并返回本次操作是否接受过停止。
    pub(crate) fn freeze(&self) -> bool {
        let mut accepting = self.accepting.lock().expect("cancel window lock poisoned");
        *accepting = false;
        self.cancellation.is_cancelled()
    }
}

/// 一个活动 turn 的控制集合，仅在设置变更时共享其写者。
pub(crate) struct TurnControls {
    pub(crate) turn_id: String,
    window: CancelWindow,
    pub(crate) inbox: TurnInboxHandle,
    writer: SessionWriter,
    /// 本轮冻结模型的有效上下文窗口；公开快照据此报告用量分母，
    /// 不随后续配置编辑改变。start_turn 解析前为 None。
    context_window: std::sync::OnceLock<u64>,
}

// Mutex 中毒表示共享状态不可信，直接报告失败。
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

    /// 本轮取消令牌：取消的接受与冻结由同一 CancelWindow 决定。
    pub(crate) fn cancellation(&self) -> &CancellationToken {
        &self.window.cancellation
    }

    /// 记录本轮冻结模型的有效上下文窗口（由 runner 在解析后调用一次）。
    pub(crate) fn record_context_window(&self, window: u64) {
        let _ = self.context_window.set(window);
    }

    /// 本轮冻结的有效上下文窗口；start_turn 解析前为 None。
    pub(crate) fn context_window(&self) -> Option<u64> {
        self.context_window.get().copied()
    }

    /// 本轮注入箱句柄：供执行体构造时接收同一句柄。
    pub(crate) fn inbox_handle(&self) -> TurnInboxHandle {
        Arc::clone(&self.inbox)
    }

    /// 本轮共享会话写者（runner 与协调器控制路径共用）。
    pub(crate) fn writer(&self) -> SessionWriter {
        Arc::clone(&self.writer)
    }

    /// 把已创建的控制请求放入本轮注入箱；注入窗口已关闭时拒绝。
    /// 请求的身份与接受序号由 Conversation 在生命周期临界区内生成。
    pub(crate) fn enqueue(&self, request: ControlRequest) -> bool {
        self.lock_inbox().enqueue(request)
    }

    /// 一次临界区内整批放入本轮注入箱；窗口已关闭时整批都不交付。
    pub(crate) fn enqueue_all(&self, requests: Vec<ControlRequest>) -> bool {
        self.lock_inbox().enqueue_all(requests)
    }

    /// 接受一次停止：取消本轮并同时关闭新的注入窗口。停止之后到达的 steer
    /// 一律被拒绝，已经排队的输入保持原位（它们属于下一轮，不属于本轮的
    /// 取消集合）。停止接受与注入窗口关闭在同一受保护边界内完成。
    fn accept_cancel(&self) -> Result<(), ConversationControlError> {
        self.window.accept()?;
        self.lock_inbox().close();
        Ok(())
    }

    /// 在提交本轮终态前冻结用户是否已停止本轮。
    pub(crate) fn finish_cancel(&self) -> bool {
        self.window.freeze()
    }

    /// 关闭注入窗口，并把其中剩余的控制请求移交给 Runner。
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

/// 按 FIFO sequence 升序插入已接受的输入；同一序号不会出现两次，因此位置唯一。
fn insert_by_sequence(queue: &mut VecDeque<ControlRequest>, input: ControlRequest) {
    let position = queue
        .iter()
        .position(|existing| existing.sequence > input.sequence)
        .unwrap_or(queue.len());
    queue.insert(position, input);
}

/// 归还未执行的输入：与队列中已有输入共用同一接受序（sequence），channel
/// 不决定等待位置。显式 send-now 的提前执行由 run_promoted 单独表达。
fn requeue_by_sequence(state: &mut ConversationState, inputs: VecDeque<ControlRequest>) {
    for input in inputs {
        insert_by_sequence(&mut state.pending_inputs, input.unbound());
    }
}

/// 按 control_id 定位未消费的待执行输入；身份不存在时统一报告 ControlNotFound。
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
    /// 尚未开始的待执行输入，按提交顺序排队；条目一律携带控制身份与接受序号。
    /// channel 只记录输入从哪个入口被接受，不代表它当前是否待处理：普通提交、
    /// follow-up 与被 runner 归还的未消费 steer 都在这里等待执行。
    pending_inputs: VecDeque<ControlRequest>,
    /// steer 与 follow_up 共用的接受序号：控制身份与 FIFO 顺序由本状态一处推进。
    control_sequence: u64,
    /// 最近一次执行的冻结上下文窗口：解释最近请求用量的事实，不随设置
    /// 编辑改变；进程重启后不可知。
    last_context_window: Option<u64>,
}

impl ConversationState {
    /// 当前执行（或最近一次执行）冻结的有效上下文窗口：runner 在 turn
    /// 开始时解析模型配置并冻结到本轮控制面，空闲后保留最近一次执行的
    /// 事实。该值解释最近请求用量，不随后续配置编辑改变。
    fn model_context_window(&self) -> Option<u64> {
        let window = match &self.turn {
            TurnLifecycle::Running(controls) => controls.context_window(),
            _ => None,
        };
        window.or(self.last_context_window)
    }

    /// 返回尚未开始的待执行输入，其处置一律为 Pending。接受来源（channel）
    /// 原样保留供界面区分，但待处理集合只由本快照决定一次：runner 失败时归还
    /// 的未消费 steer 与普通 follow-up 一样在这里出现，普通提交同样如此。
    fn pending_controls(&self) -> Vec<ControlSnapshot> {
        self.pending_inputs
            .iter()
            .map(|request| request.snapshot(ControlDisposition::Pending))
            .collect()
    }

    /// 生成下一个控制请求：接受序号在此处推进一次，身份由 channel 与序号
    /// 唯一确定。正文为空的请求不占用序号。`turn_id` 只在该输入确实绑定到
    /// 某个 turn 时给出（注入活动 turn 的 steer）；等待自己那一轮的排队输入
    /// 没有可关联的 turn，不借用当前活动 turn 的身份。
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

    /// 排队一条后续 turn 输入，保留其身份与接受序号。接受窗口与既有行为一致：
    /// 只有活动 turn 存在时才接受追加输入；排队本身不需要写者。
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
        /// 与普通 turn 共用的停止接受窗口；压缩的提交边界同样冻结接受事实。
        window: Arc<CancelWindow>,
    },
}

impl TurnLifecycle {
    fn is_busy(&self) -> bool {
        !matches!(self, Self::Idle)
    }

    /// 执行状态直接来自操作窗口及其取消令牌，客户端只投影此值。
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

    /// 当前活动 turn 的控制面借用；生命周期临界区内的读与转发都经此取得，
    /// 只有确实要交出锁范围时才克隆句柄。
    fn active(&self) -> Option<&TurnControls> {
        match self {
            Self::Running(controls) => Some(controls),
            Self::Idle | Self::Reserved | Self::Compacting { .. } => None,
        }
    }

    /// 已打开的会话写者；空闲与预订阶段没有写者，由调用方短开一个。
    fn writer(&self) -> Option<SessionWriter> {
        match self {
            Self::Running(controls) => Some(controls.writer()),
            Self::Compacting { writer, .. } => Some(Arc::clone(writer)),
            Self::Idle | Self::Reserved => None,
        }
    }

    #[cfg(test)]
    fn controls(&self) -> Option<Arc<TurnControls>> {
        match self {
            Self::Running(controls) => Some(Arc::clone(controls)),
            _ => None,
        }
    }
}

/// 一次 state 临界区读出的会话侧事实：生命周期、模型选择、冻结上下文窗口与
/// 待处理控制。不可变投影，不缓存、不跨调用复用，也不是新的事实来源。
pub struct ConversationSnapshot {
    pub phase: SessionPhase,
    pub selector: Option<String>,
    /// 本轮冻结的有效上下文窗口：解释最近请求用量，不随后续配置编辑改变；
    /// 进程内尚无执行或进程重启后为 None。
    pub model_context_window: Option<u64>,
    pub pending_controls: Vec<ControlSnapshot>,
}

/// 一个 Thread 的长驻协调器。
pub struct Conversation {
    runner: Arc<TurnRunner>,
    /// Thread 设置、活动阶段、控制接受顺序与待处理输入由同一把锁协调。
    state: Mutex<ConversationState>,
    /// 会话写者窗口：写者打开、turn 交接与设置写盘的唯一互斥点，见模块文档「锁」。
    writer_window: Mutex<()>,
}

/// 唯一的执行预订；drop 时归还未使用的已提升输入。
pub struct TurnReservation {
    conversation: Arc<Conversation>,
    promoted_input: Option<ControlRequest>,
}

impl TurnReservation {
    /// 执行本轮输入及后续队列，直至链条结束；窗口保持到预订 drop。
    /// 控制处置变化经同一事件出口带类型发布。本轮输入在此取得控制身份，
    /// 与排队的后续输入共用同一套身份与序号规则。
    pub fn run(
        &mut self,
        input: &str,
        sink: &mut dyn FnMut(TurnEvent),
    ) -> Result<TurnOutcome, ConversationError> {
        // 提升出的输入有独立执行入口，误用在这里直接失败，不静默丢掉它。
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

    /// 执行由 pending follow-up 原子提升出的输入。该输入优先于队列中其余
    /// follow-up，并沿用原控制身份和接受序号。
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

    /// 在已预订的压缩窗口执行；预订继续持有到调用方完成投影收尾。
    pub fn compact(
        &mut self,
    ) -> Result<singularity_agent::compaction::CompactionOutcome, ConversationError> {
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

/// 一次“立即发送”的原子提升结果。
pub enum FollowUpPromotion {
    /// 目标集合为空：没有需要交接的输入（“全部发送”遇到空队列）。
    Empty,
    /// 输入已进入当前 turn 的注入箱，沿用原 control identity。
    Injected(Vec<ControlSnapshot>),
    /// Session 已空闲；队首输入已从队列转移到独占预订，其余按原顺序留在队列中。
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

// fail-stop 锁策略：中毒 panic 直接显式（见模块文档「锁失效策略」）。
#[allow(clippy::expect_used)]
impl Conversation {
    /// 建立任务协调器；未消费输入只保存在当前进程。
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

    /// 原子预订单活动 turn 的链窗口：窗口内其他预订与 run_turn 立即被
    /// 拒绝；窗口可被 TurnReservation::run 消费执行整条链，或由 drop
    /// 释放。发布窗口属于写者窗口，与写者打开和交接在同一处串行。
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

    /// 当前 Thread 投影快照。
    pub fn thread(&self) -> Thread {
        self.lock_state().thread.clone()
    }

    /// 向活动 turn 注入立即引导输入；无活动 turn 或注入窗口已关闭时返回错误。
    ///
    /// 接受检查、身份生成与输入入箱都在同一生命周期临界区内完成，避免跨越收尾窗口。
    pub fn steer(
        &self,
        text: impl Into<String>,
    ) -> Result<ControlSnapshot, ConversationControlError> {
        let mut state = self.lock_state();
        // 无活动 turn 时一律拒绝；正文校验在确认可注入之后，空正文不占用序号。
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

    /// 在活动回合后按 FIFO 执行输入；空闲时应直接开始回合。
    pub fn submit_follow_up(
        &self,
        text: impl Into<String>,
    ) -> Result<ControlSnapshot, ConversationControlError> {
        self.lock_state().queue_follow_up(text.into())
    }

    /// 接受一次普通提交：它与排队的后续输入进入同一队列，因此在这里取得
    /// 同一套控制身份与接受序号。该输入不伪装成活动 turn 的 steer，也不在
    /// 接受时借用任何 turn 身份；它开始自己的那一轮时才与 turn 关联。
    fn accept_submission(&self, text: String) -> Result<ControlRequest, ConversationError> {
        self.lock_state()
            .next_control(ControlChannel::Submit, None, text)
            .map_err(|error| ConversationError::Configuration(error.to_string()))
    }

    /// 修改未消费输入，保留其身份、接受序号和队列位置。
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
        let position = locate_pending_input(&state.pending_inputs, control_id)?;
        if matches!(
            state.turn,
            TurnLifecycle::Reserved | TurnLifecycle::Compacting { .. }
        ) {
            return Err(ConversationControlError::NotRunning);
        }
        // 就地改写已定位的队列项：身份、接受序号与队列位置都由原项保留。
        let request = &mut state.pending_inputs[position];
        request.text = text;
        Ok(request.snapshot(ControlDisposition::Pending))
    }

    /// 立即发送：把目标 pending 输入原子地提升为当前 turn 的输入，或在空闲时
    /// 提升为下一条独占执行预订。`target` 省略表示当前队列中的全部待处理输入。
    ///
    /// 目标读取、注入窗口判定与所有权转移共用 Conversation 状态锁与当前 turn
    /// inbox 锁：调用方不再按自己读到的快照逐条请求。注入窗口已关闭时整批保持
    /// 原位；空闲预订未执行即销毁时，预订守卫把同一条输入放回队列。
    pub fn promote_pending(
        self: &Arc<Self>,
        target: Option<&str>,
    ) -> Result<FollowUpPromotion, ConversationControlError> {
        // 空闲分支发布预订窗口，因此与写者窗口串行。
        let _window = self.lock_writer_window();
        let mut state = self.lock_state();
        // 指定目标先定位：不存在的 control 在任何 turn 状态下都报同一错误。
        let positions: Vec<usize> = match target {
            Some(control_id) => vec![locate_pending_input(&state.pending_inputs, control_id)?],
            None => (0..state.pending_inputs.len()).collect(),
        };
        if positions.is_empty() {
            return Ok(FollowUpPromotion::Empty);
        }

        match &state.turn {
            TurnLifecycle::Running(controls) => {
                // 转交后这些输入绑定到本次注入的 turn：注入窗口拒绝时整批保持原位。
                let turn_id = controls.turn_id.clone();
                let mut requests = Vec::with_capacity(positions.len());
                let mut snapshots = Vec::with_capacity(positions.len());
                for position in &positions {
                    let request = state.pending_inputs[*position].bound_to(&turn_id);
                    snapshots.push(request.snapshot(ControlDisposition::Pending));
                    requests.push(request);
                }
                if !controls.enqueue_all(requests) {
                    return Err(ConversationControlError::NotRunning);
                }
                // 倒序移除：索引在移除过程中保持有效。
                for position in positions.iter().rev() {
                    state
                        .pending_inputs
                        .remove(*position)
                        .expect("located pending input remains present under the state lock");
                }
                Ok(FollowUpPromotion::Injected(snapshots))
            }
            TurnLifecycle::Idle => {
                // 空闲提升把队首（或指定目标）整体交给预订守卫；其余按原顺序留队，
                // 由该预订的链条在自然交接点继续消费。
                let position = positions[0];
                let input = state
                    .pending_inputs
                    .remove(position)
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

    /// 撤回未消费输入，不写入对话历史。
    pub fn withdraw_follow_up(
        &self,
        control_id: &str,
    ) -> Result<ControlSnapshot, ConversationControlError> {
        let mut state = self.lock_state();
        let position = locate_pending_input(&state.pending_inputs, control_id)?;
        if matches!(
            state.turn,
            TurnLifecycle::Reserved | TurnLifecycle::Compacting { .. }
        ) {
            return Err(ConversationControlError::NotRunning);
        }
        let snapshot = state.pending_inputs[position].snapshot(ControlDisposition::Cancelled);
        state.pending_inputs.remove(position);
        Ok(snapshot)
    }

    /// 为独立压缩预订唯一操作窗口，并公开共享写者供设置立即保存。
    /// 写者打开在状态锁之外完成，见模块文档「锁」。取消接受窗口与普通 turn
    /// 共用同一实现：压缩的提交边界同样冻结接受事实。
    pub fn reserve_compaction(self: &Arc<Self>) -> Result<TurnReservation, ConversationError> {
        let _window = self.lock_writer_window();
        let thread = {
            let state = self.lock_state();
            if state.turn.is_busy() || !state.pending_inputs.is_empty() {
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

    /// 执行状态直接来自操作窗口及其取消令牌，客户端只投影此值。
    pub fn phase(&self) -> SessionPhase {
        self.lock_state().turn.phase()
    }

    /// 同一 state 临界区内的会话侧投影：客户端快照的相位、队列与模型选择
    /// 来自同一次读取，不复制整份 Thread。
    pub fn snapshot(&self) -> ConversationSnapshot {
        let state = self.lock_state();
        ConversationSnapshot {
            phase: state.turn.phase(),
            selector: state.thread.model.clone(),
            model_context_window: state.model_context_window(),
            pending_controls: state.pending_controls(),
        }
    }

    /// 停止当前回合或独立压缩，保留尚未执行的队列。
    pub fn abort(&self) -> Result<(), ConversationControlError> {
        match &self.lock_state().turn {
            TurnLifecycle::Running(controls) => controls.accept_cancel(),
            TurnLifecycle::Compacting { window, .. } => window.accept(),
            _ => Err(ConversationControlError::NotRunning),
        }
    }

    /// 宿主故障（执行 worker panic）后的输入交还：把本轮已接受但未交付的输入
    /// 按接受序号放回队列，并让生命周期回到空闲。正常结果路径不经这里——它由
    /// Runner 的返回值完成同一交接；已经接受的停止同样取消未交付输入，不因
    /// panic 复活它们。中毒的共享状态仍按 fail-stop 直接失败，不另造恢复状态。
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
        let undelivered: VecDeque<ControlRequest> = controls.finish_inbox().into_iter().collect();
        if !controls.cancellation().is_cancelled() {
            self.requeue_inputs(undelivered);
        }
    }

    /// 校验并立即保存下一轮设置。运行或压缩期间复用当前会话写者，
    /// 空闲与预订阶段短开写者；写入成功后才改变内存选择。
    ///
    /// 写盘在状态锁之外完成：写者窗口串行化打开与写盘，状态锁只用于读取阶段
    /// 与提交选择。短开的写者在本函数返回前释放，后续预订因此在同一窗口内
    /// 看到已释放的写者。
    pub fn update_settings(&self, selector: &str) -> Result<(), ConversationError> {
        self.runner
            .validate_model_selector(Some(selector))
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
        // 写者来源按当前阶段一处决定，状态锁只覆盖这一次读取：打开写者要解析
        // 整份会话，不能落在它的作用域里。写者打开只依赖会话身份与 cwd，
        // 与本次选择无关，因此用更新后的 Thread 打开。
        let existing = { self.lock_state().turn.writer() };
        let writer = match existing {
            Some(writer) => writer,
            None => self.runner.open_turn_writer(&updated)?,
        };
        crate::store::record_thread_settings_metadata(&mut lock_writer(&writer), &updated)
            .map_err(ConversationError::Session)?;
        // 短开的写者在这里释放：窗口不跨过状态提交，也不被后续预订继承。
        drop(writer);
        self.lock_state().thread = updated;
        Ok(())
    }

    /// 执行一轮 turn 直到终态；随后自动消费已接受的后续输入。
    ///
    /// 同一时刻只允许一个活动 turn；执行期间通过共享的 TurnControls
    /// （客户端从其他线程）进行 steer 与取消。整个调用内完成：
    ///
    /// 1. 本轮显式输入的 turn（若此前有残留的已接受 followUp，则按 FIFO 先行）；
    /// 2. turn 到达可信终态（completed/failed/interrupted）后更新 Thread 投影；
    /// 3. 按 FIFO 启动已接受的 followUp 为新的 turn（各自独立 turn id），
    ///    直到队列清空；执行期间新提交的 followUp 同样被消费。
    ///
    /// 失败语义：任何已落盘的可信终态都返回 Ok（失败终态携带
    /// singularity_protocol::TurnErrorDetail，不阻断队列中其余 followUp）；
    /// 终态化失败（无可信终态）或准备阶段失败返回 Err 并中止链条，
    /// 未执行的 followUp 原样保留。返回值为最后一个到达终态的 turn 结果。
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
            } = self.run_single_turn(current, sink);
            let retained: VecDeque<ControlRequest> = undelivered.into_iter().collect();
            match result {
                Err(error) => {
                    self.requeue_inputs(retained);
                    return Err(error.into());
                }
                Ok(outcome) => {
                    // 已接受的停止决定由 Runner 随终态原样带回：无论本轮收敛为
                    // Completed 还是 Failed，都不再启动下一条队列输入；未交付的
                    // 本轮输入已随停止被取消，不重新入队。没有停止时保持既有
                    // 「普通 Failed 继续消费队列」契约。
                    let stopped = outcome.user_stopped;
                    if !stopped {
                        self.requeue_inputs(retained);
                    }
                    last = Some(outcome);
                    if stopped {
                        break;
                    }
                }
            }
        }
        #[allow(clippy::expect_used)]
        Ok(last.expect("run_turn executes at least one turn"))
    }

    /// 在预订窗口内执行一次输入，并把 Runner 的完整交接原样返回。
    /// 写者打开失败与 Runner 准备失败不区分处理：两者都以同一个
    /// TurnRunResult 表达，未消费的已接受输入随之归还。
    fn run_single_turn(
        &self,
        current: ControlRequest,
        sink: &mut dyn FnMut(TurnEvent),
    ) -> TurnRunResult {
        let (thread_snapshot, controls) = {
            // 打开写者含整份会话解析与崩溃修复，因此在状态锁之外、写者窗口之内完成。
            let _window = self.lock_writer_window();
            let thread = {
                let state = self.lock_state();
                // 链执行始终由 TurnReservation 持有预订窗口；这里是内部不变量，
                // 不再构造第二套面向并发用户的失败路径。
                assert!(
                    matches!(state.turn, TurnLifecycle::Reserved),
                    "turn chain runs under its reservation"
                );
                state.thread.clone()
            };
            let writer = match self.runner.open_turn_writer(&thread) {
                Ok(writer) => writer,
                Err(error) => {
                    return TurnRunResult {
                        result: Err(error),
                        undelivered: vec![current.unbound()],
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
            // Running → Reserved 的交接在同一写者窗口内完成：先替换生命周期并
            // 释放本函数持有的控制句柄，旧写者的守卫随之在窗口内关闭。后续任何
            // 写者打开（下一轮 turn 或空闲短开）都在本窗口之后观察到已释放的写者；
            // 控制命令经状态锁串行，不可能跨过交接点持有旧句柄。
            let _window = self.lock_writer_window();
            let mut state = self.lock_state();
            state.turn = TurnLifecycle::Reserved;
            state.last_context_window = controls.context_window();
            drop(controls);
        }
        result
    }
    /// 测试观察入口：生产控制路径一律经生命周期临界区借用当前控制面，
    /// 不再克隆活动句柄。
    #[cfg(test)]
    pub(crate) fn active_controls(&self) -> Option<Arc<TurnControls>> {
        self.lock_state().turn.controls()
    }

    /// 在状态锁内取下一条待执行输入；链预订由 guard 保持到调用方完成收尾。
    fn take_one_pending_input(&self) -> Option<ControlRequest> {
        self.lock_state().pending_inputs.pop_front()
    }

    /// 把未执行的输入放回队列，保证「每条待执行输入恰好执行一次」不变量
    /// 可观察。归还的输入保留其原始 channel、身份与接受序号，但解除 turn 关联：
    /// 它不再属于任何已开始的 turn，而是在下一轮开始时与新的 turn 关联。因此
    /// 也可能包含未交付的 steer。
    fn requeue_inputs(&self, inputs: VecDeque<ControlRequest>) {
        if inputs.is_empty() {
            return;
        }
        let mut state = self.lock_state();
        requeue_by_sequence(&mut state, inputs);
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, ConversationState> {
        self.state
            .lock()
            .expect("conversation state lock poisoned (fail-stop)")
    }

    /// 写者窗口：打开写者、交接写者与写盘都在此串行。持有本窗口时只取状态锁
    /// 做短暂读写，绝不反向等待状态锁的持有者，见模块文档「锁」。
    fn lock_writer_window(&self) -> std::sync::MutexGuard<'_, ()> {
        self.writer_window
            .lock()
            .expect("conversation writer window poisoned (fail-stop)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use singularity_protocol::TurnStatus;

    #[test]
    #[allow(clippy::unwrap_used)]
    fn cancellation_does_not_depend_on_session_writes() {
        let dir = tempfile::tempdir().unwrap();
        let session = singularity_agent::session::SessionManager::create(
            dir.path(),
            &dir.path().join("sessions"),
        )
        .unwrap();
        let path = session.path().to_path_buf();
        let controls = TurnControls::new(
            "turn",
            TurnInbox::default_handle(),
            Arc::new(Mutex::new(session)),
        );
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        controls.accept_cancel().unwrap();
        assert!(controls.cancellation().is_cancelled());
        assert!(controls.finish_cancel());
        assert!(matches!(
            controls.accept_cancel(),
            Err(ConversationControlError::NotRunning)
        ));
    }

    /// 控制命令与生命周期交接串行，后续输入不会与旧写者冲突。
    #[test]
    #[allow(clippy::expect_used)]
    #[allow(clippy::unwrap_used)]
    fn control_commands_serialize_with_the_lifecycle_and_never_conflict_the_handoff() {
        let fixture = crate::test_support::SessionsFixture::new();
        let (gate, started) = crate::test_support::GatedProvider::stop_gate();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        gate.with_release(release_rx);
        let (conversation, _) = crate::test_support::conversation_with(
            &fixture,
            gate as Arc<dyn singularity_model::Provider + Send + Sync>,
            None,
        );
        let worker = {
            let conversation = Arc::clone(&conversation);
            std::thread::spawn(move || {
                let mut sink = |_event: TurnEvent| {};
                conversation.run_turn("initial", &mut sink)
            })
        };
        started
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("turn reaches provider");

        {
            let controls = conversation.active_controls().unwrap();
            let writer = controls.writer();
            let _writer_guard = lock_writer(&writer);
            conversation.abort().unwrap();
            assert!(controls.cancellation().is_cancelled());
            assert!(conversation.snapshot().pending_controls.is_empty());
        }
        let _ = release_tx.send(());
        let outcome = worker.join().expect("worker").expect("cancel converges");
        assert_eq!(outcome.turn_status, TurnStatus::Interrupted);

        // 交接后旧写者已释放：下一条输入以新写者正常执行，不与旧句柄冲突。
        let mut sink = |_event: TurnEvent| {};
        let next = conversation
            .run_turn("next input", &mut sink)
            .expect("the thread stays usable after the handoff");
        assert_eq!(next.turn_status, TurnStatus::Completed);
    }

    #[test]
    #[allow(clippy::expect_used)]
    fn promotion_at_a_closed_inbox_keeps_the_follow_up_queued() {
        let fixture = crate::test_support::SessionsFixture::new();
        let (gate, started) = crate::test_support::GatedProvider::stop_gate();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        gate.with_release(release_rx);
        let (conversation, _) = crate::test_support::conversation_with(
            &fixture,
            Arc::clone(&gate) as Arc<dyn singularity_model::Provider + Send + Sync>,
            None,
        );
        let worker = {
            let conversation = Arc::clone(&conversation);
            std::thread::spawn(move || {
                let mut sink = |_event: TurnEvent| {};
                conversation.run_turn("initial", &mut sink)
            })
        };
        started
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("turn reaches provider");
        let queued = conversation
            .submit_follow_up("survive the terminal race")
            .expect("queue follow-up");
        conversation
            .active_controls()
            .expect("active controls")
            .lock_inbox()
            .close();

        assert!(matches!(
            conversation.promote_pending(Some(&queued.control_id)),
            Err(ConversationControlError::NotRunning)
        ));
        assert_eq!(
            conversation.snapshot().pending_controls[0].control_id,
            queued.control_id
        );
        let _ = release_tx.send(());
        worker
            .join()
            .expect("worker")
            .expect("queued follow-up still executes");
    }

    /// 批量立即发送同样按整批判定注入窗口：窗口已关闭时一条都不交付，全部留在
    /// 原队列，不存在逐条的部分接受。
    #[test]
    #[allow(clippy::expect_used)]
    fn batch_promotion_at_a_closed_inbox_keeps_every_entry_queued() {
        let fixture = crate::test_support::SessionsFixture::new();
        let (gate, started) = crate::test_support::GatedProvider::stop_gate();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        gate.with_release(release_rx);
        let (conversation, _) = crate::test_support::conversation_with(
            &fixture,
            Arc::clone(&gate) as Arc<dyn singularity_model::Provider + Send + Sync>,
            None,
        );
        let worker = {
            let conversation = Arc::clone(&conversation);
            std::thread::spawn(move || {
                let mut sink = |_event: TurnEvent| {};
                conversation.run_turn("initial", &mut sink)
            })
        };
        started
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("turn reaches provider");
        let first = conversation
            .submit_follow_up("kept one")
            .expect("queue first follow-up");
        let second = conversation
            .submit_follow_up("kept two")
            .expect("queue second follow-up");
        conversation
            .active_controls()
            .expect("active controls")
            .lock_inbox()
            .close();

        assert!(matches!(
            conversation.promote_pending(None),
            Err(ConversationControlError::NotRunning)
        ));
        assert_eq!(
            conversation
                .snapshot()
                .pending_controls
                .iter()
                .map(|control| control.control_id.clone())
                .collect::<Vec<_>>(),
            vec![first.control_id, second.control_id],
            "a rejected batch leaves every entry in its original queue"
        );
        let _ = release_tx.send(());
        worker
            .join()
            .expect("worker")
            .expect("queued follow-ups still execute");
    }
}
