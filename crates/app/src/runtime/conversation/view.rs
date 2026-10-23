//! 任务运行态视图、流式与待处理项状态及事件发布。
use super::*;
use singularity_protocol::{
    SessionFailure, SessionReadResult, SessionRuntime, StreamEvent, TurnEventEnvelope,
};

pub(super) struct ConversationView {
    pub(super) last_event_revision: u64,
    pub(super) activity: super::activity::Activity,
    pub(super) failure: Option<SessionFailure>,
    usage: singularity_protocol::SessionModelUsage,
    context: Option<singularity_protocol::InjectedContext>,
}
impl ConversationView {
    pub(super) fn new(usage: singularity_protocol::SessionModelUsage) -> Self {
        Self {
            last_event_revision: 0,
            activity: Default::default(),
            failure: None,
            usage,
            context: None,
        }
    }
    pub(super) fn begin(&mut self) {
        self.activity = Default::default();
        self.failure = None;
    }
}
impl Conversation {
    /// 首次发送返回同一时刻的任务摘要、运行状态和事件序号。
    pub fn start_result(&self) -> singularity_protocol::SessionStartResult {
        let view = self.lock_view();
        let phase = self.phase();
        let writer = lock_writer(&self.writer);
        singularity_protocol::SessionStartResult {
            summary: crate::runtime::history::summarize_thread(&writer),
            status: singularity_protocol::SessionStatus { phase, failure: view.failure.clone() },
            revision: view.last_event_revision,
        }
    }

    /// 任务列表用的简要状态，只含运行阶段和未保存的错误。
    pub fn status(&self) -> singularity_protocol::SessionStatus {
        let view = self.lock_view();
        singularity_protocol::SessionStatus {
            phase: self.phase(),
            failure: view.failure.clone(),
        }
    }

    pub(super) fn lock_view(&self) -> std::sync::MutexGuard<'_, ConversationView> {
        self.view.lock().expect("conversation view lock poisoned")
    }
    pub(super) fn runtime(&self, view: &ConversationView) -> SessionRuntime {
        let state = self.lock_state();
        SessionRuntime {
            phase: state.turn.phase(),
            selection: state.thread.model.clone(),
            context_usage: crate::runtime::history::context_usage(
                lock_writer(&self.writer).entries(),
                &state.thread.model,
                state.last_context_window,
            ),
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
            failure: view.failure.clone(),
            usage: view.usage.clone(),
        }
    }
    /// 历史与活动内容在同一次会话读取内合并。
    pub fn read(&self) -> Result<SessionReadResult, crate::runtime::CatalogError> {
        let view = self.lock_view();
        let mut runtime = self.runtime(&view);
        let background_tasks = self.background_tasks();
        let writer = lock_writer(&self.writer);
        let mut history = crate::runtime::thread_catalog::read_thread_history(&writer)?;
        runtime.usage = crate::runtime::history::session_usage(writer.entries());
        view.activity.merge(&mut history);
        Ok(SessionReadResult {
            revision: view.last_event_revision,
            history,
            runtime,
            background_tasks,
            context: view.context.clone(),
        })
    }
    pub(super) fn publish_changed(&self, view: &mut ConversationView) {
        view.last_event_revision = (self.publish)(StreamEvent::SessionChanged {
            session_id: self.thread().thread_id,
            payload: self.runtime(view),
        });
    }
    pub(super) fn summary(&self) -> singularity_protocol::ThreadSummary {
        let writer = lock_writer(&self.writer);
        crate::runtime::history::summarize_thread(&writer)
    }
    pub(super) fn publish_summary(&self, view: &mut ConversationView) {
        view.last_event_revision =
            (self.publish)(StreamEvent::SessionSummaryChanged { summary: self.summary() });
    }
    /// 接收 Agent 的共同事件，更新运行状态后发布。
    pub(super) fn on_event(&self, event: TurnEvent) {
        self.publish_event(&mut self.lock_view(), event);
    }
    pub(super) fn publish_event(&self, view: &mut ConversationView, event: TurnEvent) {
        match &event {
            TurnEvent::SessionStateChanged {} => {
                self.publish_changed(view);
                return;
            }
            TurnEvent::ProviderAttempt { observation } => {
                crate::runtime::history::accumulate_request_usage(&mut view.usage, observation);
                self.publish_changed(view);
                return;
            }
            TurnEvent::ContextLoaded { context } => {
                view.context = Some(context.clone());
            }
            TurnEvent::ItemUpdated {
                item:
                    singularity_protocol::HistoryItem::Message {
                        role: singularity_protocol::HistoryMessageRole::User,
                        ..
                    },
            } => {
                self.publish_summary(view);
            }
            _ => {}
        }
        if self.phase() != SessionPhase::Idle {
            view.activity.apply(&event);
        }
        view.last_event_revision = (self.publish)(StreamEvent::TurnEvent {
            session_id: self.thread().thread_id,
            payload: Box::new(TurnEventEnvelope {
                turn_id: view.activity.turn_id(),
                event,
                phase: self.phase(),
            }),
        });
    }
}
