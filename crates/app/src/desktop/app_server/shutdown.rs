use super::*;
use singularity_protocol::SessionPhase;

impl AppServer {
    /// 管道 EOF 释放数据目录锁之前，先停掉已接受的工作。
    pub async fn shutdown(&self) {
        let slots: Vec<_> = self.lock_sessions().values().cloned().collect();
        if let Err(error) = self.stop_sessions(&slots).await {
            eprintln!("{}", error.message);
        }
        self.mcp.shutdown().await;
    }

    /// 先关闭自动续跑并等待前台交接，再回收后台；监听持续确认更新直到回收结束。
    pub(super) async fn stop_sessions(&self, slots: &[Arc<ConversationSlot>]) -> Result<(), RpcError> {
        for slot in slots {
            slot.conversation().set_closing(true);
        }
        self.stop_foreground(slots).await;
        self.stop_background(slots).await
    }

    async fn stop_background(&self, slots: &[Arc<ConversationSlot>]) -> Result<(), RpcError> {
        let mut failure = None;
        for slot in slots {
            if let Err(message) = slot.conversation().bash_tasks().shutdown().await {
                self.background_shutdown_error(slot, message.clone());
                failure = Some(invalid_request(message));
            }
        }
        failure.map_or(Ok(()), Err)
    }

    /// 先订阅，避免漏掉预订开始或释放占用的通知。
    async fn stop_foreground(&self, slots: &[Arc<ConversationSlot>]) {
        let mut events = self.subscribe();
        loop {
            let mut busy = false;
            for slot in slots {
                match slot.conversation().phase() {
                    SessionPhase::Idle => continue,
                    SessionPhase::Running | SessionPhase::Compacting => {
                        if slot.conversation().abort().is_ok() {
                            self.bump_and_emit_session(&slot.conversation().thread().thread_id, slot);
                        }
                    }
                    SessionPhase::Reserved | SessionPhase::Stopping => {}
                }
                busy = true;
            }
            if !busy {
                break;
            }
            let _ = events.recv().await;
        }
    }
}
