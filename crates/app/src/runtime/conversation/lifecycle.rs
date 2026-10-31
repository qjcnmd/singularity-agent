//! 用户输入、独立压缩和后台续跑共用的启动、收尾与关闭流程。
use super::*;
use singularity_protocol::{SessionFailure, SessionOperation, StreamEvent, TurnStatus};

impl Conversation {
    /// 首条和后续消息都放进输入队列；空闲时占用任务并返回要启动的回合，
    /// 运行中只排队，等当前回合消费或结束后接着处理。
    fn submit(self: &Arc<Self>, input: UserInput) -> Result<Option<Operation>, ConversationError> {
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
            return Ok(None);
        }
        Ok(Some(self.begin_operation(&mut view, state, SessionOperation::Turn)))
    }

    /// 空闲时占用任务执行独立压缩；压缩标记让运行阶段显示为“压缩中”。
    fn reserve_compaction(self: &Arc<Self>) -> Result<Operation, ConversationError> {
        let mut view = self.lock_view();
        let state = self.lock_state();
        if state.closing || state.turn.is_busy() {
            return Err(ConversationError::Busy);
        }
        Ok(self.begin_operation(&mut view, state, SessionOperation::Compaction))
    }

    // 三个入口都已在状态锁内检查允许启动；占用任务、清理上次显示状态和发布由这里一起完成。
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
        view.begin();
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
    fn settle(
        &self,
        source: SessionOperation,
        result: Result<TurnStatus, singularity_protocol::TurnErrorDetail>,
    ) {
        let failed = matches!(&result, Err(_) | Ok(TurnStatus::Failed));
        let failure = result.err().map(|error| SessionFailure { source, error });
        let mut view = self.lock_view();
        if failed {
            self.pause_background_resume();
        }
        view.failure = failure;
        view.activity = Default::default();
        self.lock_state().turn = TurnLifecycle::Idle;
        view.last_event_revision = (self.publish)(StreamEvent::SessionSettled {
            session_id: self.thread_id.clone(),
            payload: self.runtime(&view),
            summary: self.summary(),
        });
        self.changed.notify_waiters();
    }
    /// 接受用户输入并启动所需执行；后续回合由会话接续。
    pub fn start(self: &Arc<Self>, input: UserInput) -> Result<(), ConversationError> {
        if let Some(operation) = self.submit(input)? {
            self.spawn_operation(operation);
        }
        Ok(())
    }
    /// 开始独立压缩。
    pub fn compact(self: &Arc<Self>) -> Result<(), ConversationError> {
        self.spawn_operation(self.reserve_compaction()?);
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
    pub async fn stop(&self) -> Result<(), String> {
        self.set_closing(true);
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            match self.phase() {
                SessionPhase::Idle if self.lock_state().context_preview.is_none() => break,
                SessionPhase::Idle => {}
                SessionPhase::Running | SessionPhase::Compacting => {
                    let _ = self.abort();
                }
                SessionPhase::Stopping => {}
            }
            changed.await;
        }
        self.bash_tasks
            .shutdown()
            .await
            .inspect_err(|message| self.background_error(message.clone()))
    }
    /// 删除期间拒绝新操作；失败时由 guard 重新开放。
    pub fn begin_delete(self: &Arc<Self>) -> Result<DeleteGuard, ConversationError> {
        let _view = self.lock_view();
        let mut state = self.lock_state();
        if state.closing {
            return Err(ConversationError::Busy);
        }
        state.closing = true;
        Ok(DeleteGuard { conversation: Some(Arc::clone(self)) })
    }
    /// 是否已关闭新操作入口。
    pub fn is_deleting(&self) -> bool {
        self.lock_state().closing
    }
}
/// 删除未完成时恢复任务的操作入口。
pub struct DeleteGuard {
    conversation: Option<Arc<Conversation>>,
}
impl DeleteGuard {
    /// 删除成功后保持关闭，异步收尾持有的旧对象不能再启动执行。
    pub fn complete(mut self) {
        self.conversation.take();
    }
}
impl Drop for DeleteGuard {
    fn drop(&mut self) {
        if let Some(conversation) = &self.conversation {
            conversation.set_closing(false);
        }
    }
}
