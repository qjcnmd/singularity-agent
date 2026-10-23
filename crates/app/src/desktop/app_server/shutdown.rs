use super::*;
impl AppServer {
    /// 管道关闭时等待已接受的操作和子进程结束。
    pub async fn shutdown(&self) {
        let sessions: Vec<_> = self.lock_sessions().values().cloned().collect();
        if let Err(error) = self.stop_sessions(&sessions).await {
            eprintln!("{}", error.message);
        }
        self.mcp.shutdown().await;
    }
    pub(super) async fn stop_sessions(&self, sessions: &[Arc<Conversation>]) -> Result<(), RpcError> {
        let mut stops = tokio::task::JoinSet::new();
        for session in sessions {
            let session = Arc::clone(session);
            stops.spawn(async move { session.stop().await });
        }
        let mut failure = None;
        while let Some(result) = stops.join_next().await {
            if let Err(error) = result.expect("session shutdown completes while the runtime is running") {
                failure = Some(invalid_request(error));
            }
        }
        failure.map_or(Ok(()), Err)
    }
}
