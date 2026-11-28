use std::sync::Arc;

use super::activity::Activity;
use super::state::{TurnControls, TurnLifecycle};
use super::{Conversation, ConversationError, ConversationInner, Operation};
use agent::agent::{QueuedInput, SteeringInbox, UserInput};
use agent::session::lock_writer;
use protocol::{SessionFailure, SessionOperation, SessionPhase, StreamEvent, TurnStatus};

impl Conversation {
    fn begin_operation(&self, inner: &mut ConversationInner, source: SessionOperation) -> Operation {
        let controls = Arc::new(TurnControls::new(uuid::Uuid::new_v4().to_string()));
        controls.set_compacting(source == SessionOperation::Compaction);
        inner.state.turn = TurnLifecycle::Running(Arc::clone(&controls));
        inner.view.begin((source == SessionOperation::Turn).then(|| controls.turn_id.clone()));
        self.publish_changed(inner);
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
        let mut inner = self.lock_inner();
        if failed {
            inner.state.pause_background_resume();
        }
        if failure.is_some() {
            inner.view.failure = failure;
        }
        inner.view.activity = Activity::default();
        inner.state.turn = TurnLifecycle::Idle;
        let (context_usage, summary) = {
            let writer = lock_writer(&self.writer);
            let data = writer.data();
            (
                crate::runtime::history::context_usage(
                    data.entries(),
                    &inner.state.model,
                    inner.state.last_context_window,
                ),
                crate::runtime::history::summarize_thread(data),
            )
        };
        let payload = self.runtime_with_context_usage(&inner, context_usage);
        self.publish(
            &mut inner,
            StreamEvent::SessionSettled {
                session_id: self.thread_id.clone(),
                payload,
                summary,
            },
        );
        self.changed.notify_waiters();
    }

    pub(crate) fn start(self: &Arc<Self>, input: UserInput) -> Result<(), ConversationError> {
        let mut inner = self.lock_inner();
        if inner.state.closing {
            return Err(ConversationError::Busy);
        }
        let start = match inner.state.turn.phase() {
            SessionPhase::Idle => true,
            SessionPhase::Running | SessionPhase::Compacting => false,
            SessionPhase::Stopping => return Err(ConversationError::Busy),
        };
        inner
            .state
            .steering_inbox
            .lock()
            .expect("turn inbox lock poisoned")
            .enqueue(QueuedInput::new(input));
        inner.state.auto_resume = true;
        if !start {
            self.publish_changed(&mut inner);
            return Ok(());
        }
        let operation = self.begin_operation(&mut inner, SessionOperation::Turn);
        drop(inner);
        self.spawn_operation(operation);
        Ok(())
    }

    pub(crate) fn compact(self: &Arc<Self>) -> Result<(), ConversationError> {
        let mut inner = self.lock_inner();
        if inner.state.closing || inner.state.turn.is_busy() {
            return Err(ConversationError::Busy);
        }
        let operation = self.begin_operation(&mut inner, SessionOperation::Compaction);
        drop(inner);
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

    pub(super) fn begin_pending_operation(&self, inner: &mut ConversationInner) -> Option<Operation> {
        if inner.state.turn.is_busy() || !inner.state.has_pending_resume() {
            return None;
        }
        Some(self.begin_operation(inner, SessionOperation::Turn))
    }

    fn pending_operation(&self) -> Option<Operation> {
        let mut inner = self.lock_inner();
        self.begin_pending_operation(&mut inner)
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
        let mut inner = self.lock_inner();
        if inner.state.closing {
            return Err(ConversationError::Busy);
        }
        if inner.state.turn.is_busy() {
            return Err(ConversationError::Message("当前任务正在运行，请先停止后再编辑。".into()));
        }
        if self.bash_tasks.is_busy() {
            return Err(ConversationError::Message(
                "当前任务仍有运行中的后台命令，请先停止后再编辑。".into(),
            ));
        }
        if inner.state.has_active_inbox() {
            return Err(ConversationError::Message("当前任务还有待处理的输入，暂无法编辑。".into()));
        }
        self.models
            .validate_selection(&inner.state.model)
            .map_err(|error| ConversationError::Message(error.to_string()))?;
        let (usage, context_usage, history) = {
            let mut writer = lock_writer(&self.writer);
            writer.rewind_before_message(message_id)?;
            let data = writer.data();
            let usage = crate::runtime::history::session_usage(data.entries());
            let context_usage = crate::runtime::history::context_usage(
                data.entries(),
                &inner.state.model,
                inner.state.last_context_window,
            );
            let history = crate::runtime::history::read_thread_history(data).map_err(|source| {
                crate::runtime::CatalogError::Session { path: data.path().to_path_buf(), source }
            });
            (usage, context_usage, history)
        };
        inner.view.reset(usage);
        let history = match history {
            Ok(history) => history,
            Err(error) => {
                self.publish_changed(&mut inner);
                drop(inner);
                self.changed.notify_waiters();
                return Err(ConversationError::Message(error.to_string()));
            }
        };
        let mut inbox = SteeringInbox::default();
        inbox.enqueue(QueuedInput::new(input));
        *inner.state.steering_inbox.lock().expect("turn inbox lock poisoned") = inbox;
        inner.state.auto_resume = true;
        let operation = self.begin_operation(&mut inner, SessionOperation::Turn);
        let result = Box::new(self.finish_read(&inner, context_usage, history));
        drop(inner);
        self.changed.notify_waiters();
        self.spawn_operation(operation);
        Ok(result)
    }
}
