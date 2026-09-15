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

use singularity_agent::agent::{TurnInbox, TurnInboxHandle};
use singularity_agent::session::{
    ControlChannel, ControlDisposition, ControlRequest, SessionWriter, control_id, lock_writer,
};
use singularity_core::CancellationToken;
use singularity_protocol::{ControlSnapshot, SessionPhase};
use uuid::Uuid;

use crate::error::TurnRunError;
use crate::runner::{TurnOutcome, TurnRunResult, TurnRunner};
use singularity_protocol::TurnEvent;
use singularity_protocol::{Thread, TurnStatus};

/// 一个活动 turn 的控制集合，仅在设置变更时共享其写者。
pub(crate) struct TurnControls {
    pub(crate) turn_id: String,
    pub cancellation: CancellationToken,
    pub(crate) inbox: TurnInboxHandle,
    accepting_cancel: Mutex<bool>,
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
            cancellation: CancellationToken::new(),
            inbox,
            accepting_cancel: Mutex::new(true),
            writer,
            context_window: std::sync::OnceLock::new(),
        }
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

    /// 在提交本轮终态前冻结用户是否已停止本轮。
    pub(crate) fn finish_cancel(&self) -> bool {
        let mut accepting = self
            .accepting_cancel
            .lock()
            .expect("cancel window lock poisoned");
        *accepting = false;
        self.cancellation.is_cancelled()
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

/// 一次显式 turn 输入，或一个已识别身份的排队输入。正文只存在于
/// 本枚举中：`Explicit` 直接持有，`Accepted` 保存在其控制请求内。
#[derive(Clone)]
pub(crate) enum ChainInput {
    Explicit(String),
    Accepted(ControlRequest),
}

impl ChainInput {
    pub(crate) fn control(&self) -> Option<&ControlRequest> {
        match self {
            Self::Explicit(_) => None,
            Self::Accepted(request) => Some(request),
        }
    }

    fn control_id(&self) -> Option<&str> {
        self.control().map(|request| request.control_id.as_str())
    }

    /// 本次执行的输入正文；两种表示各有一处唯一来源。
    pub(crate) fn text(&self) -> &str {
        match self {
            Self::Explicit(text) => text,
            Self::Accepted(request) => &request.text,
        }
    }

    /// 未消费时交回队列的控制身份；普通显式输入没有可归还的身份。
    pub(crate) fn into_unconsumed(self) -> Option<ControlRequest> {
        match self {
            Self::Explicit(_) => None,
            Self::Accepted(request) => Some(request),
        }
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

/// 按 control_id 定位未消费的队列项；身份不存在时统一报告 ControlNotFound。
fn locate_pending_follow_up(
    queue: &VecDeque<ChainInput>,
    control_id: &str,
) -> Result<usize, ConversationControlError> {
    queue
        .iter()
        .position(|input| input.control_id() == Some(control_id))
        .ok_or(ConversationControlError::ControlNotFound)
}

struct ConversationState {
    thread: Thread,
    turn: TurnLifecycle,
    /// 已接受的后续 turn 输入，按提交顺序 FIFO 执行；条目携带接受序号。
    pending_follow_ups: VecDeque<ChainInput>,
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

    /// 返回尚未开始的队列输入。
    fn pending_controls(&self) -> Vec<ControlSnapshot> {
        self.pending_follow_ups
            .iter()
            .filter_map(ChainInput::control)
            .map(|request| request.snapshot(ControlDisposition::Pending))
            .collect()
    }

    /// 生成下一个控制请求及其回执：turn 身份取当前活动 turn（无活动 turn 一律
    /// 拒绝），接受序号在此处推进一次。正文为空的请求不占用序号。
    fn next_control(
        &mut self,
        channel: ControlChannel,
        text: String,
    ) -> Result<(ControlRequest, ControlSnapshot), ConversationControlError> {
        let Some(turn_id) = self.turn.active().map(|controls| controls.turn_id.clone()) else {
            return Err(ConversationControlError::NotRunning);
        };
        if text.trim().is_empty() {
            return Err(ConversationControlError::InvalidInput);
        }
        let sequence = self.control_sequence;
        self.control_sequence = sequence + 1;
        let request = ControlRequest {
            control_id: control_id(&turn_id, channel, sequence),
            turn_id,
            channel,
            sequence,
            text,
        };
        let snapshot = request.snapshot(ControlDisposition::Pending);
        Ok((request, snapshot))
    }

    /// 排队一条后续 turn 输入，保留其身份与接受序号。
    fn queue_follow_up(
        &mut self,
        text: String,
    ) -> Result<ControlSnapshot, ConversationControlError> {
        let (request, snapshot) = self.next_control(ControlChannel::FollowUp, text)?;
        insert_by_sequence(&mut self.pending_follow_ups, ChainInput::Accepted(request));
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
        cancellation: CancellationToken,
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
            Self::Running(controls) if controls.cancellation.is_cancelled() => {
                SessionPhase::Stopping
            }
            Self::Running(_) => SessionPhase::Running,
            Self::Compacting { cancellation, .. } if cancellation.is_cancelled() => {
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
        // 提升出的输入有独立执行入口，误用在这里直接失败，不静默丢掉它。
        if self.promoted_input.is_some() {
            return Err(ConversationError::Configuration(
                "turn reservation carries a promoted follow-up; run_promoted executes it"
                    .to_string(),
            ));
        }
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
            state: Mutex::new(ConversationState {
                thread,
                turn: TurnLifecycle::Idle,
                pending_follow_ups: VecDeque::new(),
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
        let (request, snapshot) = state.next_control(ControlChannel::Steer, text.into())?;
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
        let position = locate_pending_follow_up(&state.pending_follow_ups, control_id)?;
        if matches!(
            state.turn,
            TurnLifecycle::Reserved | TurnLifecycle::Compacting { .. }
        ) {
            return Err(ConversationControlError::NotRunning);
        }
        // 就地改写已定位的队列项：身份、接受序号与队列位置都由原项保留。
        let ChainInput::Accepted(request) = &mut state.pending_follow_ups[position] else {
            return Err(ConversationControlError::ControlNotFound);
        };
        request.text = text;
        Ok(request.snapshot(ControlDisposition::Pending))
    }

    /// 将指定 pending follow-up 原子提升为当前 turn 的输入，或在空闲时提升为
    /// 下一条独占执行预订。所有权转移只使用 Conversation 状态锁与当前 turn
    /// inbox 锁：注入窗口已关闭时原队列项保持原位；空闲预订未执行即销毁时，
    /// 预订守卫把同一条输入放回队列。
    pub fn promote_follow_up(
        self: &Arc<Self>,
        control_id: &str,
    ) -> Result<FollowUpPromotion, ConversationControlError> {
        // 空闲分支发布预订窗口，因此与写者窗口串行。
        let _window = self.lock_writer_window();
        let mut state = self.lock_state();
        let position = locate_pending_follow_up(&state.pending_follow_ups, control_id)?;

        match &state.turn {
            TurnLifecycle::Running(controls) => {
                // inbox 消费请求并可能拒绝；拒绝时原队列项必须留在原位，
                // 因此这一次转交保留待转交请求的副本。
                let request = state.pending_follow_ups[position]
                    .control()
                    .cloned()
                    .ok_or(ConversationControlError::ControlNotFound)?;
                let snapshot = request.snapshot(ControlDisposition::Pending);
                if !controls.enqueue(request) {
                    return Err(ConversationControlError::NotRunning);
                }
                state
                    .pending_follow_ups
                    .remove(position)
                    .expect("located follow-up remains present under the state lock");
                Ok(FollowUpPromotion::Injected(snapshot))
            }
            TurnLifecycle::Idle => {
                // 空闲提升把原项整体交给预订守卫，不需要中间副本。
                let snapshot = state.pending_follow_ups[position]
                    .control()
                    .ok_or(ConversationControlError::ControlNotFound)?
                    .snapshot(ControlDisposition::Pending);
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
        let position = locate_pending_follow_up(&state.pending_follow_ups, control_id)?;
        if matches!(
            state.turn,
            TurnLifecycle::Reserved | TurnLifecycle::Compacting { .. }
        ) {
            return Err(ConversationControlError::NotRunning);
        }
        let snapshot = state.pending_follow_ups[position]
            .control()
            .ok_or(ConversationControlError::ControlNotFound)?
            .snapshot(ControlDisposition::Cancelled);
        state.pending_follow_ups.remove(position);
        Ok(snapshot)
    }

    /// 为独立压缩预订唯一操作窗口，并公开共享写者供设置立即保存。
    /// 写者打开在状态锁之外完成，见模块文档「锁」。
    pub fn reserve_compaction(
        self: &Arc<Self>,
        cancellation: CancellationToken,
    ) -> Result<TurnReservation, ConversationError> {
        let _window = self.lock_writer_window();
        let thread = {
            let state = self.lock_state();
            if state.turn.is_busy() || !state.pending_follow_ups.is_empty() {
                return Err(ConversationError::TurnAlreadyActive);
            }
            state.thread.clone()
        };
        let writer = self.runner.open_turn_writer(&thread)?;
        self.lock_state().turn = TurnLifecycle::Compacting {
            thread,
            writer,
            cancellation,
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
            TurnLifecycle::Compacting { cancellation, .. } => {
                cancellation.cancel();
                Ok(())
            }
            _ => Err(ConversationControlError::NotRunning),
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
        crate::runner::record_thread_settings_metadata(&mut lock_writer(&writer), &updated)
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
            let TurnRunResult {
                result,
                undelivered,
            } = self.run_single_turn(current, sink);
            let retained: VecDeque<ChainInput> =
                undelivered.into_iter().map(ChainInput::Accepted).collect();
            match result {
                Err(error) => {
                    self.requeue_follow_ups(retained);
                    return Err(error.into());
                }
                Ok(outcome) if outcome.turn_status == TurnStatus::Interrupted => {
                    // 中断时未交付的控制已由 runner 逐个发布 Cancelled 事件
                    // （不落盘）；内部队列只保留跨 turn 的后续输入。
                    return Ok(outcome);
                }
                Ok(outcome) => {
                    self.requeue_follow_ups(retained);
                    last = Some(outcome);
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
        current: ChainInput,
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
                        undelivered: current.into_unconsumed().into_iter().collect(),
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
            conversation.snapshot().pending_controls[0].control_id,
            queued.control_id
        );
        let _ = release_tx.send(());
        worker
            .join()
            .expect("worker")
            .expect("queued follow-up still executes");
    }
}
