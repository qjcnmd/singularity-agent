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

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use singularity_agent::agent::TurnInboxHandle;
use singularity_agent::session::SessionManager;
use singularity_core::CancellationToken;
use singularity_model::split_model_selector;

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
/// inbox 由 runner 在 Agent 构造完成后注册（先于 turn/started 事件发布），
/// 保证 started 后立即注入必成功；终态化前由 runner 关闭注入窗口。
#[derive(Default)]
pub struct TurnControls {
    pub(crate) turn_id: Mutex<String>,
    pub cancellation: CancellationToken,
    pub(crate) inbox: Mutex<Option<TurnInboxHandle>>,
}

impl TurnControls {
    pub fn new(turn_id: impl Into<String>) -> Self {
        Self {
            turn_id: Mutex::new(turn_id.into()),
            cancellation: CancellationToken::new(),
            inbox: Mutex::new(None),
        }
    }

    /// 把转向输入注入当前 turn；turn 已关闭注入窗口时返回 false。
    pub fn steer(&self, text: impl Into<String>) -> bool {
        let Ok(guard) = self.inbox.lock() else {
            return false;
        };
        guard.as_ref().is_some_and(|inbox| {
            inbox
                .lock()
                .is_ok_and(|mut inbox| inbox.enqueue(text.into()))
        })
    }

    pub(crate) fn register_inbox(&self, turn_id: &str, inbox: TurnInboxHandle) {
        if let Ok(mut id) = self.turn_id.lock() {
            *id = turn_id.to_string();
        }
        if let Ok(mut guard) = self.inbox.lock() {
            *guard = Some(inbox);
        }
    }

    pub(crate) fn close_inbox(&self) {
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
    }

    pub fn runner(&self) -> &TurnRunner {
        &self.runner
    }

    pub fn runner_handle(&self) -> Arc<TurnRunner> {
        Arc::clone(&self.runner)
    }

    /// 读取一个已完成 turn 持久化的思考块。
    pub fn thinking_for_turn(&self, turn_id: &str) -> Result<Vec<String>, ConversationError> {
        let thread = self.thread()?;
        self.runner
            .thinking_for_turn(&thread, turn_id)
            .map_err(ConversationError::Configuration)
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
        self.state.lock().ok().and_then(|state| match &state.turn {
            TurnLifecycle::Running(controls) => controls.turn_id.lock().ok().map(|id| id.clone()),
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

    pub fn rename(&self, name: &str) -> Result<(), ConversationError> {
        let state = self.lock_state()?;
        if state.turn.is_busy() {
            return Err(ConversationError::TurnAlreadyActive);
        }
        crate::store::rename_thread(
            self.runner.sessions_dir(),
            &state.thread.thread_id,
            name,
            self.runner.coordinator(),
        )
        .map_err(ConversationError::Configuration)
    }

    /// 列出可恢复的 Thread 摘要（会话列表数据源）。
    pub fn list_threads(&self) -> Result<Vec<crate::store::ThreadSummary>, String> {
        crate::store::list_threads(self.runner.sessions_dir())
    }

    /// 重开既有 Thread 并执行崩溃修复；返回投影后的 Thread。失败时状态不变。
    pub fn resume_thread(&self, thread_id: &str) -> Result<Thread, crate::store::ResumeError> {
        crate::store::resume_thread(
            self.runner.sessions_dir(),
            thread_id,
            self.runner.coordinator(),
        )
    }

    /// 创建新 Thread（uuid v7 会话文件，属主权限）。
    pub fn create_thread(&self, cwd: &str, model: Option<String>) -> Result<Thread, String> {
        crate::store::create_thread(
            self.runner.sessions_dir(),
            cwd,
            model,
            self.runner.coordinator(),
        )
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
                self.requeue_follow_ups(queue);
                return step;
            }
            // 成功应用后发布投影更新：客户端据此拿到下一 turn 生效的
            // 线程模型。持久化失败时
            // 保留该意图与剩余输入，返回可行动错误。
            let applied = match self.apply_pending_settings() {
                Ok(applied) => applied,
                Err(error) => {
                    self.requeue_follow_ups(queue);
                    return Err(error);
                }
            };
            if let Some(updated) = applied {
                sink.emit(TurnEvent::ThreadSettingsApplied { thread: updated });
            }
            // 可信终态（completed/failed/interrupted）后继续消费队列；
            // 单轮执行失败不阻断其余已接受的 followUp。
            last = Some(step);
        }
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
            let controls = Arc::new(TurnControls::new(String::new()));
            state.turn = TurnLifecycle::Running(Arc::clone(&controls));
            controls
        };
        let thread_snapshot = self.lock_state()?.thread.clone();
        let params = TurnParams {
            thread: thread_snapshot,
            input,
        };
        let result = self.runner.run(params, &controls, sink);
        let mut state = self.lock_state()?;
        state.turn = TurnLifecycle::Reserved;
        match &result {
            Ok(outcome) => {
                state.thread.last_turn_status = Some(match outcome.turn_status {
                    TurnStatus::Running => ThreadStatus::Active,
                    TurnStatus::Completed => ThreadStatus::Completed,
                    TurnStatus::Failed => ThreadStatus::Failed,
                    TurnStatus::Interrupted => ThreadStatus::Interrupted,
                });
            }
            // Terminalization 表示没有可信终态；保持上一投影不变。
            Err(TurnRunError::Terminalization(_)) => {}
            Err(_) => state.thread.last_turn_status = Some(ThreadStatus::Failed),
        }
        result.map_err(ConversationError::Turn)
    }

    /// 终态后应用待生效设置并返回更新后的线程投影；无待生效意图时返回
    /// `None`（不产生事件）。持久化失败时意图保留在队列中等待重试。
    ///
    /// 时序前提：调用时点上个 turn 的 SessionManager 已关闭（`run_single_turn`
    /// 中 `runner.run` 返回后其局部 session 已析构），本打开的 OS 写者锁不会
    /// 与同一会话的已释放写者冲突。
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
            let mut session =
                SessionManager::open_existing_with_coordinator(&path, self.runner.coordinator())
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

    fn requeue_follow_ups(&self, inputs: VecDeque<String>) {
        if inputs.is_empty() {
            return;
        }
        if let Ok(mut state) = self.state.lock() {
            let mut merged = inputs;
            merged.extend(state.pending_follow_ups.drain(..));
            state.pending_follow_ups = merged;
        }
    }

    fn lock_state(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, ConversationState>, ConversationError> {
        self.state
            .lock()
            .map_err(|_| ConversationError::State("conversation state poisoned".to_string()))
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
