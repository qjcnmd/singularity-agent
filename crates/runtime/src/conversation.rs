//! A session's in-memory queue and single active execution window.
//! Consumed inputs are persisted by Agent; pending inputs expire with the process.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use singularity_agent::agent::{TurnInbox, TurnInboxHandle};
use singularity_agent::session::{
    ControlChannel, ControlDisposition, ControlRequest, SessionWriter, control_id, lock_writer,
};
use singularity_core::CancellationToken;
use singularity_model::ModelConfigurationSnapshot;
use singularity_protocol::{ControlSnapshot, SessionPhase};
use uuid::Uuid;

use crate::error::TurnRunError;
use crate::runner::{TurnOutcome, TurnParams, TurnRunResult, TurnRunner};
use singularity_protocol::TurnEvent;
use singularity_protocol::{Thread, TurnStatus};

/// Controls for one active turn, sharing its writer only for settings changes.
pub(crate) struct TurnControls {
    pub(crate) turn_id: String,
    pub cancellation: CancellationToken,
    pub(crate) inbox: TurnInboxHandle,
    control_sequence: Arc<AtomicU64>,
    accepting_cancel: Mutex<bool>,
    writer: SessionWriter,
    /// runner 在 start_turn 解析出的本轮冻结模型配置；公开快照据此报告
    /// 有效上下文窗口，不随后续配置编辑改变。
    model: std::sync::OnceLock<ModelConfigurationSnapshot>,
}

// Mutex 中毒表示共享状态不可信，直接报告失败。
#[allow(clippy::expect_used)]
impl TurnControls {
    pub fn new(
        turn_id: impl Into<String>,
        inbox: TurnInboxHandle,
        control_sequence: Arc<AtomicU64>,
        writer: SessionWriter,
    ) -> Self {
        Self {
            turn_id: turn_id.into(),
            cancellation: CancellationToken::new(),
            inbox,
            control_sequence,
            accepting_cancel: Mutex::new(true),
            writer,
            model: std::sync::OnceLock::new(),
        }
    }

    /// 记录本轮冻结的模型配置（由 runner 在解析后调用一次）。
    pub(crate) fn record_model(&self, model: ModelConfigurationSnapshot) {
        let _ = self.model.set(model);
    }

    /// 本轮冻结的模型配置；start_turn 解析前为 None。
    pub(crate) fn model_configuration(&self) -> Option<&ModelConfigurationSnapshot> {
        self.model.get()
    }

    /// 本轮注入箱句柄：供执行体构造时接收同一句柄。
    pub(crate) fn inbox_handle(&self) -> TurnInboxHandle {
        Arc::clone(&self.inbox)
    }

    /// 本轮共享会话写者（runner 与协调器控制路径共用）。
    pub(crate) fn writer(&self) -> SessionWriter {
        Arc::clone(&self.writer)
    }

    /// Accept steering while the current inbox is open.
    pub fn steer(
        &self,
        text: impl Into<String>,
    ) -> Result<ControlSnapshot, ConversationControlError> {
        let text = text.into();
        if text.trim().is_empty() {
            return Err(ConversationControlError::InvalidInput);
        }
        let sequence = self.control_sequence.fetch_add(1, Ordering::Relaxed);
        let request = ControlRequest {
            control_id: control_id(&self.turn_id, ControlChannel::Steer, sequence),
            turn_id: self.turn_id.clone(),
            channel: ControlChannel::Steer,
            sequence,
            text,
        };
        let enqueued = self.lock_inbox().enqueue(request.clone());
        if !enqueued {
            return Err(ConversationControlError::NotRunning);
        }
        Ok(request.snapshot(ControlDisposition::Pending))
    }

    fn accept_cancel(&self) -> Result<(), ConversationControlError> {
        let accepting = self
            .accepting_cancel
            .lock()
            .expect("cancel window lock poisoned");
        if !*accepting {
            return Err(ConversationControlError::NotRunning);
        }
        self.cancellation.cancel();
        Ok(())
    }

    /// Freeze whether the user stopped this turn before committing its terminal.
    pub(crate) fn finish_cancel(&self) -> bool {
        let mut accepting = self
            .accepting_cancel
            .lock()
            .expect("cancel window lock poisoned");
        *accepting = false;
        self.cancellation.is_cancelled()
    }

    /// Close the injection window and transfer its remaining controls to Runner.
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

/// An explicit turn input or an identified queued input.
#[derive(Clone)]
enum ChainInput {
    Explicit(String),
    Accepted(ControlRequest),
}

impl ChainInput {
    fn control(&self) -> Option<&ControlRequest> {
        match self {
            Self::Explicit(_) => None,
            Self::Accepted(request) => Some(request),
        }
    }

    fn control_id(&self) -> Option<&str> {
        self.control().map(|request| request.control_id.as_str())
    }
}

/// 按 FIFO sequence 升序插入已接受的输入；显式输入（无控制）追加到队尾。
fn insert_by_sequence(queue: &mut VecDeque<ChainInput>, input: ChainInput) {
    let Some(sequence) = input.control().map(|request| request.sequence) else {
        queue.push_back(input);
        return;
    };
    let position = queue
        .iter()
        .position(|existing| {
            existing
                .control()
                .is_some_and(|request| request.sequence > sequence)
        })
        .unwrap_or(queue.len());
    queue.insert(position, input);
}

struct ConversationState {
    thread: Thread,
    turn: TurnLifecycle,
    /// 已接受的后续 turn 输入，按提交顺序 FIFO 执行；条目携带接受序号。
    pending_follow_ups: VecDeque<ChainInput>,
    /// 最近一次执行的冻结模型配置：解释最近请求用量的事实，不随设置
    /// 编辑改变；进程重启后不可知。
    last_model: Option<ModelConfigurationSnapshot>,
}

enum TurnLifecycle {
    Idle,
    Reserved,
    Running(Arc<TurnControls>),
    Compacting {
        thread: Thread,
        writer: SessionWriter,
        cancellation: CancellationToken,
    },
}

impl TurnLifecycle {
    fn is_busy(&self) -> bool {
        !matches!(self, Self::Idle)
    }

    fn controls(&self) -> Option<Arc<TurnControls>> {
        match self {
            Self::Running(controls) => Some(Arc::clone(controls)),
            Self::Idle | Self::Reserved | Self::Compacting { .. } => None,
        }
    }
}

/// 一个 Thread 的长驻协调器。
pub struct Conversation {
    runner: Arc<TurnRunner>,
    /// FIFO order shared by steering and follow-up inputs.
    control_sequence: Arc<AtomicU64>,
    /// Thread 设置、活动阶段与待处理输入由同一把锁协调。
    state: Mutex<ConversationState>,
}

/// Unique execution reservation; drop returns any unused promoted input.
pub struct TurnReservation {
    conversation: Arc<Conversation>,
    promoted_input: Option<ChainInput>,
}

impl TurnReservation {
    /// 执行本轮输入及后续队列，直至链条结束；窗口保持到预订 drop。
    /// 控制处置变化经同一事件出口带类型发布。
    pub fn run(
        &mut self,
        input: &str,
        sink: &mut dyn FnMut(TurnEvent),
    ) -> Result<TurnOutcome, ConversationError> {
        debug_assert!(self.promoted_input.is_none());
        self.conversation
            .run_chain(ChainInput::Explicit(input.to_string()), false, sink)
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
        let (thread, writer, cancellation) = match &self.conversation.lock_state().turn {
            TurnLifecycle::Compacting {
                thread,
                writer,
                cancellation,
            } => (thread.clone(), Arc::clone(writer), cancellation.clone()),
            _ => return Err(ConversationError::TurnAlreadyActive),
        };
        self.conversation
            .runner
            .compact_thread(&thread, &cancellation, writer)
            .map_err(ConversationError::Compaction)
    }
}

impl Drop for TurnReservation {
    fn drop(&mut self) {
        let mut state = self.conversation.lock_state();
        if let Some(input) = self.promoted_input.take() {
            insert_by_sequence(&mut state.pending_follow_ups, input);
        }
        state.turn = TurnLifecycle::Idle;
    }
}

/// 指定 follow-up 的原子提升结果。
pub enum FollowUpPromotion {
    /// 输入已进入当前 turn 的注入箱，沿用原 control identity。
    Injected(ControlSnapshot),
    /// Session 已空闲；输入已从队列转移到独占预订，调用方应启动该预订。
    Reserved {
        control: ControlSnapshot,
        reservation: TurnReservation,
    },
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
            control_sequence: Arc::new(AtomicU64::new(0)),
            state: Mutex::new(ConversationState {
                thread,
                turn: TurnLifecycle::Idle,
                pending_follow_ups: VecDeque::new(),
                last_model: None,
            }),
        })
    }

    /// 原子预订单活动 turn 的链窗口：窗口内其他预订与 run_turn 立即被
    /// 拒绝；窗口可被 TurnReservation::run 消费执行整条链，或由 drop
    /// 释放。
    pub fn reserve_start(self: &Arc<Self>) -> Result<TurnReservation, ConversationError> {
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
    /// 接受检查与输入入箱在同一生命周期临界区内完成，避免跨越收尾窗口。
    pub fn steer(
        &self,
        text: impl Into<String>,
    ) -> Result<ControlSnapshot, ConversationControlError> {
        let state = self.lock_state();
        match &state.turn {
            TurnLifecycle::Running(controls) => controls.steer(text),
            _ => Err(ConversationControlError::NotRunning),
        }
    }

    /// 在活动回合后按 FIFO 执行输入；空闲时应直接开始回合。
    pub fn submit_follow_up(
        &self,
        text: impl Into<String>,
    ) -> Result<ControlSnapshot, ConversationControlError> {
        let text = text.into();
        if text.trim().is_empty() {
            return Err(ConversationControlError::InvalidInput);
        }
        let mut state = self.lock_state();
        let controls = state
            .turn
            .controls()
            .ok_or(ConversationControlError::NotRunning)?;
        let sequence = self.control_sequence.fetch_add(1, Ordering::Relaxed);
        let request = ControlRequest {
            control_id: control_id(&controls.turn_id, ControlChannel::FollowUp, sequence),
            turn_id: controls.turn_id.clone(),
            channel: ControlChannel::FollowUp,
            sequence,
            text,
        };
        let snapshot = request.snapshot(ControlDisposition::Pending);
        insert_by_sequence(&mut state.pending_follow_ups, ChainInput::Accepted(request));
        Ok(snapshot)
    }

    /// 返回尚未开始的队列输入。
    pub fn pending_controls(&self) -> Vec<ControlSnapshot> {
        self.lock_state()
            .pending_follow_ups
            .iter()
            .filter_map(ChainInput::control)
            .map(|request| request.snapshot(ControlDisposition::Pending))
            .collect()
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
        let position = state
            .pending_follow_ups
            .iter()
            .position(|input| input.control_id() == Some(control_id))
            .ok_or(ConversationControlError::ControlNotFound)?;
        let mut request = state.pending_follow_ups[position]
            .control()
            .cloned()
            .ok_or(ConversationControlError::ControlNotFound)?;
        request.text = text;
        let snapshot = request.snapshot(ControlDisposition::Pending);
        if matches!(
            state.turn,
            TurnLifecycle::Reserved | TurnLifecycle::Compacting { .. }
        ) {
            return Err(ConversationControlError::NotRunning);
        }
        state.pending_follow_ups[position] = ChainInput::Accepted(request);
        Ok(snapshot)
    }

    /// 将指定 pending follow-up 原子提升为当前 turn 的输入，或在空闲时提升为
    /// 下一条独占执行预订。所有权转移只使用 Conversation 状态锁与当前 turn
    /// inbox 锁：注入窗口已关闭时原队列项保持原位；空闲预订未执行即销毁时，
    /// 预订守卫把同一条输入放回队列。
    pub fn promote_follow_up(
        self: &Arc<Self>,
        control_id: &str,
    ) -> Result<FollowUpPromotion, ConversationControlError> {
        let mut state = self.lock_state();
        let position = state
            .pending_follow_ups
            .iter()
            .position(|input| input.control_id() == Some(control_id))
            .ok_or(ConversationControlError::ControlNotFound)?;
        let input = state
            .pending_follow_ups
            .get(position)
            .cloned()
            .ok_or(ConversationControlError::ControlNotFound)?;
        let request = input
            .control()
            .ok_or(ConversationControlError::ControlNotFound)?;
        let snapshot = request.snapshot(ControlDisposition::Pending);

        match &state.turn {
            TurnLifecycle::Running(controls) => {
                let enqueued = controls.lock_inbox().enqueue(request.clone());
                if !enqueued {
                    return Err(ConversationControlError::NotRunning);
                }
                state
                    .pending_follow_ups
                    .remove(position)
                    .expect("located follow-up remains present under the state lock");
                Ok(FollowUpPromotion::Injected(snapshot))
            }
            TurnLifecycle::Idle => {
                let input = state
                    .pending_follow_ups
                    .remove(position)
                    .expect("located follow-up remains present under the state lock");
                state.turn = TurnLifecycle::Reserved;
                Ok(FollowUpPromotion::Reserved {
                    control: snapshot,
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
        let position = state
            .pending_follow_ups
            .iter()
            .position(|input| input.control_id() == Some(control_id))
            .ok_or(ConversationControlError::ControlNotFound)?;
        let request = state.pending_follow_ups[position]
            .control()
            .cloned()
            .ok_or(ConversationControlError::ControlNotFound)?;
        if matches!(
            state.turn,
            TurnLifecycle::Reserved | TurnLifecycle::Compacting { .. }
        ) {
            return Err(ConversationControlError::NotRunning);
        }
        state.pending_follow_ups.remove(position);
        Ok(request.snapshot(ControlDisposition::Cancelled))
    }

    /// 为独立压缩预订唯一操作窗口，并公开共享写者供设置立即保存。
    pub fn reserve_compaction(
        self: &Arc<Self>,
        cancellation: CancellationToken,
    ) -> Result<TurnReservation, ConversationError> {
        let mut state = self.lock_state();
        if state.turn.is_busy() || !state.pending_follow_ups.is_empty() {
            return Err(ConversationError::TurnAlreadyActive);
        }
        let thread = state.thread.clone();
        let writer = self.runner.open_turn_writer(&thread)?;
        state.turn = TurnLifecycle::Compacting {
            thread,
            writer,
            cancellation,
        };
        Ok(TurnReservation {
            conversation: Arc::clone(self),
            promoted_input: None,
        })
    }

    /// 同步压缩入口；与工作台共享预订、取消、写者和释放过程。
    pub fn compact(
        self: &Arc<Self>,
        cancellation: &CancellationToken,
    ) -> Result<singularity_agent::compaction::CompactionOutcome, ConversationError> {
        self.reserve_compaction(cancellation.clone())?.compact()
    }

    /// 执行状态直接来自操作窗口及其取消令牌，客户端只投影此值。
    pub fn phase(&self) -> SessionPhase {
        match &self.lock_state().turn {
            TurnLifecycle::Idle => SessionPhase::Idle,
            TurnLifecycle::Reserved => SessionPhase::Reserved,
            TurnLifecycle::Running(controls) if controls.cancellation.is_cancelled() => {
                SessionPhase::Stopping
            }
            TurnLifecycle::Running(_) => SessionPhase::Running,
            TurnLifecycle::Compacting { cancellation, .. } if cancellation.is_cancelled() => {
                SessionPhase::Stopping
            }
            TurnLifecycle::Compacting { .. } => SessionPhase::Compacting,
        }
    }

    /// 当前执行（或最近一次执行）冻结的有效上下文窗口：runner 在 turn
    /// 开始时解析模型配置并冻结到本轮控制面，空闲后保留最近一次执行的
    /// 事实。该值解释最近请求用量，不随后续配置编辑改变；进程内尚无
    /// 执行或进程重启后为 None。
    pub fn model_context_window(&self) -> Option<u64> {
        let state = self.lock_state();
        let model = match &state.turn {
            TurnLifecycle::Running(controls) => controls.model_configuration(),
            _ => None,
        };
        model
            .or(state.last_model.as_ref())
            .map(ModelConfigurationSnapshot::context_window)
    }

    /// 停止当前回合或独立压缩，保留尚未执行的队列。
    pub fn abort(&self) -> Result<(), ConversationControlError> {
        match &self.lock_state().turn {
            TurnLifecycle::Running(controls) => controls.accept_cancel(),
            TurnLifecycle::Compacting { cancellation, .. } => {
                cancellation.cancel();
                Ok(())
            }
            _ => Err(ConversationControlError::NotRunning),
        }
    }

    /// 校验并立即保存下一轮设置。运行或压缩期间复用当前会话写者，
    /// 空闲与预订阶段短开写者；写入成功后才改变内存选择。
    pub fn update_settings(&self, selector: &str) -> Result<(), ConversationError> {
        let mut state = self.lock_state();
        self.runner
            .validate_model_selector(Some(selector))
            .map_err(ConversationError::Configuration)?;
        if state.thread.model.as_deref().is_some_and(|current| {
            singularity_model::split_model_selector(current)
                == singularity_model::split_model_selector(selector)
        }) {
            return Ok(());
        }
        let mut updated = state.thread.clone();
        updated.model = Some(selector.to_string());
        let writer = match &state.turn {
            TurnLifecycle::Running(controls) => controls.writer(),
            TurnLifecycle::Compacting { writer, .. } => Arc::clone(writer),
            TurnLifecycle::Idle | TurnLifecycle::Reserved => {
                self.runner.open_turn_writer(&state.thread)?
            }
        };
        crate::runner::record_thread_settings_metadata(&mut lock_writer(&writer), &updated)
            .map_err(ConversationError::Session)?;
        state.thread = updated;
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
        input: ChainInput,
        input_first: bool,
        sink: &mut dyn FnMut(TurnEvent),
    ) -> Result<TurnOutcome, ConversationError> {
        {
            let mut state = self.lock_state();
            if input_first {
                state.pending_follow_ups.push_front(input);
            } else {
                state.pending_follow_ups.push_back(input);
            }
        }
        let mut last = None;
        while let Some(current) = self.take_one_pending_follow_up() {
            let run = match self.run_single_turn(current.clone(), sink) {
                Ok(run) => run,
                Err(error) => {
                    if current.control().is_some() {
                        self.requeue_follow_ups(VecDeque::from([current]));
                    }
                    return Err(error);
                }
            };
            let TurnRunResult {
                result,
                undelivered,
            } = run;
            match result {
                Err(error) => {
                    let mut retained: VecDeque<_> =
                        undelivered.into_iter().map(ChainInput::Accepted).collect();
                    if current.control().is_some()
                        && matches!(error, TurnRunError::Preparation { .. })
                    {
                        // Only a turn that never started may retry its current input.
                        retained.push_front(current);
                    }
                    self.requeue_follow_ups(retained);
                    return Err(error.into());
                }
                Ok(outcome) if outcome.turn_status == TurnStatus::Interrupted => {
                    // 中断时未交付的控制已由 runner 落盘 Cancelled 处置；
                    // 内部队列只保留跨 turn 的后续输入。
                    return Ok(outcome);
                }
                Ok(outcome) => {
                    self.requeue_follow_ups(
                        undelivered.into_iter().map(ChainInput::Accepted).collect(),
                    );
                    last = Some(outcome);
                }
            }
        }
        #[allow(clippy::expect_used)]
        Ok(last.expect("run_turn executes at least one turn"))
    }

    /// Set up one turn, then return Runner's complete handoff unchanged.
    /// Setup errors have no active inbox; Runner errors retain unconsumed controls.
    fn run_single_turn(
        &self,
        current: ChainInput,
        sink: &mut dyn FnMut(TurnEvent),
    ) -> Result<TurnRunResult, ConversationError> {
        let (thread_snapshot, controls) = {
            let mut state = self.lock_state();
            if !matches!(state.turn, TurnLifecycle::Reserved) {
                return Err(ConversationError::TurnAlreadyActive);
            }
            let thread = state.thread.clone();
            let writer = self.runner.open_turn_writer(&thread)?;
            let controls = Arc::new(TurnControls::new(
                Uuid::new_v4().to_string(),
                TurnInbox::default_handle(),
                Arc::clone(&self.control_sequence),
                writer,
            ));
            state.turn = TurnLifecycle::Running(Arc::clone(&controls));
            (thread, controls)
        };
        let (input, control) = match current {
            ChainInput::Explicit(text) => (text, None),
            ChainInput::Accepted(request) => (request.text.clone(), Some(request)),
        };
        let result = self.runner.run(
            TurnParams {
                thread: thread_snapshot,
                input,
                control,
            },
            &controls,
            sink,
        );
        {
            // Running → Reserved 的交接在同一生命周期临界区内完成：先替换
            // 生命周期并释放本函数持有的控制句柄，旧写者的守卫随之在锁内
            // 关闭。后续任何写者打开（下一轮 turn 或空闲短开）都在本锁之后
            // 观察到已释放的写者窗口；控制命令同样经本锁串行化，不可能跨过
            // 交接点持有旧句柄。
            let mut state = self.lock_state();
            state.turn = TurnLifecycle::Reserved;
            state.last_model = controls.model_configuration().cloned();
            drop(controls);
        }
        Ok(result)
    }
    /// 测试观察入口：生产控制路径一律经生命周期临界区借用当前控制面，
    /// 不再克隆活动句柄。
    #[cfg(test)]
    fn active_controls(&self) -> Option<Arc<TurnControls>> {
        self.lock_state().turn.controls()
    }

    /// 在状态锁内取下一条排队输入；链预订由 guard 保持到调用方完成收尾。
    fn take_one_pending_follow_up(&self) -> Option<ChainInput> {
        self.lock_state().pending_follow_ups.pop_front()
    }

    /// 把未执行的 followUp 输入放回队列（与队列中已有输入合并，输入在前），
    /// 保证「每条 followUp 恰好执行一次」不变量可观察。
    fn requeue_follow_ups(&self, inputs: VecDeque<ChainInput>) {
        if inputs.is_empty() {
            return;
        }
        let mut state = self.lock_state();
        let mut merged = inputs;
        merged.extend(state.pending_follow_ups.drain(..));
        state.pending_follow_ups = merged;
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, ConversationState> {
        self.state
            .lock()
            .expect("conversation state lock poisoned (fail-stop)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            Arc::new(AtomicU64::new(0)),
            Arc::new(Mutex::new(session)),
        );
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        controls.accept_cancel().unwrap();
        assert!(controls.cancellation.is_cancelled());
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
        let home = crate::test_support::temp_sessions();
        let sessions = home.path().join("sessions");
        let (gate, started) = crate::test_support::GatedProvider::stop_gate();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        gate.with_release(release_rx);
        let (conversation, _) = crate::test_support::conversation_with(
            &sessions,
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
            assert!(controls.cancellation.is_cancelled());
            assert!(conversation.pending_controls().is_empty());
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
        let home = crate::test_support::temp_sessions();
        let sessions = home.path().join("sessions");
        let (gate, started) = crate::test_support::GatedProvider::stop_gate();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        gate.with_release(release_rx);
        let (conversation, _) = crate::test_support::conversation_with(
            &sessions,
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
            conversation.promote_follow_up(&queued.control_id),
            Err(ConversationControlError::NotRunning)
        ));
        assert_eq!(
            conversation.pending_controls()[0].control_id,
            queued.control_id
        );
        let _ = release_tx.send(());
        worker
            .join()
            .expect("worker")
            .expect("queued follow-up still executes");
    }
}
