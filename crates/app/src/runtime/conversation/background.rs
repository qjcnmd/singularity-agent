use super::*;
use singularity_protocol::{SessionFailure, SessionOperation, StreamEvent};

impl Conversation {
    /// 所有回合共用同一份后台进程；取消前台不取消这些进程。
    pub fn bash_tasks(&self) -> &Arc<singularity_agent::tools::BashTasks> {
        &self.bash_tasks
    }

    /// 返回当前任务的后台命令列表，供界面展示。
    pub fn background_tasks(&self) -> Vec<singularity_protocol::BackgroundTask> {
        self.bash_tasks.list()
    }

    pub(super) fn on_background_update(self: &Arc<Self>, update: singularity_agent::tools::BashTaskUpdate) {
        match update {
            singularity_agent::tools::BashTaskUpdate::Updated(task) => {
                let wake = !task.state.is_running();
                {
                    let mut view = self.lock_view();
                    view.last_event_revision = (self.publish)(StreamEvent::BackgroundTaskUpdated {
                        session_id: self.thread().thread_id,
                        payload: Box::new(singularity_protocol::BackgroundTaskUpdate {
                            phase: self.phase(),
                            task,
                        }),
                    });
                }
                if wake && let Some(operation) = self.pending_operation() {
                    self.spawn_operation(operation);
                }
            }
            singularity_agent::tools::BashTaskUpdate::Failed(message) => self.background_error(message),
        }
    }
    pub(super) fn background_error(&self, message: String) {
        let mut view = self.lock_view();
        self.pause_background_resume();
        view.failure = Some(SessionFailure {
            source: SessionOperation::Turn,
            error: singularity_protocol::TurnErrorDetail {
                cause: singularity_protocol::TurnFailureCause::Store,
                message,
                item_id: None,
            },
        });
        self.publish_changed(&mut view);
    }
    /// 保存失败后保留待处理的输入，停止当前操作并暂停自动继续；用户下一条消息恢复。
    pub fn pause_background_resume(&self) {
        let mut state = self.lock_state();
        state.auto_resume = false;
        if let TurnLifecycle::Running(controls) = &state.turn {
            controls.cancellation().cancel();
        }
    }

    /// 关闭后拒绝新操作，也不再自动启动回合；删除失败时重新打开。
    pub fn set_closing(&self, closing: bool) {
        let _view = self.lock_view();
        let mut state = self.lock_state();
        state.closing = closing;
        if closing && let Some(preview) = &state.context_preview {
            preview.cancel();
        }
    }
}
