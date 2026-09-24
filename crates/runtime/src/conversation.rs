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

mod execution;
mod state;

pub(crate) use self::state::{CancelWindow, TurnControls};
use self::state::{ConversationState, TurnLifecycle, insert_by_sequence, locate_pending_input};

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use singularity_agent::agent::ControlRequest;
use singularity_agent::session::lock_writer;
use singularity_protocol::{ControlChannel, ControlDisposition, ControlSnapshot, SessionPhase};

use crate::error::TurnRunError;
use crate::runner::{TurnOutcome, TurnRunResult, TurnRunner};
use singularity_protocol::Thread;
use singularity_protocol::TurnEvent;

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
    pub async fn run(
        &mut self,
        input: &str,
        sink: &mut (dyn FnMut(TurnEvent) + Send),
    ) -> Result<TurnOutcome, ConversationError> {
        // 提升出来的输入有独立的执行入口；这里用错了就直接失败，不悄悄把它丢掉。
        if self.promoted_input.is_some() {
            return Err(ConversationError::Configuration(
                "turn reservation carries a promoted follow-up; run_promoted executes it"
                    .to_string(),
            ));
        }
        let request = self.conversation.accept_submission(input.to_string())?;
        self.conversation.run_chain(request, false, sink).await
    }

    /// 执行从 pending follow-up 里原子提升出来的输入。它排在队列中其他 follow-up
    /// 之前，并沿用原来的控制身份和接受序号。
    pub async fn run_promoted(
        &mut self,
        sink: &mut (dyn FnMut(TurnEvent) + Send),
    ) -> Result<TurnOutcome, ConversationError> {
        let input = self.promoted_input.take().ok_or_else(|| {
            ConversationError::Configuration(
                "turn reservation does not carry a promoted follow-up".to_string(),
            )
        })?;
        self.conversation.run_chain(input, true, sink).await
    }

    /// 在已经预订好的压缩窗口里执行；预订一直持有到调用方完成投影收尾。
    pub async fn compact(&mut self) -> Result<crate::CompactionOutcome, ConversationError> {
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
            .await
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
