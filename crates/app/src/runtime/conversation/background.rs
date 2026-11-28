use std::sync::Arc;

use super::{Conversation, ConversationInner};
use protocol::{SessionFailure, SessionOperation, StreamEvent};

impl Conversation {
    pub(crate) fn bash_tasks(&self) -> &Arc<agent::tools::BashTasks> {
        &self.bash_tasks
    }

    pub(super) fn on_background_update(self: &Arc<Self>, update: agent::tools::BashTaskUpdate) {
        match update {
            agent::tools::BashTaskUpdate::Updated(task) => {
                let wake = !task.state.is_running();
                let next = {
                    let mut inner = self.lock_inner();
                    self.publish_background_task(&mut inner, task.into());
                    wake.then(|| self.begin_pending_operation(&mut inner)).flatten()
                };
                if let Some(operation) = next {
                    self.spawn_operation(operation);
                }
            }
            agent::tools::BashTaskUpdate::Failed { task, error } => {
                let mut inner = self.lock_inner();
                self.set_background_failure(&mut inner, error);
                self.publish_background_task(&mut inner, task.into());
                self.publish_changed(&mut inner);
            }
        }
    }

    fn publish_background_task(&self, inner: &mut ConversationInner, task: protocol::VisibleBackgroundTask) {
        let phase = inner.state.turn.phase();
        self.publish(
            inner,
            StreamEvent::BackgroundTaskUpdated {
                session_id: self.thread_id.clone(),
                payload: Box::new(protocol::BackgroundTaskUpdate { phase, task }),
            },
        );
    }

    pub(super) fn background_error(&self, message: String) {
        let mut inner = self.lock_inner();
        self.set_background_failure(&mut inner, message);
        self.publish_changed(&mut inner);
    }

    fn set_background_failure(&self, inner: &mut ConversationInner, message: String) {
        inner.state.pause_background_resume();
        inner.view.failure = Some(SessionFailure {
            source: SessionOperation::Turn,
            error: protocol::TurnErrorDetail { message, item_id: None },
        });
    }

    pub(crate) fn mark_closing(&self) {
        self.lock_inner().state.closing = true;
    }

    pub(crate) fn clear_closing(&self) {
        self.lock_inner().state.closing = false;
    }
}
