use std::sync::{Arc, Mutex};

use agent::agent::SteeringInboxHandle;
use protocol::{ModelSelection, SessionPhase};
use tokio_util::sync::CancellationToken;

use super::ConversationError;

struct CancelState {
    accepting: bool,
    compacting: bool,
    user_stopped: bool,
}

pub(super) struct TurnControls {
    pub(super) turn_id: String,
    cancellation: CancellationToken,
    cancel_state: Mutex<CancelState>,
    pub(super) questions: Arc<agent::agent::UserQuestions>,
}

impl TurnControls {
    pub(super) fn new(turn_id: String) -> Self {
        Self {
            turn_id,
            cancellation: CancellationToken::new(),
            cancel_state: Mutex::new(CancelState {
                accepting: true,
                compacting: false,
                user_stopped: false,
            }),
            questions: Arc::new(agent::agent::UserQuestions::default()),
        }
    }

    pub(super) fn set_compacting(&self, active: bool) {
        self.cancel_state.lock().expect("cancel window lock poisoned").compacting = active;
    }

    pub(super) fn is_compacting(&self) -> bool {
        self.cancel_state.lock().expect("cancel window lock poisoned").compacting
    }

    pub(super) fn cancellation(&self) -> &CancellationToken {
        &self.cancellation
    }

    pub(super) fn accept_cancel(&self) -> Result<(), ConversationError> {
        {
            let mut state = self.cancel_state.lock().expect("cancel window lock poisoned");
            if !state.accepting {
                return Err(ConversationError::Busy);
            }
            state.user_stopped = true;
            self.cancellation.cancel();
        }
        Ok(())
    }

    pub(super) fn finish_cancel(&self) -> bool {
        let mut state = self.cancel_state.lock().expect("cancel window lock poisoned");
        state.accepting = false;
        state.user_stopped
    }
}

pub(super) struct ConversationState {
    pub(super) model: ModelSelection,
    pub(super) turn: TurnLifecycle,
    pub(super) steering_inbox: SteeringInboxHandle,
    pub(super) last_context_window: Option<u64>,
    pub(super) auto_resume: bool,
    pub(super) closing: bool,
}

impl ConversationState {
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
        if let TurnLifecycle::Running(controls) = &self.turn {
            controls.cancellation().cancel();
        }
    }
}

pub(super) enum TurnLifecycle {
    Idle,
    Running(Arc<TurnControls>),
}

impl TurnLifecycle {
    pub(super) fn is_busy(&self) -> bool {
        !matches!(self, Self::Idle)
    }

    pub(super) fn phase(&self) -> SessionPhase {
        match self {
            Self::Idle => SessionPhase::Idle,
            Self::Running(controls) if controls.cancellation().is_cancelled() => SessionPhase::Stopping,
            Self::Running(controls) if controls.is_compacting() => SessionPhase::Compacting,
            Self::Running(_) => SessionPhase::Running,
        }
    }
}
