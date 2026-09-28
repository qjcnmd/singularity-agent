use super::*;

impl AppServer {
    /// Stop accepted work before releasing the data-directory lock on pipe EOF.
    pub async fn shutdown(&self) {
        let mut events = self.subscribe();
        let slots: Vec<_> = self.lock_sessions().values().cloned().collect();
        loop {
            let mut busy = false;
            for slot in &slots {
                match slot.conversation().snapshot().phase {
                    SessionPhase::Idle => continue,
                    SessionPhase::Running | SessionPhase::Compacting => {
                        if let Err(error) = slot.conversation().abort()
                            && !matches!(error, ConversationControlError::NotRunning)
                        {
                            eprintln!("desktop shutdown cancellation: {error}");
                        }
                    }
                    SessionPhase::Reserved | SessionPhase::Stopping => {}
                }
                busy = true;
            }
            if !busy {
                break;
            }
            // A just-accepted reservation may not have started yet. Its first event lets us
            // cancel it; settled events follow release. Lag also requires a fresh state read.
            let _ = events.recv().await;
        }
    }
}
