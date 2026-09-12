//! Thread 的长驻协调器：单活动 turn、控制接受顺序、后续输入队列、取消与设置生效时序。
//!
//! Conversation 是无交互入口与 Web 工作台共用的生命周期状态机。它不实现任何
//! 执行细节：turn 体完全委托给 crate::TurnRunner，这里维护长驻事实：
//!
//! - 「同一 Thread 至多一个活动 turn」的不变量；
//! - 控制接受的唯一 FIFO 序号：steer、followUp 与 cancel 三条通道共用一个
//!   单调计数器，接受顺序即落盘 control_accepted.sequence 的顺序；
//! - steer 注入窗口（活动 turn 的 Agent 收件箱）与取消令牌；
//! - followUp 后续输入队列：活动 turn 期间接受的每条 followUp 在当前 turn
//!   到达可信终态后按提交顺序自动启动为一个新的 turn，每条恰好执行一次；
//!   队列条目携带接受序号，后续 turn 启动时由 runner 落 control_accepted
//!   （disposition started_as_new_turn）；cancel 接受时记入活动控制面的
//!   取消日志，本轮终态落盘前由 runner 落 control_accepted
//!   （disposition cancelled）——进程内队列只是这些 durable 事实的运行时投影；
//! - 设置提交时立即持久化，成功后更新下一轮选择；活动 turn 或压缩的模型快照保持不变。
//!   活动操作复用唯一会话写者，空闲时短开写者。
//! - 控制命令（steer、followUp 接受、编辑、撤回与取消）都在一次生命周期临界区内
//!   完成接受检查、durable 落盘与内存更新：活动控制句柄不离开临界区，写者寿命
//!   服从 Running → Reserved 交接，旧写者的文件锁在新写者打开前关闭。
//!
//! 结果语义与可信终态：Conversation::run_turn 对任何已落盘的可信终态
//! （completed/failed/interrupted）返回 Ok(TurnOutcome)——失败终态携带
//! 协议错误细节；Err 只表示不存在可信终态（准备失败、终态化失败、并发
//! 占用），评估器与客户端因此无需从事件重建终态事实。
//!
//! 锁失效策略
//!
//! 锁中毒只可能源自本进程自身临界区内的 panic，届时任何投影都不可信：
//! 所有锁访问 fail-stop，中毒即直接 panic 退出（进程边界负责恢复
//! 终端）。写盘失败是另一条真实通道，经 note_storage_failure 记录并在
//! 终态检查处收敛为失败。

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
use crate::events::TurnEvent;
use crate::objects::{Thread, TurnStatus};
use crate::runner::{TurnOutcome, TurnParams, TurnRunResult, TurnRunner};

/// 一个活动 turn 的控制面：调用方在执行期间持有，用于取消与实时转向注入。
///
/// 构造即完整：turn id、注入箱句柄与本轮共享会话写者在构造时一次性绑定，
/// 注入窗口在 turn 开始前即已就绪；终态化前由 runner 关闭注入窗口。
/// control_sequence 是协调器唯一的控制接受 FIFO 计数器（steer/followUp/
/// cancel 共用）；每次成功接受消耗一个序号，序号即 durable
/// control_accepted.sequence。cancel_acceptances 在 Some 时表示仍接受取消，
/// 并暂存本 turn 已接受的请求；runner 在终态记录落盘前原子关闭窗口并取走
/// 全部请求（durable-before-publish）。
///
/// durable 接受纪律：steer/followUp 在报告 accepted、影响执行或
/// 发布可见事实之前，先经本轮唯一会话写者落 control_accepted(pending)
/// 接受记录；落盘失败即返回存储错误。写者与执行线程共用
/// 同一 SessionManager 实例（短暂加锁串行追加），不存在绕过
/// SessionManager 的第二写者。取消先触发令牌，日志失败不阻止停止。
pub(crate) struct TurnControls {
    pub(crate) turn_id: String,
    pub cancellation: CancellationToken,
    pub(crate) inbox: TurnInboxHandle,
    control_sequence: Arc<AtomicU64>,
    cancel_acceptances: Mutex<Option<Vec<ControlRequest>>>,
    storage_failure: Mutex<Option<String>>,
    writer: SessionWriter,
    projection: Arc<ControlProjection>,
    /// runner 在 start_turn 解析出的本轮冻结模型配置；公开快照据此报告
    /// 有效上下文窗口，不随后续配置编辑改变。
    model: std::sync::OnceLock<ModelConfigurationSnapshot>,
}

// fail-stop 锁策略：中毒 panic 直接显式（见模块文档「锁失效策略」）。
#[allow(clippy::expect_used)]
impl TurnControls {
    pub fn new(
        turn_id: impl Into<String>,
        inbox: TurnInboxHandle,
        control_sequence: Arc<AtomicU64>,
        writer: SessionWriter,
        projection: Arc<ControlProjection>,
    ) -> Self {
        Self {
            turn_id: turn_id.into(),
            cancellation: CancellationToken::new(),
            inbox,
            control_sequence,
            cancel_acceptances: Mutex::new(Some(Vec::new())),
            storage_failure: Mutex::new(None),
            writer,
            projection,
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

    /// durable 接受记录：先落盘 pending 接受，失败即拒绝（不报告 accepted）。
    fn append_pending(&self, request: &ControlRequest) -> Result<(), ConversationControlError> {
        self.append_control(request, ControlDisposition::Pending)
            .map_err(ConversationControlError::Storage)
    }

    /// 活动 turn 共用写者追加控制事实；失败同时反馈调用方与本轮终态处理。
    pub(crate) fn append_control(
        &self,
        request: &ControlRequest,
        disposition: ControlDisposition,
    ) -> Result<(), String> {
        lock_writer(&self.writer)
            .append_record(request.record(disposition))
            .map(|_| ())
            .map_err(|error| {
                let message = error.to_string();
                self.note_storage_failure(message.clone());
                message
            })?;
        self.record_control(request.snapshot(disposition));
        Ok(())
    }

    pub(crate) fn record_control(&self, control: ControlSnapshot) {
        self.projection.record(control);
    }

    /// 把转向输入注入当前 turn：先 durable 落盘 pending 接受记录，成功后才
    /// 入箱（报告 accepted / 影响执行）。注入窗口已关闭时入箱失败，durable
    /// 收敛为 cancelled——不存在「已接受但无归宿」的输入。
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
            text: Some(text),
        };
        self.append_pending(&request)?;
        let enqueued = self.lock_inbox().enqueue(request.clone());
        if !enqueued {
            // 注入窗口已关闭：不留下无归宿的 pending 记录。
            self.append_control(&request, ControlDisposition::Cancelled)
                .map_err(ConversationControlError::Storage)?;
            return Err(ConversationControlError::NotRunning);
        }
        Ok(request.snapshot(ControlDisposition::Pending))
    }

    /// 接受检查、pending 落盘与内存归属和 runner 的关闭交接共用一把短锁。
    /// 已关闭时不再触发本轮令牌；窗口内则先发取消信号，再尝试写盘，写盘失败
    /// 仍不延迟当前任务停止。
    fn accept_cancel(&self) -> Result<ControlSnapshot, ConversationControlError> {
        let mut window = self.lock_cancel_acceptances();
        let acceptances = window
            .as_mut()
            .ok_or(ConversationControlError::NotRunning)?;
        self.cancellation.cancel();
        let sequence = self.control_sequence.fetch_add(1, Ordering::Relaxed);
        let request = ControlRequest {
            control_id: control_id(&self.turn_id, ControlChannel::Cancel, sequence),
            turn_id: self.turn_id.clone(),
            channel: ControlChannel::Cancel,
            sequence,
            text: None,
        };
        self.append_pending(&request)?;
        let snapshot = request.snapshot(ControlDisposition::Pending);
        acceptances.push(request);
        Ok(snapshot)
    }

    /// 原子关闭取消接受窗口并取走此前接受的全部请求。关闭后 abort 明确拒绝，
    /// 因而不能在终态交接之后再产生无人收敛的 pending cancel。
    pub(crate) fn close_cancel_acceptances(&self) -> Vec<ControlRequest> {
        self.lock_cancel_acceptances().take().unwrap_or_default()
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

    fn lock_cancel_acceptances(&self) -> std::sync::MutexGuard<'_, Option<Vec<ControlRequest>>> {
        self.cancel_acceptances
            .lock()
            .expect("control journal lock poisoned (fail-stop)")
    }

    fn note_storage_failure(&self, message: String) {
        let mut failure = self
            .storage_failure
            .lock()
            .expect("storage failure lock poisoned (fail-stop)");
        if failure.is_none() {
            *failure = Some(message);
        }
    }

    pub(crate) fn take_storage_failure(&self) -> Option<String> {
        self.storage_failure
            .lock()
            .expect("storage failure lock poisoned (fail-stop)")
            .take()
    }
}

/// 链队列中的一条输入：显式提交没有控制请求（它本身就是回合意图），
/// 协调器接受的 followUp/requeued steer 携带其 durable 控制请求，由后续
/// turn 落 control_accepted 终态 disposition 记录。
#[derive(Clone)]
enum ChainInput {
    Explicit(String),
    Accepted(ControlRequest),
}

pub(crate) struct ControlProjection(Mutex<Vec<ControlSnapshot>>);

impl ControlProjection {
    pub(crate) fn new(controls: Vec<ControlSnapshot>) -> Self {
        Self(Mutex::new(controls))
    }

    #[allow(clippy::expect_used)]
    fn snapshot(&self) -> Vec<ControlSnapshot> {
        self.0
            .lock()
            .expect("control projection lock poisoned (fail-stop)")
            .clone()
    }

    #[allow(clippy::expect_used)]
    fn record(&self, control: ControlSnapshot) {
        let mut controls = self
            .0
            .lock()
            .expect("control projection lock poisoned (fail-stop)");
        match controls
            .iter()
            .position(|existing| existing.control_id == control.control_id)
        {
            Some(index) => controls[index] = control,
            None => {
                controls.push(control);
                controls.sort_by_key(|entry| entry.sequence);
            }
        }
    }
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
    /// 链窗口代数：每次成功预订递增。释放只清自己代数开启的窗口
    /// （终局清理前核对代数身份），杜绝旧凭证
    /// drop 踩掉新预订。
    reservation_seq: u64,
    /// 已接受的后续 turn 输入，按提交顺序 FIFO 执行；条目携带接受序号。
    pending_follow_ups: VecDeque<ChainInput>,
    /// 最近一次执行的冻结模型配置：解释最近请求用量的事实，不随设置
    /// 编辑改变；进程重启后不可知。
    last_model: Option<ModelConfigurationSnapshot>,
}

/// 释放链窗口：仅当 seq 仍是当前代数时回收为 Idle；代数不符（窗口已属
/// 更新一次预订）时不做任何事。
fn release_turn_window(state: &mut ConversationState, seq: u64) {
    if state.reservation_seq == seq {
        state.turn = TurnLifecycle::Idle;
    }
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
    /// 控制接受的唯一 FIFO 序号：steer/followUp/cancel 共用，接受顺序即
    /// durable control_accepted.sequence 顺序。随构造起、随对象灭。
    control_sequence: Arc<AtomicU64>,
    control_projection: Arc<ControlProjection>,
    /// Thread 设置、活动阶段与待处理输入由同一把锁协调。
    state: Mutex<ConversationState>,
}

/// 单活动 turn 的执行权预订。
///
/// Conversation::reserve_start 原子开启链窗口；Self::run 执行整条链，
/// 调用方在投影收尾后销毁预订并释放窗口。drop 释放带
/// 窗口代数核对：只回收自己开启的窗口，执行中途 panic 也不会泄漏活动窗口。
pub struct TurnReservation {
    conversation: Arc<Conversation>,
    seq: u64,
    promoted_input: Option<ChainInput>,
}

impl TurnReservation {
    /// 执行本轮输入及后续队列，直至链条结束；窗口保持到预订 drop。
    /// durable 控制处置变化经同一事件出口带类型发布。
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
    /// follow-up，并沿用原 control identity 与 durable pending 事实。
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
        release_turn_window(&mut state, self.seq);
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
    #[error("{0}")]
    Storage(String),
}

// fail-stop 锁策略：中毒 panic 直接显式（见模块文档「锁失效策略」）。
#[allow(clippy::expect_used)]
impl Conversation {
    pub fn new(runner: Arc<TurnRunner>, thread: Thread) -> Result<Arc<Self>, ConversationError> {
        let (controls, pending, next_sequence) = runner
            .load_control_state(&thread)
            .map_err(ConversationError::Session)?;
        let mut pending_follow_ups = VecDeque::new();
        for request in pending {
            insert_by_sequence(&mut pending_follow_ups, ChainInput::Accepted(request));
        }
        Ok(Arc::new(Self {
            runner,
            control_sequence: Arc::new(AtomicU64::new(next_sequence)),
            control_projection: Arc::new(ControlProjection::new(controls)),
            state: Mutex::new(ConversationState {
                thread,
                turn: TurnLifecycle::Idle,
                reservation_seq: 0,
                pending_follow_ups,
                last_model: None,
            }),
        }))
    }

    /// 原子预订单活动 turn 的链窗口：窗口内其他预订与 run_turn 立即被
    /// 拒绝；窗口可被 TurnReservation::run 消费执行整条链，或由 drop
    /// 释放。
    pub fn reserve_start(self: &Arc<Self>) -> Result<TurnReservation, ConversationError> {
        let mut state = self.lock_state();
        if state.turn.is_busy() {
            return Err(ConversationError::TurnAlreadyActive);
        }
        state.reservation_seq = state.reservation_seq.wrapping_add(1);
        let seq = state.reservation_seq;
        state.turn = TurnLifecycle::Reserved;
        Ok(TurnReservation {
            conversation: Arc::clone(self),
            seq,
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
    /// 接受检查、durable 落盘与投影更新在一次生命周期临界区内完成：控制
    /// 命令不携带活动控制句柄离开临界区，写者寿命因此直接服从生命周期
    /// 交接，不会把旧写者的文件锁拖进下一轮的写者打开路径。
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

    /// 接受一条 followUp：先经活动 turn 的唯一会话写者 durable 落盘 pending
    /// 接受记录（携带控制 identity、payload 与 FIFO sequence），成功后才加入
    /// Thread 的后续输入队列，在当前 turn 到达可信终态后按 FIFO 启动为一个
    /// 新的 turn。活动 turn 不存在（含预订阶段）或 durable 接受失败时拒绝
    /// 并返回错误，调用方应在空闲时改以普通 turn 提交。
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
            text: Some(text),
        };
        controls.append_pending(&request)?;
        let snapshot = request.snapshot(ControlDisposition::Pending);
        insert_by_sequence(&mut state.pending_follow_ups, ChainInput::Accepted(request));
        Ok(snapshot)
    }

    /// durable control ledger 的当前完整归约投影。
    pub fn controls(&self) -> Vec<ControlSnapshot> {
        self.control_projection.snapshot()
    }

    pub fn pending_controls(&self) -> Vec<ControlSnapshot> {
        self.lock_state()
            .pending_follow_ups
            .iter()
            .filter_map(ChainInput::control)
            .map(|request| request.snapshot(ControlDisposition::Pending))
            .collect()
    }

    /// 原子更新一条 pending follow-up 的文本。identity 与 FIFO sequence
    /// 保持不变；新文本先追加到同一 durable 控制事实，成功后才替换内存队列。
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
        request.text = Some(text);
        let snapshot = request.snapshot(ControlDisposition::Pending);
        match &state.turn {
            // 活动写者路径在 append_pending 内完成落盘与投影更新。
            TurnLifecycle::Running(controls) => controls.append_pending(&request)?,
            TurnLifecycle::Idle => {
                self.runner
                    .append_control_record(
                        &state.thread,
                        request.record(ControlDisposition::Pending),
                    )
                    .map_err(ConversationControlError::Storage)?;
                self.control_projection.record(snapshot.clone());
            }
            TurnLifecycle::Reserved | TurnLifecycle::Compacting { .. } => {
                return Err(ConversationControlError::NotRunning);
            }
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
                state.reservation_seq = state.reservation_seq.wrapping_add(1);
                let seq = state.reservation_seq;
                state.turn = TurnLifecycle::Reserved;
                Ok(FollowUpPromotion::Reserved {
                    control: snapshot,
                    reservation: TurnReservation {
                        conversation: Arc::clone(self),
                        seq,
                        promoted_input: Some(input),
                    },
                })
            }
            TurnLifecycle::Reserved | TurnLifecycle::Compacting { .. } => {
                Err(ConversationControlError::NotRunning)
            }
        }
    }

    /// 撤回最近加入队列、尚未开始执行的一条 followUp。撤回是用户显式取消：
    /// 在一次生命周期临界区内先 durable 落盘 cancelled（活动 turn 内经共享
    /// 写者，空闲时短开写者，预订与压缩阶段拒绝），成功后才移出队列；落盘
    /// 失败时队列保持原样并返回存储错误，绝不静默丢输入。
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
        let persisted = match &state.turn {
            // 活动写者路径在 append_control 内完成落盘与投影更新；写失败
            // 记入本轮存储失败通道，终态处理按 fail-stop 收敛。
            TurnLifecycle::Running(controls) => controls
                .append_control(&request, ControlDisposition::Cancelled)
                .map_err(|error| {
                    ConversationControlError::Storage(format!(
                        "failed to persist control withdrawal: {error}"
                    ))
                }),
            TurnLifecycle::Idle => self
                .runner
                .append_control_record(&state.thread, request.record(ControlDisposition::Cancelled))
                .map_err(|error| {
                    ConversationControlError::Storage(format!(
                        "failed to persist control withdrawal: {error}"
                    ))
                })
                .map(|()| {
                    self.control_projection
                        .record(request.snapshot(ControlDisposition::Cancelled))
                }),
            TurnLifecycle::Reserved | TurnLifecycle::Compacting { .. } => {
                Err(ConversationControlError::NotRunning)
            }
        };
        persisted?;
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
        state.reservation_seq = state.reservation_seq.wrapping_add(1);
        let seq = state.reservation_seq;
        state.turn = TurnLifecycle::Compacting {
            thread,
            writer,
            cancellation,
        };
        Ok(TurnReservation {
            conversation: Arc::clone(self),
            seq,
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

    /// 取消当前操作。普通执行返回持久控制记录，独立压缩只取消自己的令牌。
    pub fn abort(&self) -> Result<Option<ControlSnapshot>, ConversationControlError> {
        match &self.lock_state().turn {
            TurnLifecycle::Running(controls) => controls.accept_cancel().map(Some),
            TurnLifecycle::Compacting { cancellation, .. } => {
                cancellation.cancel();
                Ok(None)
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
    ///    设置变更由每个 turn 开始时在会话中记录（见 TurnRunner::run）；
    /// 3. 按 FIFO 启动已接受的 followUp 为新的 turn（各自独立 turn id），
    ///    直到队列清空；执行期间新提交的 followUp 同样被消费。
    ///
    /// 失败语义：任何已落盘的可信终态都返回 Ok（失败终态携带
    /// crate::events::TurnErrorDetail，不阻断队列中其余 followUp）；
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
            crate::runner::record_thread_settings_metadata(&mut lock_writer(&writer), &thread)?;
            let controls = Arc::new(TurnControls::new(
                Uuid::new_v4().to_string(),
                TurnInbox::default_handle(),
                Arc::clone(&self.control_sequence),
                writer,
                Arc::clone(&self.control_projection),
            ));
            state.turn = TurnLifecycle::Running(Arc::clone(&controls));
            (thread, controls)
        };
        let (input, control) = match current {
            ChainInput::Explicit(text) => (text, None),
            ChainInput::Accepted(request) => {
                (request.text.clone().unwrap_or_default(), Some(request))
            }
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
            // 生命周期并释放本函数持有的控制句柄，旧写者的文件锁随之在锁内
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
    fn cancellation_signals_even_when_its_journal_cannot_be_written() {
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
            Arc::new(ControlProjection::new(Vec::new())),
        );
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(matches!(
            controls.accept_cancel(),
            Err(ConversationControlError::Storage(_))
        ));
        assert!(controls.cancellation.is_cancelled());
        assert!(controls.take_storage_failure().is_some());
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn closing_cancel_acceptance_waits_for_an_acceptance_already_in_progress() {
        let dir = tempfile::tempdir().unwrap();
        let session = singularity_agent::session::SessionManager::create(
            dir.path(),
            &dir.path().join("sessions"),
        )
        .unwrap();
        let writer = Arc::new(Mutex::new(session));
        let controls = Arc::new(TurnControls::new(
            "turn",
            TurnInbox::default_handle(),
            Arc::new(AtomicU64::new(0)),
            Arc::clone(&writer),
            Arc::new(ControlProjection::new(Vec::new())),
        ));
        let writer_guard = lock_writer(&writer);
        let accepter = {
            let controls = Arc::clone(&controls);
            std::thread::spawn(move || controls.accept_cancel())
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !controls.cancellation.is_cancelled() {
            assert!(
                std::time::Instant::now() < deadline,
                "cancel acceptance did not reach the persistence boundary"
            );
            std::thread::yield_now();
        }
        let closer = {
            let controls = Arc::clone(&controls);
            std::thread::spawn(move || controls.close_cancel_acceptances())
        };
        drop(writer_guard);

        let accepted = accepter.join().unwrap().unwrap();
        let transferred = closer.join().unwrap();
        assert_eq!(transferred.len(), 1);
        assert_eq!(transferred[0].control_id, accepted.control_id);
        assert!(matches!(
            controls.accept_cancel(),
            Err(ConversationControlError::NotRunning)
        ));
    }

    fn state(turn: TurnLifecycle, reservation_seq: u64) -> ConversationState {
        ConversationState {
            thread: Thread {
                thread_id: "t-1".to_string(),
                model: None,
                cwd: String::new(),
            },
            turn,
            last_model: None,
            reservation_seq,
            pending_follow_ups: VecDeque::new(),
        }
    }

    /// 回归：链尾提前关闭窗口后，旧预订在销毁前若被新预订超越（代数已推进），
    /// 旧预订的 drop 不得踩掉新窗口；自己的窗口正常回收。
    #[test]
    fn release_only_clears_the_window_it_opened() {
        let mut state = state(TurnLifecycle::Reserved, 2);
        release_turn_window(&mut state, 1);
        assert!(matches!(state.turn, TurnLifecycle::Reserved));
        release_turn_window(&mut state, 2);
        assert!(matches!(state.turn, TurnLifecycle::Idle));
    }

    /// 控制命令在一次生命周期临界区内完成：cancel 在翻转取消令牌后等待
    /// durable 写入，此时其他控制观察无法越过它执行；命令完成后链条照常
    /// 交接，后续输入的写者打开不与旧句柄冲突。
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

        // 占住共享写者：cancel 的 durable 写入被阻塞。cancel 先翻转取消令牌
        // 再等待写盘，令牌翻转即可证明它已持有生命周期临界区。测试自己的
        // 写者句柄在块内释放，模拟的是一次普通的控制请求。
        {
            let controls = conversation.active_controls().expect("active controls");
            let writer = controls.writer();
            let writer_guard = lock_writer(&writer);
            let aborted = {
                let conversation = Arc::clone(&conversation);
                std::thread::spawn(move || conversation.abort())
            };
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            while !controls.cancellation.is_cancelled() {
                assert!(
                    std::time::Instant::now() < deadline,
                    "cancel acceptance did not reach the persistence boundary"
                );
                std::thread::yield_now();
            }
            // 临界区被 cancel 占住：不需要写者的控制观察也无法越过它执行。
            let (probed_tx, probed_rx) = std::sync::mpsc::channel();
            let probe = {
                let conversation = Arc::clone(&conversation);
                std::thread::spawn(move || {
                    let pending = conversation.pending_controls();
                    let _ = probed_tx.send(());
                    pending
                })
            };
            assert!(
                probed_rx
                    .recv_timeout(std::time::Duration::from_millis(300))
                    .is_err(),
                "a control command blocked on the shared writer keeps the lifecycle critical section"
            );
            drop(writer_guard);
            aborted
                .join()
                .unwrap()
                .expect("cancel is accepted while the turn is running");
            probe.join().unwrap();
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
