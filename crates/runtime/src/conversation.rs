//! 一个 session 的内存队列，以及它唯一的活动执行窗口。
//! 已经被消费的输入由 Agent 落盘；还在等待处理的输入只活在进程里，进程结束就没了。
//!
//! # 锁
//!
//! 两个锁各管一件事，加锁顺序固定为「写者窗口 → 状态」，不会互相反向等待：
//!
//! - `writer_window`：会话写者的打开、Running→Reserved 的交接、设置与元数据写盘都在这里互斥；
//!   文件 I/O 不占着状态锁。
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

use singularity_agent::agent::{ControlRequest, UserInput};
use singularity_agent::session::{SessionMetadata, SessionWriter, lock_writer};
use singularity_protocol::SessionPhase;

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
    pub pending_controls: Vec<singularity_protocol::PendingInput>,
    pub pending_question: Option<singularity_protocol::PendingQuestion>,
}

/// 一个 Thread 的长驻协调器。
pub struct Conversation {
    runner: Arc<TurnRunner>,
    /// Thread 设置、活动阶段、控制接受顺序和待处理输入由同一把锁协调。
    state: Mutex<ConversationState>,
    /// 会话写入窗口：写者打开、turn 交接、设置与元数据写盘的互斥点，见模块文档「锁」。
    writer_window: Mutex<()>,
}

/// 一次执行预订；队列在预订期间保持原位，drop 时释放执行窗口。
pub struct TurnReservation {
    conversation: Arc<Conversation>,
    first_pending: usize,
}

impl TurnReservation {
    /// 执行本轮输入以及后续队列，直到链条结束；窗口一直保持到预订 drop。
    /// 调用方须在完成事件投影后释放预订，任务才重新接受其他执行。
    /// 控制处置的变化通过同一个事件出口带类型发布。本轮输入在这里取得控制身份，
    /// 和排队的后续输入共用同一套身份与序号规则。
    pub async fn run(
        &mut self,
        input: impl Into<UserInput>,
        sink: &mut (dyn FnMut(TurnEvent) + Send),
    ) -> Result<TurnOutcome, ConversationError> {
        {
            let mut state = self.conversation.lock_state();
            let request = state.next_control(input.into())?;
            state.pending_inputs.push_back(request);
        }
        self.run_pending(sink).await
    }

    /// 从预订时选定的位置开始执行，随后按原顺序消费其余队列。
    pub async fn run_pending(
        &mut self,
        sink: &mut (dyn FnMut(TurnEvent) + Send),
    ) -> Result<TurnOutcome, ConversationError> {
        self.conversation.run_chain(self.first_pending, sink).await
    }

    /// 在已经预订好的压缩窗口里执行；预订一直持有到调用方完成投影收尾。
    pub async fn compact(&mut self) -> Result<(), ConversationError> {
        let (thread, writer, window) = match &self.conversation.lock_state().turn {
            TurnLifecycle::Compacting {
                thread,
                writer,
                window,
            } => (thread.clone(), Arc::clone(writer), Arc::clone(window)),
            _ => unreachable!("compaction reservation owns its execution window"),
        };
        self.conversation
            .runner
            .compact_thread(&thread, &window, writer)
            .await
            .map_err(ConversationError::Turn)
    }
}

impl Drop for TurnReservation {
    fn drop(&mut self) {
        let mut state = self.conversation.lock_state();
        state.turn = TurnLifecycle::Idle;
    }
}

/// 一次「立即发送」原子提升的结果。
pub enum FollowUpPromotion {
    /// 目标集合为空：没有需要交接的输入（「全部发送」遇到空队列）。
    Empty,
    /// 输入已经进入当前 turn 的注入箱，沿用原来的 control 身份。
    Injected,
    /// Session 已经空闲；预订选定下一条输入，消息仍保留在队列中。
    Reserved { reservation: TurnReservation },
}

/// 协调层错误。
#[derive(Debug, thiserror::Error)]
pub enum ConversationError {
    #[error("thread already has an active turn")]
    TurnAlreadyActive,
    #[error("{0}")]
    Configuration(String),
    #[error("任务名称不能为空。")]
    InvalidName,
    #[error(transparent)]
    Control(#[from] ConversationControlError),
    #[error(transparent)]
    Turn(#[from] TurnRunError),
    #[error(transparent)]
    Session(#[from] singularity_agent::session::SessionError),
}

#[derive(Debug, thiserror::Error)]
pub enum ConversationControlError {
    #[error("session is not running")]
    NotRunning,
    #[error("输入不能为空。")]
    InvalidInput,
    #[error("pending control was not found")]
    ControlNotFound,
}

/// 输入正文的共同校验；在接受操作之前调用，空输入不占用执行窗口或队列序号。
pub fn validate_input(input: &UserInput) -> Result<(), ConversationControlError> {
    if input.text.trim().is_empty() && input.images.is_empty() {
        Err(ConversationControlError::InvalidInput)
    } else {
        Ok(())
    }
}

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
            first_pending: 0,
        })
    }

    /// 在空闲会话的写入窗口内执行操作；活动或预订期间返回错误。
    /// 动作执行时不持有状态锁，控制面的读取可以继续。
    pub fn with_idle_writer<T>(&self, action: impl FnOnce() -> T) -> Result<T, ConversationError> {
        let _window = self.lock_writer_window();
        if self.lock_state().turn.is_busy() {
            return Err(ConversationError::TurnAlreadyActive);
        }
        Ok(action())
    }

    /// 待处理输入的图片只存在于内存，预览复用这份字节。
    pub fn pending_image(&self, image_id: &str) -> Option<singularity_agent::image::InputImage> {
        let state = self.lock_state();
        state
            .pending_inputs
            .iter()
            .flat_map(|request| &request.input.images)
            .find(|image| image.attachment.id == image_id)
            .cloned()
            .or_else(|| match &state.turn {
                TurnLifecycle::Running(controls) => controls.lock_inbox().image(image_id),
                _ => None,
            })
    }

    /// 当前 Thread 的投影快照。
    pub fn thread(&self) -> Thread {
        self.lock_state().thread.clone()
    }

    /// 向活动 turn 注入即时引导输入；没有活动 turn，或注入窗口已经关闭时返回错误。接受检查、
    /// 生成身份和输入入箱都在同一个生命周期临界区内完成，避免跨过收尾窗口。
    pub fn steer(&self, input: impl Into<UserInput>) -> Result<(), ConversationControlError> {
        let mut state = self.lock_state();
        // 正文校验放在确认可以注入之后。
        let controls = match &state.turn {
            TurnLifecycle::Running(controls) => Arc::clone(controls),
            _ => return Err(ConversationControlError::NotRunning),
        };
        let request = state.next_control(input.into())?;
        if !controls.enqueue(request) {
            return Err(ConversationControlError::NotRunning);
        }
        Ok(())
    }

    /// 在活动回合之后按先进先出执行输入；空闲时应当直接开始回合。
    pub fn submit_follow_up(
        &self,
        input: impl Into<UserInput>,
    ) -> Result<(), ConversationControlError> {
        self.lock_state().queue_follow_up(input.into())
    }

    /// 修改还没被消费的输入，保留它的身份、接受序号和队列位置。
    pub fn replace_follow_up(
        &self,
        control_id: &str,
        input: impl Into<UserInput>,
    ) -> Result<(), ConversationControlError> {
        let input = input.into();
        validate_input(&input)?;
        let mut state = self.lock_state();
        let position = state.editable_pending_position(control_id)?;
        let request = &mut state.pending_inputs[position];
        request.input = input;
        Ok(())
    }

    /// 立即发送：把目标 pending 输入原子地提升为当前 turn 的输入，空闲时提升为下一条独占
    /// 执行预订；省略 `target` 表示全部待处理输入。读取目标、判定注入窗口和转移所有权共用
    /// 状态锁与当前 turn 的 inbox 锁，调用方不必按自己读到的快照逐条请求。注入窗口已关闭时
    /// 整批保持原位；空闲预订只选定起始位置，开始执行时才从队列取走。
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
                // 注入窗口拒绝时整批保持原位。
                let requests = state
                    .pending_inputs
                    .range(positions.clone())
                    .cloned()
                    .collect();
                if !controls.enqueue_all(requests) {
                    return Err(ConversationControlError::NotRunning);
                }
                state.pending_inputs.drain(positions);
                Ok(FollowUpPromotion::Injected)
            }
            TurnLifecycle::Idle => {
                state.turn = TurnLifecycle::Reserved;
                Ok(FollowUpPromotion::Reserved {
                    reservation: TurnReservation {
                        conversation: Arc::clone(self),
                        first_pending: positions.start,
                    },
                })
            }
            TurnLifecycle::Reserved | TurnLifecycle::Compacting { .. } => {
                Err(ConversationControlError::NotRunning)
            }
        }
    }

    /// 撤回还没被消费的输入，不写入对话历史。
    pub fn withdraw_follow_up(&self, control_id: &str) -> Result<(), ConversationControlError> {
        let mut state = self.lock_state();
        let position = state.editable_pending_position(control_id)?;
        state.pending_inputs.remove(position);
        Ok(())
    }

    /// 为独立压缩预订唯一的操作窗口，并公开共享写者，供设置立即保存；写者打开在状态锁
    /// 之外完成，见模块文档「锁」。
    pub fn reserve_compaction(self: &Arc<Self>) -> Result<TurnReservation, ConversationError> {
        let _window = self.lock_writer_window();
        let thread = {
            let state = self.lock_state();
            if state.turn.is_busy() {
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
            first_pending: 0,
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
            pending_question: match &state.turn {
                TurnLifecycle::Running(controls) => controls.questions.pending(),
                _ => None,
            },
        }
    }

    /// 答案只交付给当前仍在等待的工具调用。
    pub fn answer_question(
        &self,
        item_id: &str,
        answers: Vec<singularity_protocol::UserQuestionAnswer>,
    ) -> Result<(), String> {
        match &self.lock_state().turn {
            TurnLifecycle::Running(controls) if !controls.cancellation().is_cancelled() => {
                controls.questions.answer(item_id, answers)
            }
            _ => Err("该任务已停止等待回答。".into()),
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

    /// 去掉名称首尾空白后立即保存展示名称；运行和压缩期间复用当前写者。
    /// 写者窗口将改名与写者打开、交接串行化，名称不改变模型上下文或执行状态。
    pub fn rename(&self, name: &str) -> Result<(), ConversationError> {
        let name = name.trim();
        if name.is_empty() {
            return Err(ConversationError::InvalidName);
        }
        let _window = self.lock_writer_window();
        let writer = self.metadata_writer()?;
        lock_writer(&writer).append_metadata(SessionMetadata::ThreadName {
            name: name.to_string(),
        })?;
        Ok(())
    }

    /// 校验并立即保存下一轮要用的设置。运行或压缩期间复用当前的会话写者，空闲和预订阶段
    /// 临时开一个写者；写入成功之后才改变内存里的选择。写盘在状态锁之外完成：写者窗口把
    /// 打开和写盘串行化，状态锁只用来读取阶段和提交选择；临时开的写者在本函数返回前释放，
    /// 后续预订在同一个窗口里看到的是已经释放的写者。
    pub fn update_settings(&self, selector: &str) -> Result<(), ConversationError> {
        self.runner
            .validate_model_selector(selector)
            .map_err(ConversationError::Configuration)?;
        let _window = self.lock_writer_window();
        let updated = {
            let state = self.lock_state();
            if state.thread.model.as_deref() == Some(selector) {
                return Ok(());
            }
            let mut updated = state.thread.clone();
            updated.model = Some(selector.to_string());
            updated
        };
        let writer = self.metadata_writer()?;
        crate::thread_catalog::record_thread_settings_metadata(&mut lock_writer(&writer), &updated)
            .map_err(ConversationError::Session)?;
        drop(writer);
        self.lock_state().thread = updated;
        Ok(())
    }

    /// 调用方持有写者窗口；元数据修改复用活动写者，空闲或预订阶段临时打开。
    fn metadata_writer(&self) -> Result<SessionWriter, TurnRunError> {
        let state = self.lock_state();
        if let Some(writer) = state.turn.writer() {
            return Ok(writer);
        }
        let thread = state.thread.clone();
        drop(state);
        self.runner.open_turn_writer(&thread)
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
