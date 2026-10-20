use super::*;
use singularity_protocol::BackgroundTaskUpdate;
use singularity_runtime::BashTaskUpdate;

impl AppServer {
    pub(super) fn listen_background(
        &self,
        session_id: String,
        slot: Arc<ConversationSlot>,
        mut updates: tokio::sync::mpsc::Receiver<BashTaskUpdate>,
    ) {
        let weak = self.weak.clone();
        let slot = Arc::downgrade(&slot);
        tokio::spawn(async move {
            while let Some(update) = updates.recv().await {
                let Some(app) = weak.upgrade() else { break };
                let Some(slot) = slot.upgrade() else { break };
                match update {
                    BashTaskUpdate::Updated(task) => {
                        let wake = !task.state.is_running();
                        {
                            let mut state = slot.lock_state();
                            state.bump_revision();
                            app.emit(StreamEvent::BackgroundTaskUpdated {
                                session_id: session_id.clone(),
                                payload: Box::new(BackgroundTaskUpdate {
                                    session_revision: state.revision(),
                                    phase: slot.conversation().phase(),
                                    task,
                                }),
                            });
                        }
                        if wake {
                            app.start_background_turn(&session_id, &slot);
                        }
                    }
                    BashTaskUpdate::Failed(message) => {
                        slot.conversation().pause_background_resume();
                        app.background_shutdown_error(&slot, message);
                    }
                }
            }
        });
    }

    /// 与用户提交、归档和操作结算共用锁序；队列内容才决定是否需要续跑。
    pub(super) fn start_background_turn(self: &Arc<Self>, id: &str, slot: &Arc<ConversationSlot>) {
        let _lifecycle = self.lock_lifecycle();
        if slot.is_archiving() || !slot.conversation().has_background_resume() {
            return;
        }
        let mut state = slot.lock_state();
        if let Some(reservation) = slot.conversation().reserve_background_turn() {
            state.begin_turn(slot.conversation().usage());
            self.publish_session_locked(id, slot, &mut state);
            drop(state);
            self.spawn_operation(id, Arc::clone(slot), reservation);
        }
    }

    /// 单独停止后台命令不受前台压缩限制，也不建立新的模型回合。
    pub async fn stop_background_task(&self, session_id: &str, task_id: &str) -> Result<(), RpcError> {
        let tasks = {
            let _lifecycle = self.lock_lifecycle();
            Arc::clone(self.open_action_slot(session_id)?.conversation().bash_tasks())
        };
        let result = tasks.stop(task_id).await;
        if result.is_error {
            Err(invalid_request(result.content))
        } else {
            Ok(())
        }
    }

    pub(super) fn background_shutdown_error(&self, slot: &ConversationSlot, message: String) {
        let mut state = slot.lock_state();
        state.background_error(message);
        self.publish_session_locked(&slot.conversation().thread().thread_id, slot, &mut state);
    }
}
