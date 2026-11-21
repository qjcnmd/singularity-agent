//! 用户输入、独立轮换和后台续跑共用的启动、收尾与关闭流程。
use std::sync::Arc;

use super::activity::Activity;
use super::state::{ConversationState, TurnControls, TurnLifecycle};
use super::{Conversation, ConversationError, Operation, view};
use agent::agent::{QueuedInput, UserInput};
use protocol::{SessionFailure, SessionOperation, SessionPhase, StreamEvent, TurnStatus};

impl Conversation {
    // 三个入口都已在状态锁内检查允许启动；占用任务、清理上次显示状态和发布由这里一起完成。
    fn begin_operation(
        &self,
        view: &mut view::ConversationView,
        mut state: std::sync::MutexGuard<'_, ConversationState>,
        source: SessionOperation,
    ) -> Operation {
        let controls = Arc::new(TurnControls::new(uuid::Uuid::new_v4().to_string()));
        controls.set_rotating(source == SessionOperation::ContextRotation);
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
            SessionOperation::ContextRotation => {
                self.rotate_context_thread(&operation.controls, &mut emit).await
            }
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

    /// 接受用户输入并启动所需执行；后续回合由会话接续。
    pub(crate) fn start(self: &Arc<Self>, input: UserInput) -> Result<(), ConversationError> {
        let mut view = self.lock_view();
        let mut state = self.lock_state();
        if state.closing {
            return Err(ConversationError::Busy);
        }
        let start = match state.turn.phase() {
            SessionPhase::Idle => true,
            SessionPhase::Running | SessionPhase::Rotating => false,
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

    /// 空闲时占用任务执行独立轮换；轮换标记让运行阶段显示为“轮换中”。
    pub(crate) fn rotate_context(self: &Arc<Self>) -> Result<(), ConversationError> {
        let mut view = self.lock_view();
        let state = self.lock_state();
        if state.closing || state.turn.is_busy() {
            return Err(ConversationError::Busy);
        }
        let operation = self.begin_operation(&mut view, state, SessionOperation::ContextRotation);
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
    /// 等待前台结束，再回收所有后台进程。
    pub(crate) async fn stop(&self) -> Result<(), ConversationError> {
        self.mark_closing();
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let (phase, has_preview) = {
                let state = self.lock_state();
                (state.turn.phase(), state.context_preview.is_some())
            };
            match phase {
                SessionPhase::Idle if !has_preview => break,
                SessionPhase::Idle | SessionPhase::Stopping => {}
                SessionPhase::Running | SessionPhase::Rotating => {
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
    /// 删除期间拒绝新操作；失败时由 guard 重新开放。
    pub(crate) fn begin_delete(self: &Arc<Self>) -> Result<DeleteGuard, ConversationError> {
        let _view = self.lock_view();
        let mut state = self.lock_state();
        if state.closing {
            return Err(ConversationError::Busy);
        }
        state.closing = true;
        Ok(DeleteGuard { conversation: Some(Arc::clone(self)) })
    }
    pub(crate) fn is_deleting(&self) -> bool {
        self.lock_state().closing
    }
}
/// 删除未完成时恢复任务的操作入口。
pub(crate) struct DeleteGuard {
    conversation: Option<Arc<Conversation>>,
}
impl DeleteGuard {
    /// 删除成功后保持关闭，异步收尾持有的旧对象不能再启动执行。
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
