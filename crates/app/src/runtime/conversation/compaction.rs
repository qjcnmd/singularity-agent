use super::*;
use singularity_agent::agent::{AgentError, compaction_unneeded};
use singularity_agent::compaction::CompactionPrefix;
use singularity_protocol::{TurnErrorDetail, TurnStatus};

impl Conversation {
    /// 独立压缩复用会话写者和本次操作的停止控制。
    pub(crate) async fn compact_thread(
        self: &Arc<Self>,
        controls: &TurnControls,
        sink: &mut (dyn FnMut(TurnEvent) + Send),
    ) -> Result<TurnStatus, TurnErrorDetail> {
        let Some(prefix) = CompactionPrefix::select(&self.writer).await else {
            controls.finish_cancel();
            return match compaction_unneeded(sink, controls.cancellation()) {
                Ok(_) => Ok(TurnStatus::Completed),
                Err(_) => Ok(TurnStatus::Interrupted),
            };
        };
        let conversation = Arc::clone(self);
        let prepared =
            tokio::task::spawn_blocking(move || conversation.prepare_request_context(&conversation.thread()))
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
        let result = requests
            .compact_selected(prefix, sink, controls.cancellation(), || !controls.finish_cancel())
            .await;
        controls.finish_cancel();
        match result {
            Ok(_) => Ok(TurnStatus::Completed),
            Err(AgentError::Aborted) => Ok(TurnStatus::Interrupted),
            Err(error) => Err(error.detail()),
        }
    }
}
