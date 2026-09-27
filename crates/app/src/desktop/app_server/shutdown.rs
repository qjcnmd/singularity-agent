use super::*;

impl AppServer {
    pub(in crate::desktop) fn fail(&self, error: impl std::fmt::Display) {
        eprintln!("desktop backend stopped: {error}");
        self.failure.cancel();
    }

    /// Stop accepted work before releasing the data-directory lock on pipe EOF.
    pub async fn shutdown(&self) {
        let mut events = self.subscribe();
        let slots: Vec<_> = self.lock_sessions().values().cloned().collect();
        if self.failure.is_cancelled() {
            // 只取消仍可访问的执行；不等待损坏的投影再发结算事件。
            for slot in slots {
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let _ = slot.conversation().abort();
                }));
            }
            return;
        }
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
