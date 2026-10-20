//! 会话内存状态与执行窗口协调器。
//!
//! 同一会话同一时刻只允许执行一个操作（回合或独立上下文压缩）。
//!
//! 会话长期持有同一个写者；状态锁负责操作准入，写者锁串行保存记录。

mod background;
mod execution;
mod state;

pub(crate) use self::state::TurnControls;
use self::state::{ConversationState, TurnLifecycle};

use std::sync::{Arc, Mutex};

use singularity_agent::agent::{QueuedInput, UserInput};
use singularity_agent::session::{SessionMetadata, SessionWriter, lock_writer};
use singularity_protocol::SessionPhase;

use crate::error::TurnRunError;
use crate::runner::{TurnOutcome, TurnRunner};
use singularity_protocol::Thread;
use singularity_protocol::TurnEvent;

/// 会话状态快照：运行阶段、模型选择、上下文窗口、已接收输入与等待中的提问。
pub struct ConversationSnapshot {
    pub phase: SessionPhase,
    pub selector: Option<String>,
    /// 本回合冻结的有效上下文容量上限（用于解释请求用量百分比）
    pub model_context_window: Option<u64>,
    pub pending_question: Option<singularity_protocol::PendingQuestion>,
    pub pending_messages: Vec<singularity_protocol::PendingUserMessage>,
}

/// 单个会话的核心协调器。
pub struct Conversation {
    runner: Arc<TurnRunner>,
    /// 会话状态与控制信号互斥锁
    state: Mutex<ConversationState>,
    writer: SessionWriter,
    bash_tasks: Arc<singularity_agent::tools::BashTasks>,
}

/// 已接受的操作；执行结束后由会话统一释放窗口并处理排队输入。
pub struct OperationReservation {
    operation: ReservedOperation,
    conversation: Arc<Conversation>,
}

enum ReservedOperation {
    Turn,
    Compaction {
        thread: Thread,
        cancellation: tokio_util::sync::CancellationToken,
    },
}

/// 操作结果保留来源；宿主据此展示回合或独立压缩的结果。
pub enum OperationResult {
    Turn(Result<TurnOutcome, TurnRunError>),
    Compaction(Result<(), TurnRunError>),
}

impl OperationReservation {
    /// 执行一个操作，完成后交给宿主发布结果和继续处理队列。
    pub async fn execute(self, sink: &mut (dyn FnMut(TurnEvent) + Send)) -> OperationResult {
        let conversation = self.conversation;
        match self.operation {
            ReservedOperation::Turn => OperationResult::Turn(conversation.run_single_turn(sink).await),
            ReservedOperation::Compaction { thread, cancellation } => OperationResult::Compaction(
                conversation
                    .runner
                    .compact_thread(
                        &thread,
                        Arc::clone(&conversation.writer),
                        Arc::clone(&conversation.bash_tasks),
                        &cancellation,
                        sink,
                    )
                    .await,
            ),
        }
    }
}

/// 协调层错误。
#[derive(Debug, thiserror::Error)]
pub enum ConversationError {
    #[error("session cannot accept this operation in its current phase")]
    Busy,
    #[error("{0}")]
    Configuration(String),
    #[error("任务名称不能为空。")]
    InvalidName,
    #[error(transparent)]
    Session(#[from] singularity_agent::session::SessionError),
}

impl Conversation {
    /// 建立任务协调器。
    pub fn new(
        runner: Arc<TurnRunner>,
        session: singularity_agent::session::SessionManager,
    ) -> (Arc<Self>, tokio::sync::mpsc::Receiver<singularity_agent::tools::BashTaskUpdate>) {
        let model = session.entries().iter().rev().find_map(|entry| match entry {
            singularity_agent::session::SessionEntry::Metadata {
                metadata: SessionMetadata::ThreadSettings { provider, model, reasoning },
                ..
            } => Some(singularity_model::compose_model_selector(provider, model, reasoning.as_deref())),
            _ => None,
        });
        let thread = Thread {
            thread_id: session.session_id().to_owned(),
            cwd: session.cwd_string(),
            model,
        };
        let writer = Arc::new(Mutex::new(session));
        let inbox = singularity_agent::agent::SteeringInbox::default_handle();
        let (bash_tasks, updates) =
            singularity_agent::tools::BashTasks::new(Arc::clone(&writer), Arc::clone(&inbox));
        (
            Arc::new(Self {
                runner,
                state: Mutex::new(ConversationState {
                    thread,
                    turn: TurnLifecycle::Idle,
                    steering_inbox: inbox,
                    last_context_window: None,
                    auto_resume: true,
                    closing: false,
                }),
                writer,
                bash_tasks,
            }),
            updates,
        )
    }

    /// 当前 Thread 的投影快照。
    pub fn thread(&self) -> Thread {
        self.lock_state().thread.clone()
    }

    /// 首条与追加消息统一入箱；空闲时同时预订普通回合。
    /// 启动或收尾期间返回占用，宿主等待状态变化后继续提交同一份输入。
    pub fn submit(
        self: &Arc<Self>,
        input: &UserInput,
    ) -> Result<Option<OperationReservation>, ConversationError> {
        let mut state = self.lock_state();
        let start = match state.turn.phase() {
            SessionPhase::Idle => true,
            SessionPhase::Running | SessionPhase::Compacting | SessionPhase::Reserved => false,
            SessionPhase::Stopping => return Err(ConversationError::Busy),
        };
        let mut inbox = state.steering_inbox.lock().expect("turn inbox lock poisoned");
        inbox.enqueue(QueuedInput::new(input.clone()));
        drop(inbox);
        state.auto_resume = true;
        if !start {
            return Ok(None);
        }
        state.turn = TurnLifecycle::Reserved;
        Ok(Some(OperationReservation {
            conversation: Arc::clone(self),
            operation: ReservedOperation::Turn,
        }))
    }

    /// 为独立压缩预订操作窗口，复用会话写者。
    pub fn reserve_compaction(self: &Arc<Self>) -> Result<OperationReservation, ConversationError> {
        let mut state = self.lock_state();
        if state.turn.is_busy() {
            return Err(ConversationError::Busy);
        }
        let thread = state.thread.clone();
        let cancellation = tokio_util::sync::CancellationToken::new();
        state.turn = TurnLifecycle::Compacting { cancellation: cancellation.clone() };
        Ok(OperationReservation {
            conversation: Arc::clone(self),
            operation: ReservedOperation::Compaction { thread, cancellation },
        })
    }

    /// 是否有活动或已预订的执行操作。
    pub fn is_occupied(&self) -> bool {
        self.lock_state().turn.is_busy()
    }

    /// 是否还有未消费的输入，包括后台完成通知和暂停后保留的输入。
    pub fn has_pending_input(&self) -> bool {
        let state = self.lock_state();
        !state.steering_inbox.lock().expect("turn inbox lock poisoned").pending().is_empty()
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
            pending_messages: state
                .steering_inbox
                .lock()
                .expect("turn inbox lock poisoned")
                .pending()
                .iter()
                .filter_map(|request| {
                    request.user_input().map(|input| singularity_protocol::PendingUserMessage {
                        item_id: request.item_id(),
                        text: input.text.clone(),
                        images: input.images.iter().map(|image| image.attachment.clone()).collect(),
                    })
                })
                .collect(),
        }
    }

    /// 读取已接收但尚未落盘的图片，消费后由普通历史读取接手。
    pub fn pending_image(&self, image_id: &str) -> Option<String> {
        let image = {
            let state = self.lock_state();
            let inbox = state.steering_inbox.lock().expect("turn inbox lock poisoned");
            inbox
                .pending()
                .iter()
                .filter_map(QueuedInput::user_input)
                .flat_map(|input| &input.images)
                .find(|image| image.attachment.id == image_id)
                .cloned()
        };
        image.map(|image| image.data_url())
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

    /// 停止当前回合或压缩，清空尚未消费的输入。
    pub fn abort(&self) -> Result<(), ConversationError> {
        let mut state = self.lock_state();
        match &state.turn {
            TurnLifecycle::Running(controls) => controls.accept_cancel()?,
            TurnLifecycle::Compacting { cancellation } => {
                cancellation.cancel();
                state.steering_inbox.lock().expect("turn inbox lock poisoned").cancel();
            }
            _ => return Err(ConversationError::Busy),
        }
        state.auto_resume = false;
        Ok(())
    }

    /// 去掉名称首尾空白后立即保存展示名称。
    pub fn rename(&self, name: &str) -> Result<(), ConversationError> {
        let name = name.trim();
        if name.is_empty() {
            return Err(ConversationError::InvalidName);
        }
        lock_writer(&self.writer).append_metadata(SessionMetadata::ThreadName { name: name.to_string() })?;
        Ok(())
    }

    /// 保存成功后更新下一轮使用的模型选择。
    pub fn update_settings(
        &self,
        selector: &str,
    ) -> Result<Option<singularity_protocol::HistoryItem>, ConversationError> {
        self.runner.validate_model_selector(selector).map_err(ConversationError::Configuration)?;
        let mut state = self.lock_state();
        if state.thread.model.as_deref() == Some(selector) {
            return Ok(None);
        }
        let mut writer = lock_writer(&self.writer);
        crate::thread_catalog::record_thread_settings_metadata(&mut writer, selector)
            .map_err(ConversationError::Session)?;
        state.thread.model = Some(selector.to_string());
        let singularity_agent::session::SessionEntry::Metadata { id, metadata, .. } =
            writer.entries().last().expect("settings just appended")
        else {
            unreachable!("settings append creates metadata")
        };
        Ok(crate::history::metadata_item(id, metadata))
    }

    /// 从当前会话事实生成历史，不重复打开或解析日志。
    pub fn history(&self) -> Arc<crate::ThreadSnapshot> {
        Arc::new(crate::ThreadSnapshot::new(lock_writer(&self.writer).data_snapshot()))
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, ConversationState> {
        self.state.lock().expect("conversation state lock poisoned (fail-stop)")
    }
}
