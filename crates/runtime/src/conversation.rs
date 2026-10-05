//! 一个 session 的内存输入，以及它唯一的活动执行窗口。
//! 已经被消费的输入由 Agent 落盘；还在等待处理的输入只活在进程里，进程结束就没了。
//!
//! # 锁
//!
//! 两个锁各管一件事，加锁顺序固定为「写者窗口 → 状态」，不会互相反向等待：
//!
//! - `writer_window`：会话写者的打开、回合准备、Running→Reserved 的交接与元数据写盘在这里互斥；
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
use self::state::{ConversationState, TurnLifecycle};

use std::sync::{Arc, Mutex};

use singularity_agent::agent::{ControlRequest, UserInput};
use singularity_agent::session::{SessionMetadata, SessionWriter, lock_writer};
use singularity_protocol::SessionPhase;

use crate::error::TurnRunError;
use crate::runner::{TurnOutcome, TurnRunner};
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
    pub pending_input: Option<singularity_protocol::PendingInput>,
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

/// 一次执行预订；绑定操作和输入，队列在回合准备成功前保持原位，drop 时释放窗口。
pub struct OperationReservation {
    conversation: Arc<Conversation>,
    operation: Option<ReservedOperation>,
}

enum ReservedOperation {
    Turn(TurnInput),
    Compaction,
}

enum TurnInput {
    Submitted(ControlRequest),
    Queued,
}

/// 操作结果保留来源；宿主据此展示回合或独立压缩的结果。
pub enum OperationResult {
    Turn(Result<TurnOutcome, ConversationError>),
    Compaction(Result<(), ConversationError>),
}

impl OperationReservation {
    /// 执行预订时绑定的操作一次。结果返回后仍占用窗口；宿主完成事件投影后再释放预订。
    pub async fn execute(&mut self, sink: &mut (dyn FnMut(TurnEvent) + Send)) -> OperationResult {
        match self.operation.take().expect("reservation executes once") {
            ReservedOperation::Turn(input) => {
                OperationResult::Turn(self.conversation.run_chain(input, sink).await)
            }
            ReservedOperation::Compaction => OperationResult::Compaction(self.compact(sink).await),
        }
    }

    async fn compact(&self, sink: &mut (dyn FnMut(TurnEvent) + Send)) -> Result<(), ConversationError> {
        let (thread, writer, window) = match &self.conversation.lock_state().turn {
            TurnLifecycle::Compacting { thread, writer, window } => {
                (thread.clone(), Arc::clone(writer), Arc::clone(window))
            }
            _ => unreachable!("compaction reservation owns its execution window"),
        };
        self.conversation
            .runner
            .compact_thread(&thread, &window, writer, sink)
            .await
            .map_err(ConversationError::Turn)
    }
}

impl Drop for OperationReservation {
    fn drop(&mut self) {
        let mut state = self.conversation.lock_state();
        state.turn = TurnLifecycle::Idle;
    }
}

/// 一次「立即发送」原子提升的结果。
pub enum FollowUpPromotion {
    /// 输入已经进入当前 turn 的注入箱，沿用原来的 control 身份。
    Injected,
    /// Session 已经空闲；预订选定下一条输入，消息仍保留在队列中。
    Reserved { reservation: OperationReservation },
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
    #[error("会话已有一条排队消息。")]
    PendingInputExists,
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
                pending_input: None,
                steering_inbox: singularity_agent::agent::SteeringInbox::default_handle(),
                control_sequence: 0,
                last_context_window: None,
            }),
            writer_window: Mutex::new(()),
        })
    }

    /// 校验并绑定本轮输入，原子预订执行链窗口；execute 时交给 Runner。
    /// 放弃预订不留下输入。预订与写者打开和交接在同一处串行。
    pub fn reserve_start(
        self: &Arc<Self>,
        input: impl Into<UserInput>,
    ) -> Result<OperationReservation, ConversationError> {
        let _window = self.lock_writer_window();
        let mut state = self.lock_state();
        if state.turn.is_busy() {
            return Err(ConversationError::TurnAlreadyActive);
        }
        if state.pending_input.is_some() {
            return Err(ConversationControlError::PendingInputExists.into());
        }
        let request = state.next_control(input.into())?;
        state.turn = TurnLifecycle::Reserved;
        Ok(OperationReservation {
            conversation: Arc::clone(self),
            operation: Some(ReservedOperation::Turn(TurnInput::Submitted(request))),
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
            .pending_input
            .iter()
            .flat_map(|request| &request.input.images)
            .find(|image| image.attachment.id == image_id)
            .cloned()
            .or_else(|| state.steering_inbox.lock().expect("steering inbox lock poisoned").image(image_id))
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

    /// 排队一条输入，在活动回合正常完成后执行；空闲时应当直接开始回合。
    pub fn submit_follow_up(&self, input: impl Into<UserInput>) -> Result<(), ConversationControlError> {
        self.lock_state().queue_follow_up(input.into())
    }

    /// 原子取回尚未消费的完整输入，交给工作台继续编辑；取走后本轮不再消费它。
    pub fn take_follow_up(&self, control_id: &str) -> Result<UserInput, ConversationControlError> {
        let mut state = self.lock_state();
        state.editable_pending_input(control_id)?;
        Ok(state.pending_input.take().expect("located pending input exists").input)
    }

    /// 发送排队输入：运行时原子交给当前 turn 的 steer 输入箱，空闲时预订下一轮。
    /// 注入窗口关闭时保持原位；空闲预订在回合准备成功后才取走输入。
    pub fn promote_pending(
        self: &Arc<Self>,
        control_id: &str,
    ) -> Result<FollowUpPromotion, ConversationControlError> {
        // 空闲分支会发布预订窗口，因此要和写者窗口串行。
        let _window = self.lock_writer_window();
        let mut state = self.lock_state();
        let request = state.editable_pending_input(control_id)?;
        match &state.turn {
            TurnLifecycle::Running(controls) => {
                if !controls.enqueue(request.clone()) {
                    return Err(ConversationControlError::NotRunning);
                }
                state.pending_input.take();
                Ok(FollowUpPromotion::Injected)
            }
            TurnLifecycle::Idle => {
                state.turn = TurnLifecycle::Reserved;
                Ok(FollowUpPromotion::Reserved {
                    reservation: OperationReservation {
                        conversation: Arc::clone(self),
                        operation: Some(ReservedOperation::Turn(TurnInput::Queued)),
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
        self.take_follow_up(control_id)?;
        Ok(())
    }

    /// 为独立压缩预订唯一的操作窗口，并公开共享写者，供设置立即保存；写者打开在状态锁
    /// 之外完成，见模块文档「锁」。
    pub fn reserve_compaction(self: &Arc<Self>) -> Result<OperationReservation, ConversationError> {
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
        Ok(OperationReservation {
            conversation: Arc::clone(self),
            operation: Some(ReservedOperation::Compaction),
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
            pending_input: state.pending_input(),
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
        lock_writer(&writer).append_metadata(SessionMetadata::ThreadName { name: name.to_string() })?;
        Ok(())
    }

    /// 校验并立即保存下一轮要用的设置。运行或压缩期间复用当前的会话写者，空闲和预订阶段
    /// 临时开一个写者；写入成功之后才改变内存里的选择。写盘在状态锁之外完成：写者窗口把
    /// 打开和写盘串行化，状态锁只用来读取阶段和提交选择；临时开的写者在本函数返回前释放，
    /// 后续预订在同一个窗口里看到的是已经释放的写者。
    pub fn update_settings(&self, selector: &str) -> Result<(), ConversationError> {
        self.runner.validate_model_selector(selector).map_err(ConversationError::Configuration)?;
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
        self.state.lock().expect("conversation state lock poisoned (fail-stop)")
    }

    /// 写者窗口：打开写者、交接写者和写盘都在这里串行。持有本窗口时只取状态锁做
    /// 短暂的读写，绝不反向等待状态锁的持有者，见模块文档「锁」。
    fn lock_writer_window(&self) -> std::sync::MutexGuard<'_, ()> {
        self.writer_window.lock().expect("conversation writer window poisoned (fail-stop)")
    }
}
