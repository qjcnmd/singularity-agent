use std::path::PathBuf;
use std::sync::Arc;

use super::activity::Activity;
use super::state::{ConversationState, TurnControls, TurnLifecycle};
use super::{Conversation, ConversationError, Operation, view};
use agent::agent::{QueuedInput, SteeringInbox, UserInput};
use agent::session::{LedgerRecord, SessionEntry, lock_writer};
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
        if let Some(preview) = &state.context_preview {
            preview.cancel();
        }
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
        if state.closing || matches!(state.turn, TurnLifecycle::Rewinding) {
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
            let (phase, rewinding, has_preview) = {
                let state = self.lock_state();
                (
                    state.turn.phase(),
                    matches!(state.turn, TurnLifecycle::Rewinding),
                    state.context_preview.is_some(),
                )
            };
            match phase {
                SessionPhase::Idle if !rewinding && !has_preview => break,
                SessionPhase::Idle | SessionPhase::Stopping => {}
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
    pub(crate) fn begin_delete(self: &Arc<Self>) -> Result<DeleteGuard, ConversationError> {
        let _view = self.lock_view();
        let mut state = self.lock_state();
        if state.closing || matches!(state.turn, TurnLifecycle::Rewinding) {
            return Err(ConversationError::Busy);
        }
        state.closing = true;
        Ok(DeleteGuard { conversation: Some(Arc::clone(self)) })
    }
    pub(crate) fn is_deleting(&self) -> bool {
        self.lock_state().closing
    }

    pub(crate) fn begin_rewind(
        self: &Arc<Self>,
        turn_id: &str,
        restore_files: bool,
    ) -> Result<(RewindGuard, RewindPlan), ConversationError> {
        let _view = self.lock_view();
        let mut state = self.lock_state();
        if state.closing || matches!(state.turn, TurnLifecycle::Rewinding) {
            return Err(ConversationError::Busy);
        }
        if state.turn.is_busy() {
            return Err(ConversationError::Message("当前任务正在运行，请先停止后再回溯。".into()));
        }
        if self.bash_tasks.is_busy() {
            return Err(ConversationError::Message(
                "当前任务仍有运行中的后台命令，请先停止后再回溯。".into(),
            ));
        }
        let has_queued_user = state
            .steering_inbox
            .lock()
            .expect("turn inbox lock poisoned")
            .pending()
            .iter()
            .any(|entry| entry.user_input().is_some());
        if has_queued_user {
            return Err(ConversationError::Message("当前任务还有待处理的排队输入，暂无法回溯。".into()));
        }
        let (snapshot_dir, cwd, target_tree) = {
            let writer = lock_writer(&self.writer);
            let data = writer.data();
            let target_tree = data
                .entries()
                .iter()
                .find_map(|entry| match entry {
                    SessionEntry::Record {
                        id,
                        record: LedgerRecord::OperationStarted { snapshot_tree },
                        ..
                    } if id == turn_id => Some(snapshot_tree.clone()),
                    _ => None,
                })
                .ok_or_else(|| ConversationError::Message(format!("未找到可回溯的回合：{turn_id}")))?;
            (data.snapshot_directory(), data.cwd().to_path_buf(), target_tree)
        };
        let restore_tree = if restore_files {
            Some(target_tree.ok_or_else(|| {
                ConversationError::Message("该回合没有可用的文件快照，无法还原文件。".into())
            })?)
        } else {
            None
        };
        if let Some(preview) = &state.context_preview {
            preview.cancel();
        }
        state.turn = TurnLifecycle::Rewinding;
        Ok((
            RewindGuard { conversation: Some(Arc::clone(self)) },
            RewindPlan { snapshot_dir, cwd, restore_tree },
        ))
    }

    pub(crate) fn commit_rewind(
        &self,
        turn_id: &str,
        guard: RewindGuard,
    ) -> Result<protocol::SessionReadResult, ConversationError> {
        if self.is_deleting() {
            return Err(ConversationError::Busy);
        }
        lock_writer(&self.writer).rewind_before_turn(turn_id)?;
        let result = {
            let mut view = self.lock_view();
            {
                let mut state = self.lock_state();
                let usage =
                    crate::runtime::history::session_usage(lock_writer(&self.writer).data().entries());
                view.reset(usage);
                *state.steering_inbox.lock().expect("turn inbox lock poisoned") = SteeringInbox::default();
                state.auto_resume = false;
                state.turn = TurnLifecycle::Idle;
            }
            let payload = self.runtime(&view);
            self.publish(
                &mut view,
                StreamEvent::SessionSettled {
                    session_id: self.thread_id.clone(),
                    payload,
                    summary: self.summary(),
                },
            );
            self.read_locked(&view).map_err(|error| ConversationError::Message(error.to_string()))?
        };
        guard.finish();
        Ok(result)
    }
}

pub(crate) struct RewindPlan {
    pub(crate) snapshot_dir: PathBuf,
    pub(crate) cwd: PathBuf,
    pub(crate) restore_tree: Option<String>,
}

pub(crate) struct RewindGuard {
    conversation: Option<Arc<Conversation>>,
}

impl RewindGuard {
    fn finish(mut self) {
        if let Some(conversation) = self.conversation.take() {
            conversation.changed.notify_waiters();
        }
    }
}

impl Drop for RewindGuard {
    fn drop(&mut self) {
        if let Some(conversation) = &self.conversation {
            {
                let _view = conversation.lock_view();
                let mut state = conversation.lock_state();
                if matches!(state.turn, TurnLifecycle::Rewinding) {
                    state.turn = TurnLifecycle::Idle;
                }
            }
            conversation.changed.notify_waiters();
        }
    }
}
pub(crate) struct DeleteGuard {
    conversation: Option<Arc<Conversation>>,
}
impl DeleteGuard {
    pub(crate) fn complete(mut self) {
        self.conversation.take();
    }
}
impl Drop for DeleteGuard {
    fn drop(&mut self) {
        if let Some(conversation) = &self.conversation {
            conversation.clear_closing();
        }
    }
}
