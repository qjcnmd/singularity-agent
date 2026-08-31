//! Thread 的长驻协调器：单活动 turn、控制接受顺序、后续输入队列、取消与设置生效时序。
//!
//! [`Conversation`] 是无交互入口与 TUI 共用的生命周期状态机。它不实现任何
//! 执行细节：turn 体完全委托给 [`crate::TurnRunner`]，这里维护长驻事实：
//!
//! - 「同一 Thread 至多一个活动 turn」的不变量；
//! - 控制接受的唯一 FIFO 序号：steer、followUp 与 cancel 三条通道共用一个
//!   单调计数器，接受顺序即落盘 `control_accepted.sequence` 的顺序；
//! - steer 注入窗口（活动 turn 的 Agent 收件箱）与取消令牌；
//! - followUp 后续输入队列：活动 turn 期间接受的每条 followUp 在当前 turn
//!   到达可信终态后按提交顺序自动启动为一个新的 turn，每条恰好执行一次；
//!   队列条目携带接受序号，后续 turn 启动时由 runner 落 `control_accepted`
//!   （disposition `started_as_new_turn`）；cancel 接受时记入活动控制面的
//!   取消日志，本轮终态落盘前由 runner 落 `control_accepted`
//!   （disposition `cancelled`）——进程内队列只是这些 durable 事实的运行时投影；
//! - 设置生效时序：变更提交点只做校验与内存投影更新（运行中同样接受），
//!   落盘发生在 turn 开始时由 turn 在自己的会话写者上记录（turn 边界记录），本对象不持有设置持久化状态。
//!
//! 结果语义与可信终态：[`Conversation::run_turn`] 对任何已落盘的可信终态
//! （completed/failed/interrupted）返回 `Ok(TurnOutcome)`——失败终态携带
//! 协议错误细节；`Err` 只表示不存在可信终态（准备失败、终态化失败、并发
//! 占用），评估器与客户端因此无需从事件重建终态事实。
//!
//! # 锁中毒策略
//!
//! 本模块统一采用 fail-closed：`self.state` 持有的 `Mutex` 中毒 = 状态未知，
//! 所有写路径拒绝更改（保持原状或返回错误），所有读路径按 busy/None 收敛，
//! 绝不静默当成功或丢失数据。具体表现在：
//!
//! - `lock_state()`（公开 fail-loud）直接向上传播中毒错误。
//! - `release_reservation`（Drop 上下文，无法传播错误）保持窗口不释放，
//!   使链窗口永久 busy，阻止后续写入。
//! - `submit_follow_up`、`steer`、`active_controls`、`active_turn_id`：
//!   中毒时按无活动 turn 收敛（返回 false/None），阻止后续输入接受。
//! - `requeue_follow_ups`：中毒时向上传播错误，不静默丢弃 followUp 输入。
//! - `close_inbox`：中毒时跳过关闭；它是 Agent 收口已关闭后的二次保险，
//!   跳过不影响正确性。`steer`、`drain_inbox` 的注入箱中毒按拒绝/空
//!   收敛（fail-closed），不把输入写进可能已损坏的收件箱。
//! - `pending_follow_up_count`、`withdraw_follow_up`：读路径按空返回，不展示
//!   可能已损坏的数据。

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use singularity_agent::agent::{TurnInbox, TurnInboxHandle};
use singularity_agent::session::{
    ControlChannel, ControlDisposition, ControlRequest, SessionWriter, control_id, lock_writer,
    reduce_controls,
};
use singularity_core::CancellationToken;
use singularity_model::split_model_selector;
use uuid::Uuid;

use crate::error::{TurnFailureCause, TurnRunError};
use crate::events::TurnEventSink;
use crate::objects::{Thread, TurnStatus};
use crate::runner::{TurnOutcome, TurnParams, TurnRunner};

/// 客户端可修改的当前 Thread 运行时设置。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SettingsPatch {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub reasoning: ReasoningPatch,
}

/// reasoning effort 的字段级修改意图。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum ReasoningPatch {
    #[default]
    Keep,
    Set(String),
    Clear,
}

/// [`Conversation::update_settings`] 的结果：本次修改的生效时点。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsApplyTiming {
    /// 没有可应用的内容（空 patch）。
    NothingToApply,
    /// 已校验并更新内存投影；落盘由下一 turn 开始时记录。
    AppliedNow,
}

impl SettingsPatch {
    pub fn is_empty(&self) -> bool {
        self.provider.is_none()
            && self.model.is_none()
            && matches!(self.reasoning, ReasoningPatch::Keep)
    }
}

/// [`Conversation::update_settings`] 的结果：本次修改的生效时点与合并后的 selector。
///
/// selector 由 runtime 在提交点唯一组合并校验；客户端只投影，不反推。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingsApplyResult {
    pub timing: SettingsApplyTiming,
    /// 合并并校验后的完整 selector（`provider/model[#effort]`）。
    pub selector: String,
}

/// 一个活动 turn 的控制面：调用方在执行期间持有，用于取消与实时转向注入。
///
/// 构造即完整：turn id、注入箱句柄与本轮共享会话写者在构造时一次性绑定，
/// 注入窗口在 turn 开始前即已就绪；终态化前由 runner 关闭注入窗口。
/// `control_sequence` 是协调器唯一的控制接受 FIFO 计数器（steer/followUp/
/// cancel 共用）；每次成功接受消耗一个序号，序号即 durable
/// `control_accepted.sequence`。`cancel_acceptances` 暂存本 turn 已接受的
/// 取消请求，由 runner 在终态记录落盘前写入 ledger（durable-before-publish）。
///
/// durable 接受纪律：steer/followUp/cancel 都在报告 accepted、影响执行或
/// 发布可见事实之前，先经本轮唯一会话写者落 `control_accepted(pending)`
/// 接受记录；落盘失败即拒绝（返回 false / 不触发）。写者与执行线程共用
/// 同一 [`SessionManager`] 实例（短暂加锁串行追加），不存在绕过
/// [`SessionManager`] 的第二写者。
pub struct TurnControls {
    pub(crate) turn_id: String,
    pub cancellation: CancellationToken,
    pub(crate) inbox: TurnInboxHandle,
    control_sequence: Arc<AtomicU64>,
    journal: Mutex<ControlJournal>,
    storage_failure: Mutex<Option<String>>,
    writer: SessionWriter,
}

#[derive(Default)]
struct ControlJournal {
    cancel_acceptances: Vec<ControlRequest>,
    drained_inbox: Vec<ControlRequest>,
}

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
            journal: Mutex::new(ControlJournal::default()),
            storage_failure: Mutex::new(None),
            writer,
        }
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
    fn append_pending(&self, request: &ControlRequest) -> bool {
        match lock_writer(&self.writer).append_record(request.pending_record()) {
            Ok(_) => true,
            Err(error) => {
                self.note_storage_failure(error.to_string());
                false
            }
        }
    }

    /// 终态 disposition 记录（消费或收敛时落盘；payload 已存在于 pending 记录）。
    pub(crate) fn append_disposition(
        &self,
        request: &ControlRequest,
        disposition: ControlDisposition,
    ) -> Result<(), String> {
        lock_writer(&self.writer)
            .append_record(request.disposition_record(disposition))
            .map(|_| ())
            .map_err(|error| {
                let message = error.to_string();
                self.note_storage_failure(message.clone());
                message
            })
    }

    /// 把转向输入注入当前 turn：先 durable 落盘 pending 接受记录，成功后才
    /// 入箱（报告 accepted / 影响执行）。注入窗口已关闭时入箱失败，durable
    /// 收敛为 cancelled——不存在「已接受但无归宿」的输入。
    pub fn steer(&self, text: impl Into<String>) -> bool {
        let text = text.into();
        let sequence = self.control_sequence.fetch_add(1, Ordering::Relaxed);
        let request = ControlRequest {
            control_id: control_id(&self.turn_id, ControlChannel::Steer, sequence),
            turn_id: self.turn_id.clone(),
            channel: ControlChannel::Steer,
            sequence,
            text: Some(text),
        };
        if !self.append_pending(&request) {
            return false;
        }
        let enqueued = match self.inbox.lock() {
            Ok(mut inbox) => inbox.enqueue(request.clone()),
            Err(_) => {
                self.note_storage_failure("turn inbox lock poisoned".to_string());
                false
            }
        };
        if !enqueued {
            // 注入箱锁中毒或窗口已关闭：不留下无归宿的 pending 记录。
            if self
                .append_disposition(&request, ControlDisposition::Cancelled)
                .is_err()
            {
                return false;
            }
        }
        enqueued
    }

    /// 接受对本 turn 的取消：先 durable 落盘 pending 接受记录，成功后才
    /// 记入取消日志并触发令牌取消（影响执行）。落盘失败时不触发取消。
    fn accept_cancel(&self) -> bool {
        let sequence = self.control_sequence.fetch_add(1, Ordering::Relaxed);
        let request = ControlRequest {
            control_id: control_id(&self.turn_id, ControlChannel::Cancel, sequence),
            turn_id: self.turn_id.clone(),
            channel: ControlChannel::Cancel,
            sequence,
            text: None,
        };
        if !self.append_pending(&request) {
            return false;
        }
        let mut journal = match self.journal.lock() {
            Ok(journal) => journal,
            Err(_) => {
                self.note_storage_failure("cancel acceptance journal lock poisoned".to_string());
                if self
                    .append_disposition(&request, ControlDisposition::Cancelled)
                    .is_err()
                {
                    return false;
                }
                return false;
            }
        };
        journal.cancel_acceptances.push(request);
        self.cancellation.cancel();
        true
    }

    /// 取走本 turn 已接受的取消请求（runner 在终态落盘前写入 ledger）。
    pub(crate) fn take_cancel_acceptances(&self) -> Vec<ControlRequest> {
        match self.journal.lock() {
            Ok(mut journal) => std::mem::take(&mut journal.cancel_acceptances),
            Err(_) => {
                self.note_storage_failure("cancel acceptance journal lock poisoned".to_string());
                Vec::new()
            }
        }
    }

    /// Drain the inbox before terminal publication and retain the exact controls
    /// for the coordinator to consume after the runner returns.
    pub(crate) fn drain_inbox_before_terminal(&self) -> Vec<ControlRequest> {
        let drained = match self.inbox.lock() {
            Ok(mut inbox) => inbox.drain(),
            Err(_) => {
                self.note_storage_failure("turn inbox lock poisoned".to_string());
                Vec::new()
            }
        };
        match self.journal.lock() {
            Ok(mut journal) => journal.drained_inbox.extend(drained.iter().cloned()),
            Err(_) => {
                self.note_storage_failure("control journal lock poisoned".to_string());
            }
        }
        drained
    }

    pub(crate) fn take_drained_inbox(&self) -> Vec<ControlRequest> {
        match self.journal.lock() {
            Ok(mut journal) => std::mem::take(&mut journal.drained_inbox),
            Err(_) => {
                self.note_storage_failure("control journal lock poisoned".to_string());
                Vec::new()
            }
        }
    }

    pub(crate) fn close_inbox(&self) {
        // 锁中毒（状态未知）→ 跳过关闭；此方法是 Agent 收口关闭之后的
        // 二次保险。关闭后新输入仍被拒绝，但已接受而未交付的文本保留在
        // 箱内，由终态排水取走并给出归宿——不随句柄丢弃。
        match self.inbox.lock() {
            Ok(mut inbox) => inbox.close(),
            Err(_) => self.note_storage_failure("turn inbox lock poisoned".to_string()),
        }
    }

    fn note_storage_failure(&self, message: String) {
        if let Ok(mut failure) = self.storage_failure.lock()
            && failure.is_none()
        {
            *failure = Some(message);
        }
    }

    pub(crate) fn take_storage_failure(&self) -> Option<String> {
        match self.storage_failure.lock() {
            Ok(mut failure) => failure.take(),
            Err(_) => Some("control storage failure state poisoned".to_string()),
        }
    }
}

/// 链队列中的一条输入：显式提交没有控制请求（它本身就是回合意图），
/// 协调器接受的 followUp/requeued steer 携带其 durable 控制请求，由后续
/// turn 落 `control_accepted` 终态 disposition 记录。
#[derive(Clone)]
struct ChainInput {
    control: Option<ControlRequest>,
    text: String,
}

impl ChainInput {
    fn explicit(text: impl Into<String>) -> Self {
        Self {
            control: None,
            text: text.into(),
        }
    }

    fn accepted(request: ControlRequest) -> Self {
        let text = request.text.clone().unwrap_or_default();
        Self {
            control: Some(request),
            text,
        }
    }

    fn control_id(&self) -> Option<&str> {
        self.control
            .as_ref()
            .map(|request| request.control_id.as_str())
    }
}

/// 按 FIFO sequence 升序插入已接受的输入；显式输入（无控制）追加到队尾。
fn insert_by_sequence(queue: &mut VecDeque<ChainInput>, input: ChainInput) {
    let Some(sequence) = input.control.as_ref().map(|request| request.sequence) else {
        queue.push_back(input);
        return;
    };
    let position = queue
        .iter()
        .position(|existing| {
            existing
                .control
                .as_ref()
                .is_none_or(|request| request.sequence > sequence)
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
}

/// 释放链窗口：仅当 `seq` 仍是当前代数时回收为 Idle；代数不符（窗口已属
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
}

impl TurnLifecycle {
    fn is_busy(&self) -> bool {
        !matches!(self, Self::Idle)
    }

    fn controls(&self) -> Option<Arc<TurnControls>> {
        match self {
            Self::Running(controls) => Some(Arc::clone(controls)),
            Self::Idle | Self::Reserved => None,
        }
    }
}

/// 一个 Thread 的长驻协调器。
pub struct Conversation {
    runner: Arc<TurnRunner>,
    self_weak: std::sync::Weak<Self>,
    /// Headless-only per-execution selector override. The durable Thread model
    /// remains unchanged; each turn receives this value as a model snapshot input.
    model_override: Option<String>,
    /// 控制接受的唯一 FIFO 序号：steer/followUp/cancel 共用，接受顺序即
    /// durable `control_accepted.sequence` 顺序。随构造起、随对象灭。
    control_sequence: Arc<AtomicU64>,
    state: Mutex<ConversationState>,
}

/// 单活动 turn 的执行权预订。
///
/// [`Conversation::reserve_start`] 原子开启链窗口并持有到消费执行；预订由
/// [`Self::run`] 消费执行整条链条，或在未执行时由 drop 释放。drop 释放带
/// 窗口代数核对：只回收自己开启的窗口，执行中途 panic 也不会泄漏活动窗口。
pub struct TurnReservation {
    conversation: Arc<Conversation>,
    seq: u64,
}

impl TurnReservation {
    /// 消费预订：执行本轮输入及后续队列，直至链条结束；窗口由 drop 释放。
    pub fn run(
        self,
        input: &str,
        sink: &mut dyn TurnEventSink,
    ) -> Result<TurnOutcome, ConversationError> {
        self.conversation.run_chain(input, sink)
    }

    /// 访问所属协调器（投影与并发护栏需要它）。
    pub fn conversation(&self) -> &Arc<Conversation> {
        &self.conversation
    }
}

impl Drop for TurnReservation {
    fn drop(&mut self) {
        self.conversation.release_reservation(self.seq);
    }
}

/// 协调层错误。
#[derive(Debug, thiserror::Error)]
pub enum ConversationError {
    #[error("thread already has an active turn")]
    TurnAlreadyActive,
    #[error("{0}")]
    Configuration(String),
    #[error("{0}")]
    State(String),
    #[error(transparent)]
    Turn(#[from] TurnRunError),
}

impl Conversation {
    pub fn new(runner: Arc<TurnRunner>, thread: Thread) -> Arc<Self> {
        Self::new_with_model_override(runner, thread, None)
    }

    pub fn new_with_model_override(
        runner: Arc<TurnRunner>,
        thread: Thread,
        model_override: Option<String>,
    ) -> Arc<Self> {
        Arc::new_cyclic(|weak| Self {
            runner,
            self_weak: weak.clone(),
            control_sequence: Arc::new(AtomicU64::new(0)),
            model_override,
            state: Mutex::new(ConversationState {
                thread,
                turn: TurnLifecycle::Idle,
                reservation_seq: 0,
                pending_follow_ups: VecDeque::new(),
            }),
        })
    }

    /// 原子预订单活动 turn 的链窗口：窗口内其他预订与 `run_turn` 立即被
    /// 拒绝；窗口可被 [`TurnReservation::run`] 消费执行整条链，或由 drop
    /// 释放。
    pub fn reserve_start(&self) -> Result<TurnReservation, ConversationError> {
        let mut state = self.lock_state()?;
        if state.turn.is_busy() {
            return Err(ConversationError::TurnAlreadyActive);
        }
        state.reservation_seq = state.reservation_seq.wrapping_add(1);
        let seq = state.reservation_seq;
        state.turn = TurnLifecycle::Reserved;
        // 不变量：Conversation 由 Arc 持有并注册 self_weak 后才可 reserve_start，upgrade 必成功。
        #[allow(clippy::expect_used)]
        let conversation = self
            .self_weak
            .upgrade()
            .expect("reservation requires a live conversation");
        Ok(TurnReservation { conversation, seq })
    }

    fn release_reservation(&self, seq: u64) {
        if let Ok(mut state) = self.state.lock() {
            release_turn_window(&mut state, seq);
        }
        // 锁中毒（状态未知）时保持窗口不释放：链窗口永久 busy = fail-closed，
        // 宁可让后续预订与提交被拒，也不在状态损坏时静默放行写入。
    }

    pub fn runner_handle(&self) -> Arc<TurnRunner> {
        Arc::clone(&self.runner)
    }

    /// 当前 Thread 投影快照。
    pub fn thread(&self) -> Result<Thread, ConversationError> {
        self.lock_state().map(|state| state.thread.clone())
    }

    /// 当前 Thread 是否有正在执行的 turn（含后续队列的连续执行期）。
    pub fn has_active_turn(&self) -> bool {
        match self.state.lock() {
            Ok(state) => state.turn.is_busy(),
            // 状态未知时保持删除与并发启动护栏关闭，避免把潜在写者误判为空闲。
            Err(_) => true,
        }
    }

    /// 当前 turn id；预订阶段与空闲阶段均无活动 turn。
    pub fn active_turn_id(&self) -> Option<String> {
        // 锁中毒（状态未知）→ 按无活动 turn 收敛（None），与 has_active_turn
        // 的「中毒按 busy」同向：读路径不泄露可能损坏的状态。
        self.state.lock().ok().and_then(|state| match &state.turn {
            TurnLifecycle::Running(controls) => Some(controls.turn_id.clone()),
            TurnLifecycle::Idle | TurnLifecycle::Reserved => None,
        })
    }

    /// 向活动 turn 注入立即引导输入；无活动 turn 或注入窗口已关闭时为 false。
    pub fn steer(&self, text: impl Into<String>) -> bool {
        self.active_controls()
            .is_some_and(|controls| controls.steer(text))
    }

    /// 接受一条 followUp：先经活动 turn 的唯一会话写者 durable 落盘 pending
    /// 接受记录（携带控制 identity、payload 与 FIFO sequence），成功后才加入
    /// Thread 的后续输入队列，在当前 turn 到达可信终态后按 FIFO 启动为一个
    /// 新的 turn。活动 turn 不存在（含预订阶段）或 durable 接受失败时拒绝
    /// （false），调用方应改以普通 turn 提交。
    pub fn submit_follow_up(&self, text: impl Into<String>) -> bool {
        let text = text.into();
        if text.trim().is_empty() {
            return false;
        }
        let Some(controls) = self.active_controls() else {
            return false;
        };
        let sequence = self.control_sequence.fetch_add(1, Ordering::Relaxed);
        let request = ControlRequest {
            control_id: control_id(&controls.turn_id, ControlChannel::FollowUp, sequence),
            turn_id: controls.turn_id.clone(),
            channel: ControlChannel::FollowUp,
            sequence,
            text: Some(text),
        };
        if !controls.append_pending(&request) {
            return false;
        }
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(_) => {
                // 状态未知（fail-closed）：durable 收敛为 cancelled，不留下
                // 无归宿的 pending 记录。
                let _ = controls.append_disposition(&request, ControlDisposition::Cancelled);
                return false;
            }
        };
        insert_by_sequence(&mut state.pending_follow_ups, ChainInput::accepted(request));
        true
    }

    /// 当前排队的 followUp 数量（仅用于展示计数）。
    pub fn pending_follow_up_count(&self) -> usize {
        self.state
            .lock()
            .map(|state| state.pending_follow_ups.len())
            .unwrap_or(0)
    }

    /// 撤回最近加入队列、尚未开始执行的一条 followUp。撤回是用户显式取消：
    /// durable 收敛为 cancelled（活动 turn 内经共享写者，否则短开 Append 写者），
    /// 收敛失败时放回队列并返回 None，绝不静默丢输入。
    pub fn withdraw_follow_up(&self) -> Option<String> {
        let (thread, popped) = {
            let mut state = self.state.lock().ok()?;
            let thread = state.thread.clone();
            let popped = state.pending_follow_ups.pop_back()?;
            (thread, popped)
        };
        let text = popped.text.clone();
        if let Some(request) = &popped.control {
            let appended = self
                .active_controls()
                .map(|controls| controls.append_disposition(request, ControlDisposition::Cancelled))
                .unwrap_or_else(|| {
                    self.runner.append_control_disposition(
                        &thread,
                        request,
                        ControlDisposition::Cancelled,
                    )
                });
            if appended.is_err() {
                if let Ok(mut state) = self.state.lock() {
                    insert_by_sequence(&mut state.pending_follow_ups, popped);
                }
                return None;
            }
        }
        Some(text)
    }

    /// 空闲时执行一次用户请求的上下文压缩；`cancellation` 允许调用方
    /// 随时中止压缩（TUI 中 Esc 取消）。
    pub fn compact(
        &self,
        cancellation: &CancellationToken,
    ) -> Result<singularity_agent::compaction::CompactionOutcome, ConversationError> {
        let reservation = self.reserve_start()?;
        let thread = reservation.conversation.thread()?;
        let result = self.runner.compact_thread(&thread, cancellation);
        drop(reservation);
        result.map_err(ConversationError::Configuration)
    }

    /// 中断当前活动 turn；无活动 turn 时为 no-op（返回 false）。接受时先
    /// durable 落盘 pending 接受记录（成功才触发取消令牌，影响执行），记入
    /// 本 turn 的取消日志，runner 在终态记录前落 `control_accepted`
    /// （disposition `cancelled`）。已接受的 followUp 保留在待处理队列中，
    /// 不在中断当轮自动执行，由下一次 `run_turn` 按 FIFO 继续消费。
    pub fn interrupt(&self) -> bool {
        self.active_controls()
            .is_some_and(|controls| controls.accept_cancel())
    }

    /// 修改当前 Thread 的 provider/model/reasoning：变更即生效。
    ///
    /// 提交点只做校验与内存投影更新，不写会话文件：turn 执行期间写者锁被
    /// 本轮占用，提交点写文件会使「运行中改设置」报错。持久化由下一 turn
    /// 开始时执行体在自己的会话写者上记录（去重后追加 `thread_settings`
    /// metadata），因此运行中与空闲时同路径，提交点不会因落盘失败。
    pub fn update_settings(
        &self,
        patch: SettingsPatch,
    ) -> Result<SettingsApplyResult, ConversationError> {
        let mut state = self.lock_state()?;
        if patch.is_empty() {
            let selector = compose_merged_selector(state.thread.model.as_deref(), &patch);
            return Ok(SettingsApplyResult {
                timing: SettingsApplyTiming::NothingToApply,
                selector,
            });
        }
        let selector = compose_validated_selector(&state.thread.model, &patch, &self.runner)
            .map_err(ConversationError::Configuration)?;
        state.thread.model = Some(selector.clone());
        Ok(SettingsApplyResult {
            timing: SettingsApplyTiming::AppliedNow,
            selector,
        })
    }

    /// 执行一轮 turn 直到终态；随后自动消费已接受的后续输入。
    ///
    /// 同一时刻只允许一个活动 turn；执行期间通过共享的 [`TurnControls`]
    /// （TUI 从其他线程）进行 steer 与取消。整个调用内完成：
    ///
    /// 1. 本轮显式输入的 turn（若此前有残留的已接受 followUp，则按 FIFO 先行）；
    /// 2. turn 到达可信终态（completed/failed/interrupted）后更新 Thread 投影；
    ///    设置变更由每个 turn 开始时在会话中记录（见 [`TurnRunner::run`]）；
    /// 3. 按 FIFO 启动已接受的 followUp 为新的 turn（各自独立 turn id），
    ///    直到队列清空；执行期间新提交的 followUp 同样被消费。
    ///
    /// 失败语义：任何已落盘的可信终态都返回 `Ok`（失败终态携带
    /// [`crate::events::TurnErrorDetail`]，不阻断队列中其余 followUp）；
    /// 终态化失败（无可信终态）或准备阶段失败返回 `Err` 并中止链条，
    /// 未执行的 followUp 原样保留。返回值为最后一个到达终态的 turn 结果。
    pub fn run_turn(
        &self,
        input: &str,
        sink: &mut dyn TurnEventSink,
    ) -> Result<TurnOutcome, ConversationError> {
        let reservation = self.reserve_start()?;
        reservation.run(input, sink)
    }

    /// 以已预订的链窗口执行链条；窗口由预订守卫持有，实施中的轮次各自
    /// 独立控制面（取消与转向只作用于当前轮）。
    fn run_chain(
        &self,
        input: &str,
        sink: &mut dyn TurnEventSink,
    ) -> Result<TurnOutcome, ConversationError> {
        // 残留的已接受输入先于本轮显式输入（FIFO）；正常路径队列为空。
        let mut queue: VecDeque<ChainInput> = {
            let mut state = self.lock_state()?;
            std::mem::take(&mut state.pending_follow_ups)
        };
        queue.push_back(ChainInput::explicit(input.to_string()));

        let mut last: Option<Result<TurnOutcome, ConversationError>> = None;
        loop {
            let current = match queue.pop_front() {
                Some(current) => current,
                None => match self.take_one_pending_follow_up_or_close()? {
                    Some(input) => input,
                    None => break,
                },
            };
            let (step, turn_undelivered) = self.run_single_turn(current.clone(), sink);
            // 无可信终态的轮次（终态化失败、准备失败、并发占用、状态中毒）
            // 中止链条：剩余输入原样保留，返回可行动错误。
            let untrusted = matches!(
                step,
                Err(ConversationError::Turn(TurnRunError::Terminalization(_)))
                    | Err(ConversationError::Turn(TurnRunError::Preparation { .. }))
                    | Err(ConversationError::TurnAlreadyActive)
                    | Err(ConversationError::State(_))
            );
            if untrusted {
                // 本轮已接受的转向输入与剩余输入一样保留在队列中（携带
                // durable 控制 identity），不存在无归宿的丢弃路径。
                for request in turn_undelivered.iter().rev() {
                    queue.push_front(ChainInput::accepted(request.clone()));
                }
                // 准备失败时本轮输入必然未执行过：放回队首继续可用；
                // 终态化失败时本轮可能已部分执行，不重放。
                if !matches!(
                    step,
                    Err(ConversationError::Turn(TurnRunError::Terminalization(_)))
                ) {
                    queue.push_front(current);
                }
                self.requeue_follow_ups(queue)?;
                return step;
            }
            // 中断终态停止链条：剩余 followUp 原样保留，未交付转向输入
            // 已 durable 收敛为 cancelled 并随本轮 outcome 退还调用方。
            if matches!(
                &step,
                Ok(outcome) if outcome.turn_status == TurnStatus::Interrupted
            ) {
                self.requeue_follow_ups(queue)?;
                return step;
            }
            // completed/failed 可信终态（Ok 携带各自状态与错误细节）后，
            // 未交付转向输入（携带 durable 控制 identity）排到链队列队首，
            // 先于已排队的 followUp 执行（排水优先）；随后继续消费队列。
            // 单轮失败终态不阻断其余已接受的 followUp。
            for request in turn_undelivered.iter().rev() {
                queue.push_front(ChainInput::accepted(request.clone()));
            }
            last = Some(step);
        }
        // 不变量：run_single_turn 至少消费一轮队列，last 必为 Some。
        #[allow(clippy::expect_used)]
        last.expect("run_turn executes at least one turn")
    }

    /// 单个 turn 的执行与投影收敛；每轮使用独立控制面，取消与注入只影响
    /// 当前轮，后续队列中的轮次不受本轮取消影响。输入携带控制请求时由
    /// runner 在 operation 起始后落终态 disposition（`started_as_new_turn`）。
    /// 第二元素是终态后注入箱的排水结果（携带 durable 控制 identity）：
    /// Ok 时已并入 `TurnOutcome::undelivered_inputs`（中断时同时 durable
    /// 收敛为 cancelled），Err 时交由链条保留归宿。
    fn run_single_turn(
        &self,
        current: ChainInput,
        sink: &mut dyn TurnEventSink,
    ) -> (Result<TurnOutcome, ConversationError>, Vec<ControlRequest>) {
        let (thread_snapshot, writer) = {
            let state = match self.lock_state() {
                Ok(state) => state,
                Err(error) => return (Err(error), Vec::new()),
            };
            if !matches!(state.turn, TurnLifecycle::Reserved) {
                return (Err(ConversationError::TurnAlreadyActive), Vec::new());
            }
            let thread = state.thread.clone();
            // 打开本轮唯一会话写者（含崩溃修复）；任何失败按准备失败收敛，
            // 不留下 operation 痕迹。
            let writer = match self.runner.open_turn_writer(&thread) {
                Ok(writer) => writer,
                Err(TurnRunError::Preparation { cause, message }) => {
                    return (
                        Err(ConversationError::Turn(TurnRunError::Preparation {
                            cause,
                            message,
                        })),
                        Vec::new(),
                    );
                }
                Err(error) => {
                    return (Err(ConversationError::Turn(error)), Vec::new());
                }
            };
            (thread, writer)
        };
        // 构造即完整：turn id、注入箱与共享写者在此一次性绑定。
        let controls = Arc::new(TurnControls::new(
            Uuid::new_v4().to_string(),
            TurnInbox::default_handle(),
            Arc::clone(&self.control_sequence),
            Arc::clone(&writer),
        ));
        // durable pending 控制重建：崩溃/重启前已接受但未交付的 steer 与
        // followUp 按 FIFO sequence 重新入队（跨进程 resume 语义）；pending
        // cancel 已由修复路径收敛为 cancelled。接受计数器抬升到 durable
        // 水位之上，新接受不与旧事实 sequence 冲突。
        let pending = match reduce_controls(lock_writer(&writer).entries()) {
            Ok(facts) => facts,
            Err(error) => {
                return (
                    Err(ConversationError::Turn(TurnRunError::Preparation {
                        cause: TurnFailureCause::Store,
                        message: error.to_string(),
                    })),
                    Vec::new(),
                );
            }
        };
        if let Some(max_sequence) = pending.iter().map(|control| control.sequence).max() {
            self.control_sequence
                .fetch_max(max_sequence + 1, Ordering::Relaxed);
        }
        {
            let mut state = match self.lock_state() {
                Ok(state) => state,
                Err(error) => return (Err(error), Vec::new()),
            };
            state.turn = TurnLifecycle::Running(Arc::clone(&controls));
            // 本轮正要执行的控制已在链条中（runner 会为其落终态 disposition），
            // 不得再按 pending 事实重排进队列——否则该控制在终态后会被再次
            // 弹出执行，产生重复终态记录。
            let executing = current
                .control
                .as_ref()
                .map(|request| request.control_id.as_str());
            for control in pending {
                if control.disposition == ControlDisposition::Pending
                    && matches!(
                        control.channel,
                        ControlChannel::Steer | ControlChannel::FollowUp
                    )
                    && executing != Some(control.control_id.as_str())
                {
                    let request = ControlRequest {
                        control_id: control.control_id,
                        turn_id: control.turn_id,
                        channel: control.channel,
                        sequence: control.sequence,
                        text: control.text,
                    };
                    if !state
                        .pending_follow_ups
                        .iter()
                        .any(|input| input.control_id() == Some(request.control_id.as_str()))
                    {
                        insert_by_sequence(
                            &mut state.pending_follow_ups,
                            ChainInput::accepted(request),
                        );
                    }
                }
            }
        }
        let params = TurnParams {
            thread: thread_snapshot,
            input: current.text,
            model_override: self.model_override.clone(),
            control: current.control,
        };
        let result = self.runner.run(params, &controls, sink);
        // 终态后排水：注入箱中仍未交付的转向输入随结果返回，由链条决定
        // 重排到下一轮或退还调用方。
        let undelivered = controls.take_drained_inbox();
        let mut state = match self.lock_state() {
            Ok(state) => state,
            Err(error) => return (Err(error), undelivered),
        };
        state.turn = TurnLifecycle::Reserved;
        match &result {
            Ok(outcome) => {
                state.thread.last_turn_status = Some(outcome.turn_status);
            }
            // Terminalization 表示没有可信终态；保持上一投影不变。
            Err(TurnRunError::Terminalization(_)) => {}
            Err(_) => state.thread.last_turn_status = Some(TurnStatus::Failed),
        }
        // Ok 时排水结果并入 outcome 单一字段（文本）；Err 时无 outcome 可
        // 承载，排水结果（携带 identity）随元组第二元素返回，交由链条保留归宿。
        match result {
            Ok(mut outcome) => {
                outcome.undelivered_inputs = undelivered
                    .iter()
                    .filter_map(|request| request.text.clone())
                    .collect();
                (Ok(outcome), undelivered)
            }
            Err(error) => (Err(ConversationError::Turn(error)), undelivered),
        }
    }

    fn active_controls(&self) -> Option<Arc<TurnControls>> {
        // 锁中毒（状态未知）→ 按无活动 turn 收敛（None），与 has_active_turn
        // 的「中毒按 busy」同向：读路径不泄露可能损坏的控制面。
        self.state
            .lock()
            .ok()
            .and_then(|state| state.turn.controls())
    }

    /// 从状态锁内原子地取下一条 followUp（携带其接受序号）；队列为空时在同
    /// 一临界区关闭链窗口（转入 Idle），此后 submit_follow_up 自然拒绝。
    /// 窗口关闭与取队列是同一把锁内的原子操作，不存在"取空后仍接受新输入"
    /// 的窗口。
    fn take_one_pending_follow_up_or_close(&self) -> Result<Option<ChainInput>, ConversationError> {
        let mut state = self.lock_state()?;
        match state.pending_follow_ups.pop_front() {
            Some(input) => Ok(Some(input)),
            None => {
                state.turn = TurnLifecycle::Idle;
                Ok(None)
            }
        }
    }

    /// 把未执行的 followUp 输入放回队列（与队列中已有输入合并，输入在前）。
    /// 锁中毒时向上传播错误，不静默丢弃输入：保证「每条 followUp 恰好执行
    /// 一次」不变量可观察（调用方在错误路径上合并该失败）。
    fn requeue_follow_ups(&self, inputs: VecDeque<ChainInput>) -> Result<(), ConversationError> {
        if inputs.is_empty() {
            return Ok(());
        }
        let mut state = self.lock_state()?;
        let mut merged = inputs;
        merged.extend(state.pending_follow_ups.drain(..));
        state.pending_follow_ups = merged;
        Ok(())
    }

    fn lock_state(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, ConversationState>, ConversationError> {
        self.state
            .lock()
            .map_err(|_| ConversationError::State("conversation state poisoned".to_string()))
    }

    /// 毒化 `self.state` 供测试验证 fail-closed 行为。
    #[cfg(test)]
    pub(crate) fn poison_state_lock(&self) {
        // 测试专用：若锁已中毒则直接 panic，不掩盖测试意图。
        #[allow(clippy::expect_used)]
        let _guard = self.state.lock().expect("lock poisoned");
        panic!("intentional poison for test");
    }
}

/// 把 patch 合并到当前 selector 上（`provider/model[#effort]`），返回完整
/// 选择器；不做合法性校验。提交点校验与内存投影更新共用同一组合语义。
fn compose_merged_selector(current: Option<&str>, patch: &SettingsPatch) -> String {
    let parts = split_model_selector(current.unwrap_or(""));
    let provider = patch
        .provider
        .clone()
        .or_else(|| parts.provider.map(str::to_string))
        .unwrap_or_else(|| singularity_model::DEFAULT_PROVIDER_NAME.to_string());
    let model = patch
        .model
        .clone()
        .or_else(|| parts.model.map(str::to_string));
    let reasoning = match &patch.reasoning {
        ReasoningPatch::Keep => parts.effort.map(str::to_string),
        ReasoningPatch::Set(value) => Some(value.clone()),
        ReasoningPatch::Clear => None,
    };
    singularity_model::compose_model_selector(
        &provider,
        model.as_deref().unwrap_or(""),
        reasoning.as_deref(),
    )
}

/// 无锁的 selector 组合与校验：以当前 Thread 投影为基线合并 patch，
/// 并确认快照能解析结果。锁内路径与提交点校验共用同一实现。
fn compose_validated_selector(
    current: &Option<String>,
    patch: &SettingsPatch,
    runner: &TurnRunner,
) -> Result<String, String> {
    let selector = compose_merged_selector(current.as_deref(), patch);
    let parts = split_model_selector(&selector);
    let model = parts.model.unwrap_or_default();
    if model.trim().is_empty() {
        return Err("thread settings require a model".to_string());
    }
    let provider = parts
        .provider
        .unwrap_or(singularity_model::DEFAULT_PROVIDER_NAME);
    let reasoning = parts.effort;
    if provider.trim().is_empty()
        || provider.chars().any(char::is_whitespace)
        || model.chars().any(char::is_whitespace)
        || reasoning
            .is_some_and(|value| value.trim().is_empty() || value.chars().any(char::is_whitespace))
    {
        return Err("invalid provider/model/reasoning value".to_string());
    }
    runner.validate_model_selector(Some(&selector))?;
    Ok(selector)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(turn: TurnLifecycle, reservation_seq: u64) -> ConversationState {
        ConversationState {
            thread: Thread {
                thread_id: "t-1".to_string(),
                model: None,
                cwd: String::new(),
                last_turn_status: None,
            },
            turn,
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
}
