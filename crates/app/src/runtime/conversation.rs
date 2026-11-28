mod activity;
mod background;
mod lifecycle;
mod state;
mod turn;
mod view;

use self::state::{ConversationState, TurnControls, TurnLifecycle};

use std::sync::{Arc, Mutex};

use agent::agent::QueuedInput;
use agent::session::{SessionMetadata, SessionWriter, lock_writer};
use protocol::SessionPhase;

use crate::desktop::stream_hub::StreamHub;

struct ConversationInner {
    view: view::ConversationView,
    state: ConversationState,
}

pub(crate) struct Conversation {
    pub(crate) thread_id: String,
    pub(crate) cwd: String,
    pub(crate) disabled_modules: Vec<String>,
    inner: Mutex<ConversationInner>,
    hub: Arc<StreamHub>,
    changed: tokio::sync::Notify,
    home: std::path::PathBuf,
    models: Arc<model::ModelConfigManager>,
    mcp: Arc<mcp::McpManager>,
    writer: SessionWriter,
    bash_tasks: Arc<agent::tools::BashTasks>,
}

struct Operation {
    source: protocol::SessionOperation,
    controls: Arc<TurnControls>,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ConversationError {
    #[error("session cannot accept this operation in its current phase")]
    Busy,
    #[error("{0}")]
    Message(String),
    #[error(transparent)]
    Agent(#[from] agent::agent::AgentError),
    #[error(transparent)]
    Session(#[from] agent::session::SessionError),
}

impl Conversation {
    pub(crate) fn new(
        session: agent::session::SessionManager,
        home: std::path::PathBuf,
        models: Arc<model::ModelConfigManager>,
        mcp: Arc<mcp::McpManager>,
        initial_revision: u64,
        hub: Arc<StreamHub>,
    ) -> Arc<Self> {
        let data = session.data();
        let model = data.latest_selection().clone();
        let thread_id = data.session_id().to_owned();
        let cwd = data.cwd_string();
        let disabled_modules = data.disabled_modules();
        let usage = crate::runtime::history::session_usage(data.entries());
        let context_window = models.context_window(&model);
        let writer = Arc::new(Mutex::new(session));
        let inbox = Arc::new(std::sync::Mutex::new(agent::agent::SteeringInbox::default()));
        Arc::new_cyclic(|weak: &std::sync::Weak<Self>| {
            let weak = weak.clone();
            let bash_tasks = agent::tools::BashTasks::new(
                Arc::clone(&writer),
                Arc::clone(&inbox),
                Arc::new(move |update| {
                    if let Some(conversation) = weak.upgrade() {
                        conversation.on_background_update(update);
                    }
                }),
            );
            Self {
                thread_id,
                cwd,
                disabled_modules,
                inner: Mutex::new(ConversationInner {
                    view: view::ConversationView::new(usage, initial_revision),
                    state: ConversationState {
                        model,
                        turn: TurnLifecycle::Idle,
                        steering_inbox: inbox,
                        last_context_window: context_window,
                        auto_resume: true,
                        closing: false,
                    },
                }),
                hub,
                changed: tokio::sync::Notify::new(),
                home,
                models,
                mcp,
                writer,
                bash_tasks,
            }
        })
    }

    pub(crate) fn model(&self) -> protocol::ModelSelection {
        self.lock_inner().state.model.clone()
    }

    pub(crate) fn has_active_work(&self) -> bool {
        let inner = self.lock_inner();
        inner.state.closing || inner.state.has_work(&self.bash_tasks)
    }

    pub(crate) fn can_evict(&self) -> bool {
        let inner = self.lock_inner();
        !inner.state.closing && inner.view.failure.is_none() && !inner.state.has_work(&self.bash_tasks)
    }

    pub(super) fn phase(&self) -> SessionPhase {
        self.lock_inner().state.turn.phase()
    }

    pub(crate) fn image_data(&self, image_id: &str) -> Result<String, crate::runtime::CatalogError> {
        let image = {
            let inner = self.lock_inner();
            let inbox = inner.state.steering_inbox.lock().expect("turn inbox lock poisoned");
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
        let (attachment, path) = {
            let writer = lock_writer(&self.writer);
            let data = writer.data();
            (data.image_attachment(image_id), data.path().to_path_buf())
        };
        attachment
            .and_then(|image| image.data_url().map_err(Into::into))
            .map_err(|source| crate::runtime::CatalogError::Session { path, source })
    }

    pub(crate) fn answer_question(
        &self,
        item_id: &str,
        answers: Vec<protocol::UserQuestionAnswer>,
    ) -> Result<(), ConversationError> {
        match &self.lock_inner().state.turn {
            TurnLifecycle::Running(controls) if !controls.cancellation().is_cancelled() => {
                controls.questions.answer(item_id, answers).map_err(ConversationError::Message)
            }
            _ => Err(ConversationError::Message("该任务已停止等待回答。".into())),
        }
    }

    pub(crate) fn abort(&self) -> Result<(), ConversationError> {
        let mut inner = self.lock_inner();
        let TurnLifecycle::Running(controls) = &inner.state.turn else {
            return Err(ConversationError::Busy);
        };
        controls.accept_cancel()?;
        inner.state.steering_inbox.lock().expect("turn inbox lock poisoned").cancel();
        inner.state.auto_resume = false;
        self.publish_changed(&mut inner);
        Ok(())
    }

    pub(crate) fn rename(&self, name: &str) -> Result<(), ConversationError> {
        let name = name.trim();
        if name.is_empty() {
            return Err(ConversationError::Message("任务名称不能为空。".into()));
        }
        let mut inner = self.lock_inner();
        if inner.state.closing {
            return Err(ConversationError::Busy);
        }
        lock_writer(&self.writer).append_metadata(SessionMetadata::ThreadName { name: name.to_string() })?;
        self.publish_summary(&mut inner);
        Ok(())
    }

    pub(crate) fn update_settings(
        &self,
        selection: &protocol::ModelSelection,
    ) -> Result<(), ConversationError> {
        let context_window = self
            .models
            .validate_selection(selection)
            .map_err(|error| ConversationError::Message(format!("invalid model selection: {error}")))?;
        let mut inner = self.lock_inner();
        if inner.state.closing {
            return Err(ConversationError::Busy);
        }
        if &inner.state.model == selection {
            return Ok(());
        }
        lock_writer(&self.writer)
            .append_metadata(SessionMetadata::ThreadSettings { selection: selection.clone() })?;
        inner.state.model = selection.clone();
        inner.state.last_context_window = Some(context_window);
        self.publish_changed(&mut inner);
        Ok(())
    }

    fn lock_inner(&self) -> std::sync::MutexGuard<'_, ConversationInner> {
        self.inner.lock().expect("conversation inner lock poisoned")
    }
}
