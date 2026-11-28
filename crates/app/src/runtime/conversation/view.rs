use super::activity::Activity;
use super::{Conversation, ConversationInner, TurnLifecycle};
use agent::agent::AgentEvent;
use agent::session::lock_writer;
use protocol::{
    SessionFailure, SessionPhase, SessionReadResult, SessionRuntime, StreamEvent, TurnEvent,
    TurnEventEnvelope,
};

pub(super) struct ConversationView {
    pub(super) last_event_revision: u64,
    pub(super) activity: Activity,
    pub(super) failure: Option<SessionFailure>,
    usage: protocol::SessionModelUsage,
    pub(super) context: Option<Vec<protocol::ContextModule>>,
}
impl ConversationView {
    pub(super) fn new(usage: protocol::SessionModelUsage, initial_revision: u64) -> Self {
        Self {
            last_event_revision: initial_revision,
            activity: Activity::default(),
            failure: None,
            usage,
            context: None,
        }
    }
    pub(super) fn begin(&mut self, turn_id: Option<String>) {
        self.activity = Activity::with_turn_id(turn_id);
        self.failure = None;
    }
    pub(super) fn reset(&mut self, usage: protocol::SessionModelUsage) {
        self.activity = Activity::default();
        self.failure = None;
        self.usage = usage;
        self.context = None;
    }
}
impl Conversation {
    pub(crate) fn start_result(&self) -> protocol::SessionStartResult {
        let inner = self.lock_inner();
        let phase = inner.state.phase();
        let writer = lock_writer(&self.writer);
        protocol::SessionStartResult {
            summary: crate::runtime::history::summarize_thread(writer.data()),
            status: protocol::SessionStatus {
                phase,
                failure: inner.view.failure.clone(),
            },
            revision: inner.view.last_event_revision,
        }
    }

    pub(crate) fn status(&self) -> protocol::SessionStatus {
        let inner = self.lock_inner();
        protocol::SessionStatus {
            phase: inner.state.phase(),
            failure: inner.view.failure.clone(),
        }
    }

    pub(super) fn runtime(&self, inner: &ConversationInner) -> SessionRuntime {
        let context_usage = crate::runtime::history::context_usage(
            lock_writer(&self.writer).data().entries(),
            &inner.state.model,
            inner.state.last_context_window,
        );
        self.runtime_with_context_usage(inner, context_usage)
    }

    pub(super) fn runtime_with_context_usage(
        &self,
        inner: &ConversationInner,
        context_usage: Option<protocol::ContextUsage>,
    ) -> SessionRuntime {
        SessionRuntime {
            phase: inner.state.phase(),
            selection: inner.state.model.clone(),
            context_usage,
            model_usage: inner.view.usage.has_metrics().then(|| inner.view.usage.clone()),
            pending_question: match &inner.state.turn {
                TurnLifecycle::Running(_) => inner.state.questions.pending(),
                TurnLifecycle::Idle => None,
            },
            pending_messages: inner
                .state
                .steering_inbox
                .lock()
                .expect("turn inbox lock poisoned")
                .pending()
                .iter()
                .filter_map(|request| {
                    request.user_input().map(|input| protocol::PendingUserMessage {
                        item_id: request.item_id(),
                        text: input.text.clone(),
                        images: input.images.iter().map(|image| image.attachment.clone()).collect(),
                        files: input.files.clone(),
                        annotations: input.annotations.clone(),
                    })
                })
                .collect(),
            failure: inner.view.failure.clone(),
        }
    }

    pub(crate) fn read(&self) -> Result<SessionReadResult, crate::runtime::CatalogError> {
        let inner = self.lock_inner();
        let (context_usage, history) = {
            let writer = lock_writer(&self.writer);
            let data = writer.data();
            let context_usage = crate::runtime::history::context_usage(
                data.entries(),
                &inner.state.model,
                inner.state.last_context_window,
            );
            let history = crate::runtime::history::read_thread_history(data).map_err(|source| {
                crate::runtime::CatalogError::Session { path: data.path().to_path_buf(), source }
            })?;
            (context_usage, history)
        };
        Ok(self.finish_read(&inner, context_usage, history))
    }

    pub(super) fn finish_read(
        &self,
        inner: &ConversationInner,
        context_usage: Option<protocol::ContextUsage>,
        mut history: protocol::ThreadHistory,
    ) -> SessionReadResult {
        let background_tasks = self.bash_tasks.list().into_iter().map(Into::into).collect();
        let runtime = self.runtime_with_context_usage(inner, context_usage);
        inner.view.activity.merge(&mut history);
        SessionReadResult {
            revision: inner.view.last_event_revision,
            history,
            runtime,
            background_tasks,
            context: inner.view.context.clone(),
        }
    }

    pub(super) fn publish(&self, inner: &mut ConversationInner, event: StreamEvent) {
        inner.view.last_event_revision = self.hub.emit(event);
    }

    pub(super) fn publish_changed(&self, inner: &mut ConversationInner) {
        let payload = self.runtime(inner);
        self.publish(
            inner,
            StreamEvent::SessionChanged {
                session_id: self.thread_id.clone(),
                payload,
            },
        );
    }

    pub(super) fn publish_summary(&self, inner: &mut ConversationInner) {
        let summary = crate::runtime::history::summarize_thread(lock_writer(&self.writer).data());
        self.publish(inner, StreamEvent::SessionSummaryChanged { summary });
    }

    pub(super) fn on_event(&self, event: AgentEvent) {
        let mut inner = self.lock_inner();
        match event {
            AgentEvent::SessionStateChanged => self.publish_changed(&mut inner),
            AgentEvent::ProviderAttempt { observation } => {
                crate::runtime::history::accumulate_request_usage(&mut inner.view.usage, &observation);
                self.publish_changed(&mut inner);
            }
            AgentEvent::Turn(event) => self.publish_event(&mut inner, event),
        }
    }

    pub(super) fn publish_event(&self, inner: &mut ConversationInner, mut event: TurnEvent) {
        match &mut event {
            TurnEvent::MessageFinished { items, .. } => {
                agent::message::attach_usage(items, &inner.view.usage);
            }
            TurnEvent::TurnCompleted { stats, .. } | TurnEvent::TurnFailed { stats, .. } => {
                *stats = Some(inner.view.usage.clone());
            }
            _ => {}
        }
        match &event {
            TurnEvent::ContextLoaded { context } => {
                inner.view.context = Some(context.clone());
            }
            TurnEvent::ItemUpdated {
                item:
                    protocol::HistoryItem::Message {
                        role: protocol::HistoryMessageRole::User, ..
                    },
            } => {
                self.publish_summary(inner);
            }
            _ => {}
        }
        let phase = inner.state.phase();
        if phase != SessionPhase::Idle {
            inner.view.activity.apply(&event);
        }
        self.publish(
            inner,
            StreamEvent::TurnEvent {
                session_id: self.thread_id.clone(),
                payload: Box::new(TurnEventEnvelope {
                    turn_id: inner.view.activity.turn_id(),
                    event,
                    phase,
                }),
            },
        );
    }
}
