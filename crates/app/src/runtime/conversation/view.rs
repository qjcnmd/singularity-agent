//! 任务运行态视图、流式与待处理项状态及事件发布。
use super::activity::Activity;
use super::{Conversation, TurnLifecycle};
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
    pub(super) context: Option<protocol::InjectedContext>,
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
}
impl Conversation {
    /// 首次发送返回同一时刻的任务摘要、运行状态和事件序号。
    pub(crate) fn start_result(&self) -> protocol::SessionStartResult {
        let view = self.lock_view();
        let phase = self.phase();
        let writer = lock_writer(&self.writer);
        protocol::SessionStartResult {
            summary: crate::runtime::history::summarize_thread(writer.data()),
            status: protocol::SessionStatus { phase, failure: view.failure.clone() },
            revision: view.last_event_revision,
        }
    }

    /// 任务列表用的简要状态，只含运行阶段和未保存的错误。
    pub(crate) fn status(&self) -> protocol::SessionStatus {
        let view = self.lock_view();
        protocol::SessionStatus {
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
            selection: state.model.clone(),
            context_usage: crate::runtime::history::context_usage(
                lock_writer(&self.writer).data().entries(),
                &state.model,
                state.last_context_window,
            ),
            pending_question: match &state.turn {
                TurnLifecycle::Running(controls) => controls.questions.pending(),
                TurnLifecycle::Idle => None,
            },
            pending_messages: state
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
                    })
                })
                .collect(),
            failure: view.failure.clone(),
        }
    }
    /// 历史与活动内容在同一次会话读取内合并。
    pub(crate) fn read(&self) -> Result<SessionReadResult, crate::runtime::CatalogError> {
        let view = self.lock_view();
        let runtime = self.runtime(&view);
        let background_tasks = self.bash_tasks.list().into_iter().map(Into::into).collect();
        let writer = lock_writer(&self.writer);
        let mut history = crate::runtime::history::read_thread_history(writer.data()).map_err(|source| {
            crate::runtime::CatalogError::Session {
                path: writer.data().path().to_path_buf(),
                source,
            }
        })?;
        view.activity.merge(&mut history);
        Ok(SessionReadResult {
            revision: view.last_event_revision,
            history,
            runtime,
            background_tasks,
            context: view.context.clone(),
        })
    }
    pub(super) fn publish(&self, view: &mut ConversationView, event: StreamEvent) {
        view.last_event_revision = (self.publish)(event);
    }

    pub(super) fn publish_changed(&self, view: &mut ConversationView) {
        let payload = self.runtime(view);
        self.publish(
            view,
            StreamEvent::SessionChanged {
                session_id: self.thread_id.clone(),
                payload,
            },
        );
    }
    pub(super) fn summary(&self) -> protocol::ThreadSummary {
        let writer = lock_writer(&self.writer);
        crate::runtime::history::summarize_thread(writer.data())
    }
    pub(super) fn publish_summary(&self, view: &mut ConversationView) {
        self.publish(view, StreamEvent::SessionSummaryChanged { summary: self.summary() });
    }
    /// 接收 Agent 的共同事件，更新运行状态后发布。
    pub(super) fn on_event(&self, event: AgentEvent) {
        let mut view = self.lock_view();
        match event {
            AgentEvent::SessionStateChanged => self.publish_changed(&mut view),
            AgentEvent::ProviderAttempt { observation } => {
                crate::runtime::history::accumulate_request_usage(&mut view.usage, &observation);
                self.publish_changed(&mut view);
            }
            AgentEvent::Turn(event) => self.publish_event(&mut view, event),
        }
    }
    pub(super) fn publish_event(&self, view: &mut ConversationView, mut event: TurnEvent) {
        // 请求结束先发 ProviderAttempt 累计用量，再发完成的回复或回合结束；均带上此刻的会话累计。
        match &mut event {
            TurnEvent::MessageFinished { items, .. } => {
                agent::message::attach_usage(items, &view.usage);
            }
            TurnEvent::TurnCompleted { stats, .. } | TurnEvent::TurnFailed { stats, .. } => {
                *stats = Some(view.usage.clone());
            }
            _ => {}
        }
        match &event {
            TurnEvent::ContextLoaded { context } => {
                view.context = Some(context.clone());
            }
            TurnEvent::ItemUpdated {
                item:
                    protocol::HistoryItem::Message {
                        role: protocol::HistoryMessageRole::User, ..
                    },
            } => {
                self.publish_summary(view);
            }
            _ => {}
        }
        let phase = self.phase();
        if phase != SessionPhase::Idle {
            view.activity.apply(&event);
        }
        self.publish(
            view,
            StreamEvent::TurnEvent {
                session_id: self.thread_id.clone(),
                payload: Box::new(TurnEventEnvelope {
                    turn_id: view.activity.turn_id(),
                    event,
                    phase,
                }),
            },
        );
    }
}
