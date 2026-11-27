use std::sync::Arc;

use super::Conversation;
use super::state::TurnControls;
use agent::agent::{AgentError, AgentEvent};
use protocol::{TurnErrorDetail, TurnStatus};

impl Conversation {
    pub(super) async fn compact_thread(
        self: &Arc<Self>,
        controls: &TurnControls,
        sink: &mut (dyn FnMut(AgentEvent) + Send),
    ) -> Result<TurnStatus, TurnErrorDetail> {
        let conversation = Arc::clone(self);
        let prepared = tokio::task::spawn_blocking(move || conversation.prepare_request_context())
            .await
            .expect("compaction preparation completes while the runtime is running");
        let mut requests = match prepared {
            Ok(requests) => requests,
            Err(error) => {
                return if controls.finish_cancel() {
                    Ok(TurnStatus::Interrupted)
                } else {
                    Err(error)
                };
            }
        };
        let result = requests.compact_manual(sink, controls.cancellation()).await;
        let cancelled = controls.finish_cancel();
        match result {
            Ok(()) if cancelled => Ok(TurnStatus::Interrupted),
            Ok(()) => Ok(TurnStatus::Completed),
            Err(AgentError::Aborted) => Ok(TurnStatus::Interrupted),
            Err(error) if error.is_session_failure() => Err(error.detail()),
            Err(_) if cancelled => Ok(TurnStatus::Interrupted),
            Err(error) => Err(error.detail()),
        }
    }
}
