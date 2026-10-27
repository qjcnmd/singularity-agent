use super::*;
impl AppServer {
    /// 管道关闭时等待已接受的操作和子进程结束。
    pub async fn shutdown(&self) {
        let sessions: Vec<_> = self.lock_sessions().values().cloned().collect();
        let mut stops = tokio::task::JoinSet::new();
        for session in sessions {
            stops.spawn(async move { session.stop().await });
        }
        while let Some(result) = stops.join_next().await {
            if let Err(error) = result.expect("session shutdown completes while the runtime is running") {
                eprintln!("{error}");
            }
        }
        self.mcp.shutdown().await;
    }
}
