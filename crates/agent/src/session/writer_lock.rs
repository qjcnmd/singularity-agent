//! 进程内会话写者。数据目录的 OS 级锁由 singularity 程序持有。

use super::format::SessionError;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};

/// 由 Runner 打开的每个会话共用一份。
#[derive(Default)]
pub struct WriterLockCoordinator {
    writers: Mutex<HashSet<String>>,
}

/// drop 时释放该会话的写者占用。
pub struct WriterLockGuard {
    coordinator: Arc<WriterLockCoordinator>,
    thread_id: String,
}

impl WriterLockCoordinator {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashSet<String>> {
        self.writers.lock().expect("session writer lock poisoned")
    }

    /// 登记写者占用：已被占用就报冲突，不排队等待。
    pub fn acquire(self: &Arc<Self>, thread_id: &str) -> Result<WriterLockGuard, SessionError> {
        let mut writers = self.lock();
        if !writers.insert(thread_id.to_string()) {
            return Err(SessionError::WriterConflict {
                thread_id: thread_id.to_string(),
            });
        }
        Ok(WriterLockGuard {
            coordinator: Arc::clone(self),
            thread_id: thread_id.to_string(),
        })
    }
}

impl Drop for WriterLockGuard {
    fn drop(&mut self) {
        self.coordinator.lock().remove(&self.thread_id);
    }
}
