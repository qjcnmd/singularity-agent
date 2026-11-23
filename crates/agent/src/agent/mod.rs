mod compaction;
pub use compaction::compaction_unneeded;
mod dispatch;
mod inbox;
pub mod modules;
mod questions;
pub(crate) mod request;

use std::sync::Arc;

use model::{ModelClient, ProviderError};
use thiserror::Error;
use tokio_util::sync::CancellationToken;

pub use self::inbox::{QueuedInput, SkillExpansion, SteeringInbox, SteeringInboxHandle, UserInput};
pub use self::questions::UserQuestions;
pub use crate::image::InputImage;
use protocol::TurnEvent;

#[derive(Debug, Clone, PartialEq)]
pub enum AgentEvent {
    Turn(TurnEvent),
    ProviderAttempt {
        observation: protocol::RequestObservation,
    },
    SessionStateChanged,
}

use self::inbox::{QueuedInputKind, lock_inbox};
use crate::message::public_items;
use crate::session::{MessageDisplay, SessionError, SessionWriter, with_writer};
use crate::tools::ToolRegistrySnapshot;
use model::Message;

#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub instruction_home: std::path::PathBuf,
    pub initial_instructions: Vec<app_core::InstructionFile>,
    pub disabled_modules: Vec<String>,
}

#[derive(Debug, Error)]
pub enum AgentError {
    #[error("session error: {0}")]
    Session(#[from] SessionError),
    #[error("provider error: {0}")]
    Provider(#[from] ProviderError),
    #[error("{0}")]
    Context(#[from] std::io::Error),
    #[error("agent operation aborted")]
    Aborted,
    #[error("模型回复达到输出上限，内容未完成。请发送消息继续。")]
    OutputTruncated,
    #[error("{0}")]
    InvalidCompaction(String),
    #[error("{source}")]
    Compaction {
        item_id: String,
        source: Box<AgentError>,
    },
}

impl AgentError {
    pub fn is_session_failure(&self) -> bool {
        match self {
            Self::Session(_) => true,
            Self::Compaction { source, .. } => source.is_session_failure(),
            _ => false,
        }
    }

    pub fn detail(&self) -> protocol::TurnErrorDetail {
        let item_id = match self {
            Self::Compaction { item_id, .. } => Some(item_id.clone()),
            _ => None,
        };
        protocol::TurnErrorDetail { message: self.to_string(), item_id }
    }
}

pub(crate) type Result<T> = std::result::Result<T, AgentError>;

pub struct RequestContext {
    session: SessionWriter,
    registry: ToolRegistrySnapshot,
    mcp: Arc<mcp::McpManager>,
    definitions: Option<request::RequestDefinitions>,
    provider: ModelClient,
    models: Arc<model::ModelConfigManager>,
    config: AgentConfig,
}

impl std::fmt::Debug for RequestContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RequestContext")
            .field("provider", &self.provider)
            .field("config", &self.config)
            .field("definitions_loaded", &self.definitions.is_some())
            .finish_non_exhaustive()
    }
}
impl RequestContext {
    pub async fn inspect_context(
        mut self,
        cancellation: &CancellationToken,
    ) -> Result<protocol::InjectedContext> {
        let (_, context) = self.load_definitions(cancellation).await?;
        Ok(context)
    }

    pub fn new(
        provider: ModelClient,
        models: Arc<model::ModelConfigManager>,
        config: AgentConfig,
        session: SessionWriter,
        mcp: Arc<mcp::McpManager>,
    ) -> Self {
        Self {
            session,
            registry: ToolRegistrySnapshot::default(),
            mcp,
            definitions: None,
            provider,
            models,
            config,
        }
    }

    pub fn context_window(&self) -> u64 {
        self.provider.model_configuration().context_window()
    }

    pub fn model_configuration(&self) -> model::ModelConfigurationSnapshot {
        self.provider.model_configuration()
    }
}

#[derive(Debug)]
pub struct Agent {
    requests: RequestContext,
    bash_tasks: Arc<crate::tools::BashTasks>,
    questions: Arc<UserQuestions>,
    inbox: SteeringInboxHandle,
}
impl Agent {
    pub fn new(
        requests: RequestContext,
        inbox: SteeringInboxHandle,
        questions: Arc<UserQuestions>,
        bash_tasks: Arc<crate::tools::BashTasks>,
    ) -> Self {
        Self { requests, bash_tasks, questions, inbox }
    }

    pub async fn run(
        &mut self,
        on_event: &mut (dyn FnMut(AgentEvent) + Send),
        cancellation: &CancellationToken,
    ) -> Result<()> {
        self.drain_inputs(on_event, cancellation).await?;

        self.requests.ensure_definitions(on_event, cancellation).await?;

        loop {
            self.drain_inputs(on_event, cancellation).await?;
            let (response, assistant_result_entry_id, saved_at) =
                self.generate_response(on_event, cancellation).await?;
            let length_truncated = response.is_length_truncated();
            let assistant = response.assistant_message;

            let tool_calls = assistant.tool_calls().cloned().collect::<Vec<_>>();
            let public_items = public_items(
                &assistant,
                None,
                &assistant_result_entry_id,
                protocol::TurnStatus::Completed,
                protocol::ToolStatus::Pending,
                &saved_at,
            );
            on_event(AgentEvent::Turn(TurnEvent::MessageFinished {
                message_id: assistant_result_entry_id.clone(),
                items: public_items,
            }));
            let has_tool_calls = !tool_calls.is_empty();
            if has_tool_calls {
                self.dispatch_tools(
                    tool_calls,
                    &assistant_result_entry_id,
                    length_truncated,
                    cancellation,
                    on_event,
                )
                .await?;
            }
            if length_truncated {
                return Err(AgentError::OutputTruncated);
            }
            if !has_tool_calls && lock_inbox(&self.inbox).pending().is_empty() {
                return Ok(());
            }
        }
    }

    pub(super) fn has_pending_inputs(&self) -> bool {
        !lock_inbox(&self.inbox).pending().is_empty()
    }

    async fn drain_inputs(
        &mut self,
        on_event: &mut (dyn FnMut(AgentEvent) + Send),
        cancellation: &CancellationToken,
    ) -> Result<()> {
        loop {
            if cancellation.is_cancelled() {
                return Err(AgentError::Aborted);
            }
            let next = lock_inbox(&self.inbox).next();
            let Some(request) = next else { break };
            self.append_user_input(&request, on_event).await?;
        }
        Ok(())
    }

    async fn append_user_input(
        &mut self,
        request: &QueuedInput,
        on_event: &mut (dyn FnMut(AgentEvent) + Send),
    ) -> Result<()> {
        let input = match &request.input {
            QueuedInputKind::User(input) => input,
            QueuedInputKind::Background { task_id } => {
                let tasks = Arc::clone(&self.bash_tasks);
                let id = task_id.clone();
                if let Some((text, end, terminal)) =
                    tokio::task::spawn_blocking(move || tasks.prepare_completion_message(&id))
                        .await
                        .expect("bash completion lookup completes while the runtime is running")
                        .map_err(std::io::Error::other)?
                {
                    let message = Message::Notification {
                        content: vec![model::ContentBlock::text(text)],
                    };
                    self.append_message(&request.message_id, message, None, &[]).await?;
                    self.bash_tasks.confirm_delivered(task_id, end, terminal);
                }
                lock_inbox(&self.inbox).confirm_saved(&request.message_id);
                return Ok(());
            }
        };
        let images: Vec<_> = input.images.iter().map(|image| image.attachment.clone()).collect();
        let model_text = input.model_text();
        let display_text = (model_text != input.text).then(|| input.text.clone());
        let display = (display_text.is_some() || !input.files.is_empty() || !input.annotations.is_empty())
            .then(|| MessageDisplay::User {
                display_text,
                files: input.files.clone(),
                annotations: input.annotations.clone(),
            });
        let mut content = vec![model::ContentBlock::text(model_text)];
        content.extend(images.iter().cloned().map(model::ContentBlock::Image));
        let message = Message::User { content };
        let saved_at = self.append_message(&request.message_id, message, display, &input.images).await?;
        lock_inbox(&self.inbox).confirm_saved(&request.message_id);
        on_event(AgentEvent::Turn(TurnEvent::ItemUpdated {
            item: protocol::HistoryItem::Message {
                id: request.item_id(),
                text: input.text.clone(),
                at: Some(saved_at),
                stats: None,
                images,
                files: input.files.clone(),
                annotations: input.annotations.clone(),
                role: protocol::HistoryMessageRole::User,
                status: protocol::TurnStatus::Completed,
            },
        }));
        Ok(())
    }

    async fn append_message(
        &self,
        id: &str,
        message: Message,
        display: Option<MessageDisplay>,
        images: &[crate::image::InputImage],
    ) -> Result<String> {
        let session = Arc::clone(&self.requests.session);
        let id = id.to_string();
        let images = images.to_vec();
        Ok(tokio::task::spawn_blocking(move || {
            if !images.is_empty() {
                let directory = with_writer(&session, |writer| writer.data().image_directory());
                for image in images {
                    image.save(&directory)?;
                }
            }
            with_writer(&session, |writer| writer.append_message_with_id(&id, message, None, display))
        })
        .await
        .expect("message writer completes while the runtime is running")?)
    }
}
