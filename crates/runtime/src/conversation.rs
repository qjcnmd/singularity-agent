//! 会话内存状态与执行窗口协调器。
//!
//! 同一会话同一时刻只允许执行一个操作（回合或独立上下文压缩）。
//!
//! # 锁顺序
//! 固定为 `writer_window` -> `state`：
//! - `writer_window`: 串行化文件 I/O、写者创建与回合交接。
//! - `state`: 维护生命周期阶段、模型选择与待处理输入队列（steer/abort）。

mod execution;
mod state;

pub(crate) use self::state::TurnControls;
use self::state::{ConversationState, TurnLifecycle};

use std::sync::{Arc, Mutex};

use singularity_agent::agent::{ControlRequest, UserInput};
use singularity_agent::session::{SessionMetadata, SessionWriter, lock_writer};
use singularity_protocol::SessionPhase;

use crate::error::TurnRunError;
use crate::runner::{TurnOutcome, TurnRunner};
use singularity_protocol::Thread;
use singularity_protocol::TurnEvent;

/// 会话状态快照：生命周期阶段、模型选择、冻结的上下文窗口大小与等待中的提问。
pub struct ConversationSnapshot {
    pub phase: SessionPhase,
    pub selector: Option<String>,
    /// 本回合冻结的有效上下文容量上限（用于解释请求用量百分比）
    pub model_context_window: Option<u64>,
    pub pending_question: Option<singularity_protocol::PendingQuestion>,
}

/// 单个会话的核心协调器。
pub struct Conversation {
    runner: Arc<TurnRunner>,
    /// 会话状态与控制信号互斥锁
    state: Mutex<ConversationState>,
    /// 文件写入与生命周期交接互斥锁
    writer_window: Mutex<()>,
}

/// 执行预订凭证：绑定待执行操作与生命周期守卫，Drop 时自动释放窗口。
pub struct OperationReservation {
    operation: ReservedOperation,
    guard: OperationGuard,
}

/// 操作独占守卫：保持占用状态直至完成投影与状态结算。
#[must_use = "hold the operation guard until settlement is complete"]
pub struct OperationGuard {
    conversation: Arc<Conversation>,
}

enum ReservedOperation {
    Turn(ControlRequest),
    Compaction { thread: Thread, writer: SessionWriter },
}

/// 操作结果保留来源；宿主据此展示回合或独立压缩的结果。
pub enum OperationResult {
    Turn(Result<TurnOutcome, ConversationError>),
    Compaction(Result<(), ConversationError>),
}

impl OperationReservation {
    /// 提交绑定的消息身份；独立压缩没有提交回执。
    pub fn submitted_item_id(&self) -> Option<String> {
        match &self.operation {
            ReservedOperation::Turn(request) => Some(request.item_id()),
            _ => None,
        }
    }

    /// 消费预订并执行一次。返回的守卫继续占用窗口，宿主完成事件投影后再释放它。
    pub async fn execute(
        self,
        sink: &mut (dyn FnMut(TurnEvent) + Send),
    ) -> (OperationResult, OperationGuard) {
        let Self { guard, operation } = self;
        let result = match operation {
            ReservedOperation::Turn(input) => OperationResult::Turn(
                guard.conversation.run_single_turn(input, sink).await.map_err(Into::into),
            ),
            ReservedOperation::Compaction { thread, writer } => OperationResult::Compaction(
                guard
                    .conversation
                    .runner
                    .compact_thread(&thread, writer, sink)
                    .await
                    .map_err(ConversationError::Turn),
            ),
        };
        (result, guard)
    }
}

impl Drop for OperationGuard {
    fn drop(&mut self) {
        let mut state = self.conversation.lock_state();
        state.turn = TurnLifecycle::Idle;
    }
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
    #[error("正在压缩上下文，完成前不能停止。")]
    Compacting,
    #[error("输入不能为空。")]
    InvalidInput,
}

/// 输入正文的共同校验；在接受操作之前调用，空输入不占用执行窗口或接受序号。
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
                steering_inbox: singularity_agent::agent::SteeringInbox::default_handle(),
                control_sequence: 0,
                last_context_window: None,
            }),
            writer_window: Mutex::new(()),
        })
    }

    /// 校验并绑定本轮输入，原子预订执行窗口，execute 时交给 Runner。放弃预订不留下
    /// 输入。预订与写者打开、交接在同一处串行。
    pub fn reserve_start(
        self: &Arc<Self>,
        input: impl Into<UserInput>,
    ) -> Result<OperationReservation, ConversationError> {
        let _window = self.lock_writer_window();
        let mut state = self.lock_state();
        if state.turn.is_busy() {
            return Err(ConversationError::TurnAlreadyActive);
        }
        let request = state.next_control(input.into())?;
        state.turn = TurnLifecycle::Reserved;
        Ok(OperationReservation {
            guard: OperationGuard { conversation: Arc::clone(self) },
            operation: ReservedOperation::Turn(request),
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

    /// 当前 Thread 的投影快照。
    pub fn thread(&self) -> Thread {
        self.lock_state().thread.clone()
    }

    /// 向活动 turn 注入即时引导输入；没有活动 turn，或注入窗口已经关闭时返回错误。接受检查、
    /// 生成身份和输入入箱都在同一个生命周期临界区内完成，避免跨过收尾窗口。
    pub fn steer(&self, input: impl Into<UserInput>) -> Result<(), ConversationControlError> {
        let mut state = self.lock_state();
        // 先判断能否注入，再校验正文，NotRunning 优先于空输入。
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

    /// 为独立压缩预订操作窗口，并公开共享写者，供设置立即保存；写者打开在状态锁之外
    /// 完成，见模块文档「锁」。
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
        self.lock_state().turn = TurnLifecycle::Compacting { writer: Arc::clone(&writer) };
        Ok(OperationReservation {
            guard: OperationGuard { conversation: Arc::clone(self) },
            operation: ReservedOperation::Compaction { thread, writer },
        })
    }

    /// 是否有活动或已预订的执行操作。
    pub fn is_occupied(&self) -> bool {
        self.lock_state().turn.is_busy()
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
            model_context_window: state.last_context_window,
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

    /// 停止普通任务并清空未消费输入；手动和自动压缩期间都拒绝停止。
    pub fn abort(&self) -> Result<(), ConversationControlError> {
        match &self.lock_state().turn {
            TurnLifecycle::Running(controls) => controls.accept_cancel(),
            TurnLifecycle::Compacting { .. } => Err(ConversationControlError::Compacting),
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

    /// 校验并保存下一轮生效的设置。运行或压缩期间复用当前的会话写者，空闲和预订阶段临时
    /// 开一个；写盘成功之后才更新内存里的选择。写盘在状态锁之外完成，写者窗口把打开和写盘
    /// 串行化，临时开的写者在函数返回前释放，后续预订在同一窗口里看到的是已释放的写者。
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

    /// 写者窗口：写者打开、交接和写盘都在这把锁上串行。持有本窗口时只取状态锁做短暂的
    /// 读写，不得反向等待状态锁的持有者，见模块文档「锁」。
    fn lock_writer_window(&self) -> std::sync::MutexGuard<'_, ()> {
        self.writer_window.lock().expect("conversation writer window poisoned (fail-stop)")
    }
}
