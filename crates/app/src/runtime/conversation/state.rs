use std::sync::Arc;

use agent::agent::SteeringInboxHandle;
use protocol::{ModelSelection, SessionPhase};
use tokio_util::sync::CancellationToken;

pub(super) struct ConversationState {
    pub(super) model: ModelSelection,
    pub(super) turn: TurnLifecycle,
    pub(super) compacting: bool,
    pub(super) user_stopped: bool,
    pub(super) questions: Arc<agent::agent::UserQuestions>,
    pub(super) steering_inbox: SteeringInboxHandle,
    pub(super) last_context_window: Option<u64>,
    pub(super) auto_resume: bool,
    pub(super) closing: bool,
    pub(super) inspect_cancel: CancellationToken,
}

impl ConversationState {
    pub(super) fn phase(&self) -> SessionPhase {
        match &self.turn {
            TurnLifecycle::Idle => SessionPhase::Idle,
            TurnLifecycle::Running(token) if token.is_cancelled() => SessionPhase::Stopping,
            TurnLifecycle::Running(_) if self.compacting => SessionPhase::Compacting,
            TurnLifecycle::Running(_) => SessionPhase::Running,
        }
    }

    pub(super) fn mark_closing(&mut self) {
        self.closing = true;
        self.inspect_cancel.cancel();
    }

    pub(super) fn has_work(&self, bash_tasks: &agent::tools::BashTasks) -> bool {
        self.turn.is_busy() || bash_tasks.is_busy() || self.has_active_inbox()
    }

    pub(super) fn has_active_inbox(&self) -> bool {
        let inbox = self.steering_inbox.lock().expect("turn inbox lock poisoned");
        if self.auto_resume {
            !inbox.pending().is_empty()
        } else {
            inbox.pending().iter().any(|entry| entry.user_input().is_some())
        }
    }

    pub(super) fn has_pending_resume(&self) -> bool {
        self.auto_resume
            && !self.closing
            && !self.steering_inbox.lock().expect("turn inbox lock poisoned").pending().is_empty()
    }

    pub(super) fn pause_background_resume(&mut self) {
        self.auto_resume = false;
        self.steering_inbox.lock().expect("turn inbox lock poisoned").cancel();
        if let TurnLifecycle::Running(token) = &self.turn {
            token.cancel();
        }
    }
}

pub(super) enum TurnLifecycle {
    Idle,
    Running(CancellationToken),
}

impl TurnLifecycle {
    pub(super) fn is_busy(&self) -> bool {
        !matches!(self, Self::Idle)
    }
}
