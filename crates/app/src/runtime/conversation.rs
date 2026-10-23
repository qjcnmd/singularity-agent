//! 单个任务的内存状态和执行调度。
//!
//! 同一任务同一时刻只运行一个操作（普通回合或独立压缩）。
//!
//! 任务长期持有同一个会话写者；状态锁决定能否开始新操作，写者锁让保存按顺序进行。

mod activity;
mod background;
mod compaction;
mod lifecycle;
mod state;
mod turn;
mod view;

pub(crate) use self::state::TurnControls;
use self::state::{ConversationState, TurnLifecycle};

use std::sync::{Arc, Mutex};

use singularity_agent::agent::{QueuedInput, UserInput};
use singularity_agent::session::{SessionMetadata, SessionWriter, lock_writer};
use singularity_protocol::SessionPhase;

use singularity_protocol::Thread;
use singularity_protocol::TurnEvent;

/// 一个已打开的任务。
pub struct Conversation {
    view: Mutex<view::ConversationView>,
    publish: Arc<dyn Fn(singularity_protocol::StreamEvent) -> u64 + Send + Sync>,
    changed: tokio::sync::Notify,
    home: std::path::PathBuf,
    models: Arc<singularity_model::ModelConfigManager>,
    mcp: Arc<singularity_mcp::McpManager>,
    state: Mutex<ConversationState>,
    writer: SessionWriter,
    bash_tasks: Arc<singularity_agent::tools::BashTasks>,
}

struct Operation {
    source: singularity_protocol::SessionOperation,
    controls: Arc<TurnControls>,
}

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
    /// 用已打开的会话日志建立任务，并开始监听后台命令的状态变化。
    pub fn new(
        session: singularity_agent::session::SessionManager,
        home: std::path::PathBuf,
        models: Arc<singularity_model::ModelConfigManager>,
        mcp: Arc<singularity_mcp::McpManager>,
        publish: Arc<dyn Fn(singularity_protocol::StreamEvent) -> u64 + Send + Sync>,
    ) -> Arc<Self> {
        let model = session
            .entries()
            .iter()
            .rev()
            .find_map(|entry| match entry {
                singularity_agent::session::SessionEntry::Metadata {
                    metadata: SessionMetadata::ThreadSettings { selection },
                    ..
                } => Some(selection.clone()),
                _ => None,
            })
            .expect("session creation and parsing require an initial model selection");
        let thread = Thread {
            thread_id: session.session_id().to_owned(),
            cwd: session.cwd_string(),
            model,
        };
        let usage = crate::runtime::history::session_usage(session.entries());
        let writer = Arc::new(Mutex::new(session));
        let inbox = singularity_agent::agent::SteeringInbox::default_handle();
        let (bash_tasks, updates) =
            singularity_agent::tools::BashTasks::new(Arc::clone(&writer), Arc::clone(&inbox));
        let conversation = Arc::new(Self {
            view: Mutex::new(view::ConversationView::new(usage)),
            publish,
            changed: tokio::sync::Notify::new(),
            home,
            models,
            mcp,
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
        });
        conversation.listen_background(updates);
        conversation
    }

    /// 任务 ID、工作目录和当前模型选择的副本。
    pub fn thread(&self) -> Thread {
        self.lock_state().thread.clone()
    }

    /// 在同一次加锁中检查执行、后台命令和待处理输入，避免检查途中后台通知启动新回合。
    pub fn has_work(&self) -> bool {
        let state = self.lock_state();
        state.turn.is_busy()
            || self.bash_tasks.is_busy()
            || !state.steering_inbox.lock().expect("turn inbox lock poisoned").pending().is_empty()
    }

    pub fn phase(&self) -> SessionPhase {
        self.lock_state().turn.phase()
    }

    /// 排队中的图片从输入队列读取；已保存的图片从当前会话记录查找。
    pub fn image_data(&self, image_id: &str) -> Result<String, crate::runtime::CatalogError> {
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
        if let Some(image) = image {
            return Ok(image.data_url());
        }
        let writer = lock_writer(&self.writer);
        writer.image_data(image_id).map_err(|source| crate::runtime::CatalogError::Session {
            path: writer.path().to_path_buf(),
            source,
        })
    }

    /// 答案只交给仍在等待的提问工具；任务已停止时拒绝。
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
        let TurnLifecycle::Running(controls) = &state.turn else {
            return Err(ConversationError::Busy);
        };
        controls.accept_cancel()?;
        state.steering_inbox.lock().expect("turn inbox lock poisoned").cancel();
        state.auto_resume = false;
        drop(state);
        self.publish_changed(&mut self.lock_view());
        Ok(())
    }

    /// 去掉名称首尾空白后立即保存展示名称。
    pub fn rename(&self, name: &str) -> Result<(), ConversationError> {
        let name = name.trim();
        if name.is_empty() {
            return Err(ConversationError::InvalidName);
        }
        let mut view = self.lock_view();
        lock_writer(&self.writer).append_metadata(SessionMetadata::ThreadName { name: name.to_string() })?;
        self.publish_summary(&mut view);
        Ok(())
    }

    /// 保存成功后更新下一轮使用的模型选择。
    pub fn update_settings(
        &self,
        selection: &singularity_protocol::ModelSelection,
    ) -> Result<(), ConversationError> {
        self.validate_model_selection(selection).map_err(ConversationError::Configuration)?;
        let mut view = self.lock_view();
        let mut state = self.lock_state();
        if &state.thread.model == selection {
            return Ok(());
        }
        let mut writer = lock_writer(&self.writer);
        writer.append_metadata(SessionMetadata::ThreadSettings { selection: selection.clone() })?;
        state.thread.model = selection.clone();
        drop(writer);
        drop(state);
        self.publish_changed(&mut view);
        Ok(())
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, ConversationState> {
        self.state.lock().expect("conversation state lock poisoned")
    }
}
