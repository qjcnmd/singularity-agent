use std::sync::Arc;

use super::Conversation;
use super::state::TurnControls;
use agent::agent::{AgentError, AgentEvent, compaction_unneeded};
use agent::session::{CompactionInput, retention_budget_tokens};
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
        let retain_tokens = retention_budget_tokens(requests.model_configuration());
        let Some(input) = CompactionInput::select(&self.writer, retain_tokens).await else {
            controls.finish_cancel();
            return Ok(compaction_unneeded(sink, controls.cancellation()));
        };
        let result = requests
            .compact_selected(input, sink, controls.cancellation(), || !controls.finish_cancel())
            .await;
        controls.finish_cancel();
        match result {
            Ok(()) => Ok(TurnStatus::Completed),
            Err(AgentError::Aborted) => Ok(TurnStatus::Interrupted),
            Err(error) => Err(error.detail()),
        }
    }
}
