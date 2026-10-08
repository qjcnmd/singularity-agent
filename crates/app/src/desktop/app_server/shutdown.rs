use super::*;
use singularity_protocol::SessionPhase;

impl AppServer {
    /// 管道 EOF 释放数据目录锁之前，先停掉已接受的工作。
    pub async fn shutdown(&self) {
        let mut events = self.subscribe();
        let slots: Vec<_> = self.lock_sessions().values().cloned().collect();
        loop {
            let mut busy = false;
            for slot in &slots {
                match slot.conversation().phase() {
                    SessionPhase::Idle => continue,
                    SessionPhase::Running | SessionPhase::Compacting => {
                        let _ = slot.conversation().abort();
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
        self.mcp.shutdown().await;
    }
}
