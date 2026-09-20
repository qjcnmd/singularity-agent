//! 进程内会话写者。数据目录的 OS 锁由 CLI 持有。

use super::format::SessionError;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// 经 Runner 打开的每个会话共用。
#[derive(Default)]
pub struct WriterLockCoordinator {
    /// 键表示写者占用；值为当前运行 ID，None 表示只持有写者。
    writers: Mutex<HashMap<String, Option<String>>>,
}

/// 在 drop 时释放会话的写者与活动运行标记。
pub struct WriterLockGuard {
    coordinator: Arc<WriterLockCoordinator>,
    thread_id: String,
}

impl WriterLockCoordinator {
    #[allow(clippy::expect_used)]
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Option<String>>> {
        self.writers.lock().expect("session writer lock poisoned")
    }

    /// 本进程当前是否正在执行该会话。
    pub fn has_local_run(&self, thread_id: &str) -> bool {
        self.lock().get(thread_id).is_some_and(Option::is_some)
    }

    /// 拒绝竞争写入，且不阻塞其他任务。
    pub fn acquire(self: &Arc<Self>, thread_id: &str) -> Result<WriterLockGuard, SessionError> {
        let mut writers = self.lock();
        if writers.contains_key(thread_id) {
            return Err(SessionError::WriterConflict {
                thread_id: thread_id.to_string(),
            });
        }
        writers.insert(thread_id.to_string(), None);
        Ok(WriterLockGuard {
            coordinator: Arc::clone(self),
            thread_id: thread_id.to_string(),
        })
    }
}

impl WriterLockGuard {
    #[allow(clippy::expect_used)] // 表项由 acquire 建立，仅在该守卫 drop 时移除。
    pub(super) fn observe_run(&mut self, operation_id: String, started: bool) {
        let mut writers = self.coordinator.lock();
        let running = writers
            .get_mut(&self.thread_id)
            .expect("guard owns its writer entry");
        if started {
            *running = Some(operation_id);
        } else if running.as_ref() == Some(&operation_id) {
            *running = None;
        }
    }
}

impl Drop for WriterLockGuard {
    fn drop(&mut self) {
        if let Ok(mut writers) = self.coordinator.writers.lock() {
            writers.remove(&self.thread_id);
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
        let mut owner = coordinator.acquire("one").unwrap();
        let other = coordinator.acquire("two").unwrap();
        assert!(!coordinator.has_local_run("one"));
        owner.observe_run("run-1".into(), true);
        assert!(coordinator.has_local_run("one"));
        assert!(!coordinator.has_local_run("two"));
        let contender = Arc::clone(&coordinator);
        assert!(
            std::thread::spawn(move || contender.acquire("one"))
                .join()
                .unwrap()
                .is_err()
        );
        assert!(
            coordinator.has_local_run("one"),
            "a rejected writer preserves the run"
        );
        owner.observe_run("older-run".into(), false);
        assert!(coordinator.has_local_run("one"));
        owner.observe_run("run-1".into(), false);
        assert!(!coordinator.has_local_run("one"));
        assert!(
            coordinator.acquire("one").is_err(),
            "finishing a run keeps its writer"
        );
        owner.observe_run("run-2".into(), true);
        drop(owner);
        assert!(!coordinator.has_local_run("one"));
        assert!(coordinator.acquire("one").is_ok());
        assert!(coordinator.acquire("two").is_err());
        drop(other);
        assert!(coordinator.acquire("two").is_ok());
    }
}
