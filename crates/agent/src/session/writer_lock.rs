//! 进程内会话写者。数据目录的 OS 级锁由 CLI 持有。

use super::format::SessionError;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// 由 Runner 打开的每个会话共用一份。
#[derive(Default)]
pub struct WriterLockCoordinator {
    /// 键是已被占用的会话；值表示这个写者是否正在执行 run operation。
    writers: Mutex<HashMap<String, bool>>,
}

/// drop 时释放该会话的写者占用与活动运行标记。
pub struct WriterLockGuard {
    coordinator: Arc<WriterLockCoordinator>,
    thread_id: String,
}

impl WriterLockCoordinator {
    #[allow(clippy::expect_used)]
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, bool>> {
        self.writers.lock().expect("session writer lock poisoned")
    }

    /// 本进程当前是否正在执行这个会话。
    pub fn has_local_run(&self, thread_id: &str) -> bool {
        self.lock().get(thread_id).copied().unwrap_or(false)
    }

    /// 只读扫描遇到未写完的尾行时，区分活动追加与已经遗留的损坏文件。
    pub fn has_writer(&self, thread_id: &str) -> bool {
        self.lock().contains_key(thread_id)
    }

    /// 登记写者占用：已被占用就报冲突，不排队等待。
    pub fn acquire(self: &Arc<Self>, thread_id: &str) -> Result<WriterLockGuard, SessionError> {
        let mut writers = self.lock();
        if writers.contains_key(thread_id) {
            return Err(SessionError::WriterConflict {
                thread_id: thread_id.to_string(),
            });
        }
        writers.insert(thread_id.to_string(), false);
        Ok(WriterLockGuard {
            coordinator: Arc::clone(self),
            thread_id: thread_id.to_string(),
        })
    }
}

impl WriterLockGuard {
    #[allow(clippy::expect_used)] // 表项由 acquire 建立，只在该守卫 drop 时移除。
    pub(super) fn observe_run(&mut self, started: bool) {
        let mut writers = self.coordinator.lock();
        let running = writers
            .get_mut(&self.thread_id)
            .expect("guard owns its writer entry");
        *running = started;
    }
}

impl Drop for WriterLockGuard {
    fn drop(&mut self) {
        if let Ok(mut writers) = self.coordinator.writers.lock() {
            writers.remove(&self.thread_id);
        }
    }
}
