//! 进程内会话写者。数据目录的 OS 锁由 CLI 持有。

use super::format::SessionError;
use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Writers {
    held: BTreeSet<String>,
    running: BTreeSet<String>,
}

/// 经 Runner 打开的每个会话共用。
#[derive(Default)]
pub struct WriterLockCoordinator {
    writers: Mutex<Writers>,
}

/// 在 drop 时释放会话的写者与活动运行标记。
pub struct WriterLockGuard {
    coordinator: Arc<WriterLockCoordinator>,
    thread_id: String,
    live_operation_id: Option<String>,
}

impl WriterLockCoordinator {
    #[allow(clippy::expect_used)]
    fn lock(&self) -> std::sync::MutexGuard<'_, Writers> {
        self.writers.lock().expect("session writer lock poisoned")
    }

    /// 本进程当前是否正在执行该会话。
    pub fn has_local_run(&self, thread_id: &str) -> bool {
        self.lock().running.contains(thread_id)
    }

    /// 拒绝竞争写入，且不阻塞其他任务。
    pub fn acquire(self: &Arc<Self>, thread_id: &str) -> Result<WriterLockGuard, SessionError> {
        if !self.lock().held.insert(thread_id.to_string()) {
            return Err(SessionError::WriterConflict {
                thread_id: thread_id.to_string(),
            });
        }
        Ok(WriterLockGuard {
            coordinator: Arc::clone(self),
            thread_id: thread_id.to_string(),
            live_operation_id: None,
        })
    }
}

impl WriterLockGuard {
    pub(super) fn observe_run(&mut self, operation_id: &str, started: bool) {
        let mut writers = self.coordinator.lock();
        if started {
            self.live_operation_id = Some(operation_id.to_string());
            writers.running.insert(self.thread_id.clone());
        } else if self.live_operation_id.as_deref() == Some(operation_id) {
            self.live_operation_id = None;
            writers.running.remove(&self.thread_id);
        }
    }
}

impl Drop for WriterLockGuard {
    fn drop(&mut self) {
        if let Ok(mut writers) = self.coordinator.writers.lock() {
            writers.held.remove(&self.thread_id);
            writers.running.remove(&self.thread_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[allow(clippy::unwrap_used)]
    fn competing_writers_fail_and_drop_releases_only_their_session() {
        let coordinator = Arc::new(WriterLockCoordinator::default());
        let owner = coordinator.acquire("one").unwrap();
        let other = coordinator.acquire("two").unwrap();
        let contender = Arc::clone(&coordinator);
        assert!(
            std::thread::spawn(move || contender.acquire("one"))
                .join()
                .unwrap()
                .is_err()
        );
        drop(owner);
        assert!(coordinator.acquire("one").is_ok());
        assert!(coordinator.acquire("two").is_err());
        drop(other);
        assert!(coordinator.acquire("two").is_ok());
    }
}
