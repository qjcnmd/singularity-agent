use super::*;
use singularity_protocol::SessionPhase;

impl AppServer {
    /// 管道 EOF 释放数据目录锁之前，先停掉已接受的工作。
    pub async fn shutdown(&self) {
        let slots: Vec<_> = self.lock_sessions().values().cloned().collect();
        self.stop_sessions(&slots).await;
        self.mcp.shutdown().await;
    }

    /// 归档与退出共用停止和等待结算：先订阅，避免漏掉预订开始或释放占用的通知。
    pub(super) async fn stop_sessions(&self, slots: &[Arc<ConversationSlot>]) {
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
            // 刚接下的预订可能还没开始跑，它的首个事件让取消能生效；结算事件跟在释放之后。
            // 接收滞后时也要重新读一次状态。
            let _ = events.recv().await;
        }
    }
}
