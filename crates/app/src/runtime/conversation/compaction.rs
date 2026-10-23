use super::*;
use singularity_agent::agent::AgentError;
use singularity_protocol::{TurnErrorDetail, TurnStatus};

impl Conversation {
    /// 独立压缩复用会话写者和本次操作的停止控制。
    pub(crate) async fn compact_thread(
        self: &Arc<Self>,
        controls: &TurnControls,
        sink: &mut (dyn FnMut(TurnEvent) + Send),
    ) -> Result<TurnStatus, TurnErrorDetail> {
        let conversation = Arc::clone(self);
        let mut agent =
            tokio::task::spawn_blocking(move || conversation.prepare_request_context(&conversation.thread()))
                .await
                .expect("compaction preparation completes while the runtime is running")?;
        match agent.compact_now(sink, controls.cancellation()).await {
            Ok(_) => Ok(TurnStatus::Completed),
            Err(AgentError::Aborted) => Ok(TurnStatus::Interrupted),
            Err(error) => Err(error.detail()),
        }
    }
}
