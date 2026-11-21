use std::sync::Arc;

use super::{Conversation, TurnLifecycle, view};
use protocol::{SessionFailure, SessionOperation, StreamEvent};

impl Conversation {
    /// 所有回合共用同一份后台进程；取消前台不取消这些进程。
    pub(crate) fn bash_tasks(&self) -> &Arc<agent::tools::BashTasks> {
        &self.bash_tasks
    }

    pub(super) fn on_background_update(self: &Arc<Self>, update: agent::tools::BashTaskUpdate) {
        match update {
            agent::tools::BashTaskUpdate::Updated(task) => {
                let wake = !task.state.is_running();
                {
                    let mut view = self.lock_view();
                    self.publish_background_task(&mut view, task.into());
                }
                if wake && let Some(operation) = self.pending_operation() {
                    self.spawn_operation(operation);
                }
            }
            agent::tools::BashTaskUpdate::Failed { task, error } => {
                let mut view = self.lock_view();
                self.set_background_failure(&mut view, error);
                self.publish_background_task(&mut view, task.into());
                self.publish_changed(&mut view);
            }
        }
    }

    fn publish_background_task(
        &self,
        view: &mut view::ConversationView,
        task: protocol::VisibleBackgroundTask,
    ) {
        self.publish(
            view,
            StreamEvent::BackgroundTaskUpdated {
                session_id: self.thread_id.clone(),
                payload: Box::new(protocol::BackgroundTaskUpdate { phase: self.phase(), task }),
            },
        );
    }
    pub(super) fn background_error(&self, message: String) {
        let mut view = self.lock_view();
        self.set_background_failure(&mut view, message);
        self.publish_changed(&mut view);
    }

    fn set_background_failure(&self, view: &mut view::ConversationView, message: String) {
        self.pause_background_resume();
        view.failure = Some(SessionFailure {
            source: SessionOperation::Turn,
            error: protocol::TurnErrorDetail { message, item_id: None },
        });
    }
    /// 保存失败后保留待处理的输入，停止当前操作并暂停自动继续；用户下一条消息恢复。
    pub(super) fn pause_background_resume(&self) {
        let mut state = self.lock_state();
        state.auto_resume = false;
        if let TurnLifecycle::Running(controls) = &state.turn {
            controls.cancellation().cancel();
        }
    }

    /// 关闭后拒绝新操作，不再自动启动回合，并取消正在进行的上下文预览。
    pub(crate) fn mark_closing(&self) {
        let _view = self.lock_view();
        let mut state = self.lock_state();
        state.closing = true;
        if let Some(preview) = &state.context_preview {
            preview.cancel();
        }
    }

    /// 删除失败时重新开放任务操作入口。
    pub(super) fn clear_closing(&self) {
        let _view = self.lock_view();
        self.lock_state().closing = false;
    }
}
