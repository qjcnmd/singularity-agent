use std::sync::Arc;

use super::activity::Activity;
use super::state::{ConversationState, TurnControls, TurnLifecycle};
use super::{Conversation, ConversationError, Operation, view};
use agent::agent::{QueuedInput, SteeringInbox, UserInput};
use agent::session::lock_writer;
use protocol::{SessionFailure, SessionOperation, SessionPhase, StreamEvent, TurnStatus};

impl Conversation {
    fn begin_operation(
        &self,
        view: &mut view::ConversationView,
        mut state: std::sync::MutexGuard<'_, ConversationState>,
        source: SessionOperation,
    ) -> Operation {
        let controls = Arc::new(TurnControls::new(uuid::Uuid::new_v4().to_string()));
        controls.set_compacting(source == SessionOperation::Compaction);
        state.turn = TurnLifecycle::Running(Arc::clone(&controls));
        drop(state);
        view.begin((source == SessionOperation::Turn).then(|| controls.turn_id.clone()));
        self.publish_changed(view);
        Operation { source, controls }
    }

    async fn execute(self: &Arc<Self>, operation: Operation) {
        let mut emit = |event| self.on_event(event);
        let result = match operation.source {
            SessionOperation::Turn => self.run_single_turn(&operation.controls, &mut emit).await,
            SessionOperation::Compaction => self.compact_thread(&operation.controls, &mut emit).await,
        };
        self.settle(operation.source, result);
    }
    fn settle(&self, source: SessionOperation, result: Result<TurnStatus, protocol::TurnErrorDetail>) {
        let failed = matches!(&result, Err(_) | Ok(TurnStatus::Failed));
        let failure = result.err().map(|error| SessionFailure { source, error });
        let mut view = self.lock_view();
        if failed {
            self.pause_background_resume();
        }
        if failure.is_some() {
            view.failure = failure;
        }
        view.activity = Activity::default();
        self.lock_state().turn = TurnLifecycle::Idle;
        let payload = self.runtime(&view);
        self.publish(
            &mut view,
            StreamEvent::SessionSettled {
                session_id: self.thread_id.clone(),
                payload,
                summary: self.summary(),
            },
        );
        self.changed.notify_waiters();
    }

    pub(crate) fn start(self: &Arc<Self>, input: UserInput) -> Result<(), ConversationError> {
        let mut view = self.lock_view();
        let mut state = self.lock_state();
        if state.closing {
            return Err(ConversationError::Busy);
        }
        let start = match state.turn.phase() {
            SessionPhase::Idle => true,
            SessionPhase::Running | SessionPhase::Compacting => false,
            SessionPhase::Stopping => return Err(ConversationError::Busy),
        };
        let mut inbox = state.steering_inbox.lock().expect("turn inbox lock poisoned");
        inbox.enqueue(QueuedInput::new(input));
        drop(inbox);
        state.auto_resume = true;
        if !start {
            drop(state);
            self.publish_changed(&mut view);
            return Ok(());
        }
        let operation = self.begin_operation(&mut view, state, SessionOperation::Turn);
        drop(view);
        self.spawn_operation(operation);
        Ok(())
    }

    pub(crate) fn compact(self: &Arc<Self>) -> Result<(), ConversationError> {
        let mut view = self.lock_view();
        let state = self.lock_state();
        if state.closing || state.turn.is_busy() {
            return Err(ConversationError::Busy);
        }
        let operation = self.begin_operation(&mut view, state, SessionOperation::Compaction);
        drop(view);
        self.spawn_operation(operation);
        Ok(())
    }
    pub(super) fn spawn_operation(self: &Arc<Self>, mut operation: Operation) {
        let conversation = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                conversation.execute(operation).await;
                let Some(next) = conversation.pending_operation() else {
                    break;
                };
                operation = next;
            }
        });
    }
    pub(super) fn pending_operation(self: &Arc<Self>) -> Option<Operation> {
        let mut view = self.lock_view();
        let state = self.lock_state();
        if state.turn.is_busy() || !state.has_pending_resume() {
            return None;
        }
        Some(self.begin_operation(&mut view, state, SessionOperation::Turn))
    }
    pub(crate) async fn stop(&self) -> Result<(), ConversationError> {
        self.mark_closing();
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            match self.phase() {
                SessionPhase::Idle => break,
                SessionPhase::Stopping => {}
                SessionPhase::Running | SessionPhase::Compacting => {
                    let _ = self.abort();
                }
            }
            changed.await;
        }
        self.bash_tasks
            .shutdown()
            .await
            .inspect_err(|message| self.background_error(message.clone()))
            .map_err(ConversationError::Message)
    }

    pub(crate) fn rewind(
        self: &Arc<Self>,
        message_id: &str,
        input: UserInput,
    ) -> Result<Box<protocol::SessionReadResult>, ConversationError> {
        let mut view = self.lock_view();
        let (result, operation) = {
            let mut state = self.lock_state();
            if state.closing {
                return Err(ConversationError::Busy);
            }
            if state.turn.is_busy() {
                return Err(ConversationError::Message("当前任务正在运行，请先停止后再编辑。".into()));
            }
            if self.bash_tasks.is_busy() {
                return Err(ConversationError::Message(
                    "当前任务仍有运行中的后台命令，请先停止后再编辑。".into(),
                ));
            }
            if state.has_active_inbox() {
                return Err(ConversationError::Message("当前任务还有待处理的输入，暂无法编辑。".into()));
            }
            self.models
                .validate_selection(&state.model)
                .map_err(|error| ConversationError::Message(error.to_string()))?;
            let mut writer = lock_writer(&self.writer);
            writer.rewind_before_message(message_id)?;
            let usage = crate::runtime::history::session_usage(writer.data().entries());
            drop(writer);
            view.reset(usage);
            let mut inbox = SteeringInbox::default();
            inbox.enqueue(QueuedInput::new(input));
            *state.steering_inbox.lock().expect("turn inbox lock poisoned") = inbox;
            state.auto_resume = true;
            let operation = self.begin_operation(&mut view, state, SessionOperation::Turn);
            let result = self
                .read_locked(&view)
                .map(Box::new)
                .map_err(|error| ConversationError::Message(error.to_string()));
            (result, operation)
        };
        drop(view);
        self.changed.notify_waiters();
        self.spawn_operation(operation);
        result
    }
}
