//! Thread 的长驻协调器：单活动 turn、转向注入、后续输入队列、取消与设置生效时序。
//!
//! [`Conversation`] 是无交互入口与 TUI 共用的生命周期状态机。它不实现任何
//! 执行细节：turn 体完全委托给 [`crate::TurnRunner`]，这里维护四类长驻事实：
//!
//! - 「同一 Thread 至多一个活动 turn」的不变量；
//! - steer 注入窗口（活动 turn 的 Agent 收件箱）与取消令牌；
//! - followUp 后续输入队列：活动 turn 期间接受的每条 followUp 在当前 turn
//!   到达可信终态后按提交顺序自动启动为一个新的 turn，每条恰好执行一次；
//! - 设置生效时序：活动 turn 期间的变更只记录一份待生效意图，turn 终态收敛
//!   后由本对象自动校验并持久化，调用方无需手动提取或应用。
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
//! - `register_inbox`、`close_inbox`：中毒时跳过操作，`steer` 全程 false
//!   （fail-closed）；`close_inbox` 是 Agent 收口已关闭后的二次保险，
//!   跳过不影响正确性。
//! - `pending_follow_ups`、`withdraw_follow_up`：读路径按空返回，不展示
//!   可能已损坏的数据。

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use singularity_agent::agent::{TurnInbox, TurnInboxHandle};
use singularity_agent::session::{SessionAccess, SessionManager};
use singularity_core::CancellationToken;
use singularity_model::split_model_selector;
use uuid::Uuid;

use crate::error::TurnRunError;
use crate::events::{TurnEvent, TurnEventSink};
use crate::objects::{Thread, ThreadStatus, TurnStatus};
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

/// [`Conversation::queue_settings`] 的结果：本次修改的生效时点。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsApplyTiming {
    /// 没有可应用的内容（空 patch）。
    NothingToApply,
    /// 已立即校验并持久化，内存投影同步更新。
    AppliedNow,
    /// 活动 turn 期间只记录了意图；turn 到达可信终态后自动持久化并在
    /// 下一 turn 生效（持久化结果以 `thread/settingsApplied` 事件发布）。
    QueuedForNextTurn,
}

impl SettingsPatch {
    pub fn is_empty(&self) -> bool {
        self.provider.is_none()
            && self.model.is_none()
            && matches!(self.reasoning, ReasoningPatch::Keep)
    }

    /// 字段级合并：`patch` 中给出的字段覆盖 `self`，未给出的保持不变。
    fn merged_with(self, patch: &SettingsPatch) -> Self {
        Self {
            provider: patch.provider.clone().or(self.provider),
            model: patch.model.clone().or(self.model),
            reasoning: match patch.reasoning {
                ReasoningPatch::Keep => self.reasoning,
                _ => patch.reasoning.clone(),
            },
        }
    }
}

/// [`Conversation::queue_settings`] 的结果：本次修改的生效时点与合并后的 selector。
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
/// 构造即完整：turn id 与注入箱句柄在构造时一次性绑定，注入窗口在
/// turn 开始前即已就绪；终态化前由 runner 关闭注入窗口。
pub struct TurnControls {
    pub(crate) turn_id: String,
    pub cancellation: CancellationToken,
    pub(crate) inbox: Mutex<Option<TurnInboxHandle>>,
}

impl TurnControls {
    pub fn new(turn_id: impl Into<String>, inbox: TurnInboxHandle) -> Self {
        Self {
            turn_id: turn_id.into(),
            cancellation: CancellationToken::new(),
            inbox: Mutex::new(Some(inbox)),
        }
    }

    /// 本轮注入箱句柄：供执行体构造时接收同一句柄。
    pub(crate) fn inbox_handle(&self) -> TurnInboxHandle {
        let Ok(guard) = self.inbox.lock() else {
            // 锁中毒（状态未知）→ 返回空箱句柄，注入将被 fail-closed 拒绝。
            return TurnInbox::default_handle();
        };
        guard
            .as_ref()
            .cloned()
            .unwrap_or_else(TurnInbox::default_handle)
    }

    /// 把转向输入注入当前 turn；turn 已关闭注入窗口时返回 false。
    pub fn steer(&self, text: impl Into<String>) -> bool {
        let Ok(guard) = self.inbox.lock() else {
            // 锁中毒（状态未知）→ 拒绝注入，fail-closed：不把输入写进
            // 可能已损坏的收件箱，调用方按注入失败处理。
            return false;
        };
        guard.as_ref().is_some_and(|inbox| {
            inbox
                .lock()
                .is_ok_and(|mut inbox| inbox.enqueue(text.into()))
        })
    }

    /// 取走本轮尚未交付的转向输入（终态后由链条排水到下一轮）。
    pub(crate) fn drain_inbox(&self) -> Vec<String> {
        let Ok(guard) = self.inbox.lock() else {
            // 锁中毒（状态未知）→ 按无残留输入收敛，不泄露可能损坏的状态。
            return Vec::new();
        };
        guard.as_ref().map_or_else(Vec::new, |inbox| {
            inbox
                .lock()
                .map_or_else(|_| Vec::new(), |mut inbox| inbox.drain())
        })
    }

    pub(crate) fn close_inbox(&self) {
        // 锁中毒（状态未知）→ 跳过关闭；此方法是 Agent 收口关闭之后的
        // 二次保险（runner 的 abort/fail 路径已关 inbox），跳过不影响正确性。
        if let Ok(mut guard) = self.inbox.lock() {
            *guard = None;
        }
    }
}

struct ConversationState {
    thread: Thread,
    turn: TurnLifecycle,
    /// 活动 turn 期间接受、待终态后生效的设置意图；至多一份。
    queued_settings: Option<SettingsPatch>,
    /// 已接受的后续 turn 输入，按提交顺序 FIFO 执行。
    pending_follow_ups: VecDeque<String>,
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
    state: Mutex<ConversationState>,
}

/// 单活动 turn 的执行权预订。
///
/// [`Conversation::reserve_start`] 原子开启链窗口并持有到消费执行；预订由
/// [`Self::run`] 消费执行整条链条，或在未执行时由 drop 释放。窗口释放
/// 完全依赖 Drop（重复释放幂等），执行中途 panic 也不会泄漏活动窗口。
pub struct TurnReservation {
    conversation: Arc<Conversation>,
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
        self.conversation.release_reservation();
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
        Arc::new_cyclic(|weak| Self {
            runner,
            self_weak: weak.clone(),
            state: Mutex::new(ConversationState {
                thread,
                turn: TurnLifecycle::Idle,
                queued_settings: None,
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
        state.turn = TurnLifecycle::Reserved;
        // 不变量：Conversation 由 Arc 持有并注册 self_weak 后才可 reserve_start，upgrade 必成功。
        #[allow(clippy::expect_used)]
        let conversation = self
            .self_weak
            .upgrade()
            .expect("reservation requires a live conversation");
        Ok(TurnReservation { conversation })
    }

    fn release_reservation(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.turn = TurnLifecycle::Idle;
        }
        // 锁中毒（状态未知）时保持窗口不释放：链窗口永久 busy = fail-closed，
        // 宁可让后续预订与提交被拒，也不在状态损坏时静默放行写入。
    }

    pub fn runner(&self) -> &TurnRunner {
        &self.runner
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

    /// 接受一条 followUp：加入 Thread 的后续输入队列，在当前 turn 到达可信
    /// 终态后按提交顺序启动为一个新的 turn。链窗口未开启时拒绝（false），
    /// 调用方应改以普通 turn 提交。
    pub fn submit_follow_up(&self, text: impl Into<String>) -> bool {
        let text = text.into();
        if text.trim().is_empty() {
            return false;
        }
        let Ok(mut state) = self.state.lock() else {
            // 锁中毒（状态未知）→ 拒绝接受，fail-closed：不把输入写进
            // 可能已损坏的队列，调用方按未接受处理。
            return false;
        };
        if !state.turn.is_busy() {
            return false;
        }
        state.pending_follow_ups.push_back(text);
        true
    }

    /// 当前排队的 followUp 快照（仅用于展示计数与诊断）。
    pub fn pending_follow_ups(&self) -> Vec<String> {
        self.state
            .lock()
            .map(|state| state.pending_follow_ups.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// 撤回最近加入队列、尚未开始执行的一条 followUp。
    pub fn withdraw_follow_up(&self) -> Option<String> {
        self.state
            .lock()
            .ok()
            .and_then(|mut state| state.pending_follow_ups.pop_back())
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

    /// 中断当前活动 turn；无活动 turn 时为 no-op。已接受的 followUp 不受
    /// 影响，仍按合同在该 turn 终态后继续执行。
    pub fn interrupt(&self) {
        if let Some(controls) = self.active_controls() {
            controls.cancellation.cancel();
        }
    }

    /// 修改当前 Thread 的 provider/model/reasoning。
    ///
    /// 空闲时立即校验并持久化为 `thread_settings` metadata（不改写全局配置）；
    /// 活动 turn 期间只记录一份待生效意图（新 patch 按字段合并覆盖），当前
    /// turn 继续使用启动时的 selector，turn 到达可信终态后由 [`Self::run_turn`]
    /// 自动持久化并以 `thread/settingsApplied` 事件发布，下一 turn 生效。
    /// 校验失败立即报错，不进入队列。返回本次修改的生效时点。
    pub fn queue_settings(
        &self,
        patch: SettingsPatch,
    ) -> Result<SettingsApplyResult, ConversationError> {
        // 单次拿锁事务：读当前 thread.model、合并已有待生效意图与新 patch、
        // 校验最终组合、写回 pending、返回同一 selector。消除先校验后合并的
        // 两次拿锁窗口，也避免合并结果只在终态持久化时才被发现非法。
        let mut state = self.lock_state()?;
        let merged = match state.queued_settings.as_ref() {
            Some(pending) => pending.clone().merged_with(&patch),
            None => patch,
        };
        if merged.is_empty() {
            let selector = compose_merged_selector(state.thread.model.as_deref(), &merged);
            return Ok(SettingsApplyResult {
                timing: SettingsApplyTiming::NothingToApply,
                selector,
            });
        }
        // 提交点校验最终组合（含已排队部分）：无效组合立即被拒绝，
        // 而不是等到终态持久化时才失败；校验失败时原 pending 保留。
        let selector = compose_validated_selector(&state.thread.model, &merged, &self.runner)
            .map_err(ConversationError::Configuration)?;
        state.queued_settings = Some(merged);
        if state.turn.is_busy() {
            return Ok(SettingsApplyResult {
                timing: SettingsApplyTiming::QueuedForNextTurn,
                selector,
            });
        }
        // 空闲路径与终态后路径共用同一份待生效意图：先入队再立即消费，
        // 持久化失败时意图保留在队列中等待重试。
        self.persist_pending_settings_locked(&mut state)?;
        Ok(SettingsApplyResult {
            timing: SettingsApplyTiming::AppliedNow,
            selector,
        })
    }

    /// 执行一轮 turn 直到终态；随后自动消费已接受的后续输入与设置。
    ///
    /// 同一时刻只允许一个活动 turn；执行期间通过共享的 [`TurnControls`]
    /// （TUI 从其他线程）进行 steer 与取消。整个调用内完成：
    ///
    /// 1. 本轮显式输入的 turn（若此前有残留的已接受 followUp，则按 FIFO 先行）；
    /// 2. turn 到达可信终态（completed/failed/interrupted）后，自动持久化
    ///    待生效设置并更新 Thread 投影——下一 turn 使用新 selector；
    /// 3. 按 FIFO 启动已接受的 followUp 为新的 turn（各自独立 turn id），
    ///    直到队列清空；执行期间新提交的 followUp 同样被消费。
    ///
    /// 失败语义：单轮执行失败（`Execution`）不阻断队列中其余 followUp；
    /// 终态化失败（无可信终态）、准备阶段失败或设置持久化失败会中止链条，
    /// 未执行的 followUp 与未生效的设置原样保留，并返回可行动错误。
    /// 返回值为最后一个到达终态的 turn 结果。
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
        let mut queue: VecDeque<String> = {
            let mut state = self.lock_state()?;
            std::mem::take(&mut state.pending_follow_ups)
        };
        queue.push_back(input.to_string());

        let mut last: Option<Result<TurnOutcome, ConversationError>> = None;
        loop {
            let current = match queue.pop_front() {
                Some(current) => current,
                None => match self.take_one_pending_follow_up_or_close()? {
                    Some(input) => input,
                    None => break,
                },
            };
            let step = self.run_single_turn(current.clone(), sink);
            // 无可信终态的轮次（终态化失败、准备失败、并发占用）中止链条：
            // 剩余输入与待生效设置原样保留，返回可行动错误。
            let untrusted = matches!(
                step,
                Err(ConversationError::Turn(TurnRunError::Terminalization(_)))
                    | Err(ConversationError::Turn(TurnRunError::Preparation { .. }))
                    | Err(ConversationError::TurnAlreadyActive)
            );
            if untrusted {
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
            // 随本轮 outcome 退还调用方（pi 的 interrupted 排水语义）。
            if matches!(
                &step,
                Ok(outcome) if outcome.turn_status == TurnStatus::Interrupted
            ) {
                self.requeue_follow_ups(queue)?;
                return step;
            }
            // 成功应用后发布投影更新：客户端据此拿到下一 turn 生效的
            // 线程模型。持久化失败时
            // 保留该意图与剩余输入，返回可行动错误。
            let applied = match self.apply_pending_settings() {
                Ok(applied) => applied,
                Err(error) => {
                    // requeue 失败（锁中毒）比 settings 持久化更根本，优先传播。
                    self.requeue_follow_ups(queue)?;
                    return Err(error);
                }
            };
            if let Some(updated) = applied {
                sink.emit(TurnEvent::ThreadSettingsApplied { thread: updated });
            }
            // completed/failed 终态后，未交付转向输入排到链队列队首，先于
            // 已排队的 followUp 执行（pi continue 的排水顺序）；随后继续
            // 消费队列。单轮执行失败不阻断其余已接受的 followUp。
            if let Ok(outcome) = &step {
                for text in outcome.undelivered_inputs.iter().rev() {
                    queue.push_front(text.clone());
                }
            }
            last = Some(step);
        }
        // 不变量：run_single_turn 至少消费一轮队列，last 必为 Some。
        #[allow(clippy::expect_used)]
        last.expect("run_turn executes at least one turn")
    }

    /// 单个 turn 的执行与投影收敛；每轮使用独立控制面，取消与注入只影响
    /// 当前轮，后续队列中的轮次不受本轮取消影响。
    fn run_single_turn(
        &self,
        input: String,
        sink: &mut dyn TurnEventSink,
    ) -> Result<TurnOutcome, ConversationError> {
        let controls = {
            let mut state = self.lock_state()?;
            if !matches!(state.turn, TurnLifecycle::Reserved) {
                return Err(ConversationError::TurnAlreadyActive);
            }
            // 构造即完整：turn id 与注入箱在此一次性绑定，无需事后注册。
            let controls = Arc::new(TurnControls::new(
                Uuid::new_v4().to_string(),
                TurnInbox::default_handle(),
            ));
            state.turn = TurnLifecycle::Running(Arc::clone(&controls));
            controls
        };
        let thread_snapshot = self.lock_state()?.thread.clone();
        let params = TurnParams {
            thread: thread_snapshot,
            input,
        };
        let result = self.runner.run(params, &controls, sink);
        // 终态后排水：注入箱中仍未交付的转向输入随 outcome 返回，由链条
        // 决定重排到下一轮或退还调用方。
        let undelivered = controls.drain_inbox();
        let mut state = self.lock_state()?;
        state.turn = TurnLifecycle::Reserved;
        match &result {
            Ok(outcome) => {
                state.thread.last_turn_status = Some(ThreadStatus::from(outcome.turn_status));
            }
            // Terminalization 表示没有可信终态；保持上一投影不变。
            Err(TurnRunError::Terminalization(_)) => {}
            Err(_) => state.thread.last_turn_status = Some(ThreadStatus::Failed),
        }
        match result {
            Ok(mut outcome) => {
                outcome.undelivered_inputs = undelivered;
                Ok(outcome)
            }
            Err(error) => Err(error),
        }
        .map_err(ConversationError::Turn)
    }

    /// 终态后应用待生效设置并返回更新后的线程投影；无待生效意图时返回
    /// `None`（不产生事件）。持久化失败时意图保留在队列中等待重试；若与
    /// 存活写者并发，写者锁显式拒绝（WriterConflict），同样进队列重试。
    fn apply_pending_settings(&self) -> Result<Option<Thread>, ConversationError> {
        let mut state = self.lock_state()?;
        if state.queued_settings.is_none() {
            return Ok(None);
        }
        self.persist_pending_settings_locked(&mut state)?;
        Ok(Some(state.thread.clone()))
    }

    /// 校验并把当前待生效意图持久化为 `thread_settings` metadata，
    /// 同步更新内存 Thread 投影。持久化失败时意图保留在队列中等待重试。
    fn persist_pending_settings_locked(
        &self,
        state: &mut ConversationState,
    ) -> Result<(), ConversationError> {
        let Some(pending) = state.queued_settings.clone() else {
            return Ok(());
        };
        if pending.is_empty() {
            state.queued_settings = None;
            return Ok(());
        }
        // 调用方已持有 state 锁：selector 组合必须走无锁纯函数。
        let selector = compose_validated_selector(&state.thread.model, &pending, &self.runner)
            .map_err(ConversationError::Configuration)?;
        let path =
            crate::store::thread_session_path(self.runner.sessions_dir(), &state.thread.thread_id);
        let write_result = (|| -> Result<(), String> {
            let mut session = SessionManager::open_existing_with_access(
                &path,
                self.runner.coordinator(),
                &state.thread.thread_id,
                SessionAccess::Append,
            )
            .map_err(|error| error.to_string())?;
            let parts = split_model_selector(&selector);
            let metadata = singularity_agent::session::SessionMetadata::thread_settings(
                parts
                    .provider
                    .unwrap_or(singularity_model::DEFAULT_PROVIDER_NAME),
                parts.model.unwrap_or_default(),
                parts.effort.map(str::to_string),
            );
            session
                .append_metadata(metadata)
                .map_err(|error| error.to_string())
                .map(|_| ())
        })();
        match write_result {
            Ok(()) => {
                state.queued_settings = None;
                state.thread.model = Some(selector);
                Ok(())
            }
            // JSONL 未写入：意图原样保留，避免静默丢失。
            Err(message) => Err(ConversationError::Configuration(format!(
                "failed to persist thread settings: {message}"
            ))),
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

    /// 从状态锁内原子地取下一条 followUp；队列为空时在同一临界区关闭链
    /// 窗口（转入 Idle），此后 submit_follow_up 自然拒绝。窗口关闭与
    /// 取队列是同一把锁内的原子操作，不存在"取空后仍接受新输入"的窗口。
    fn take_one_pending_follow_up_or_close(&self) -> Result<Option<String>, ConversationError> {
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
    fn requeue_follow_ups(&self, inputs: VecDeque<String>) -> Result<(), ConversationError> {
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
/// 选择器；不做合法性校验。提交点校验、终态自动生效与 app-server 回显
/// 共用同一组合语义。
pub fn compose_merged_selector(current: Option<&str>, patch: &SettingsPatch) -> String {
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
