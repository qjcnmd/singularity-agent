//! 会话 OS 写者锁：每会话一把锁文件 + `try_lock` 快速失败 + 协调锁串行化清理。
//!
//! 同一会话同一时刻至多一个存活写者由文件锁强制执行（跨进程），不依赖单进程内存状态。
//! 协调锁串行化 stale 锁清理与 Guard Drop 的删除操作；Guard Drop 先关句柄再
//! 删锁文件（Windows 兼容：必须先关闭句柄才能删除文件）。

use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use singularity_core::create_owner_only_dir;

use super::format::SessionError;

const WRITER_LOCK_DIR: &str = "thread-writer-locks";
const COORDINATION_LOCK_FILE: &str = ".coordination.lock";

/// 会话写者锁协调器：锁目录的持有者与一次性的 stale 清理触发器。
pub struct WriterLockCoordinator {
    directory: PathBuf,
    cleanup_attempted: AtomicBool,
    /// 本进程已 durable 开始且尚未终结的 run operation；只读投影据此
    /// 辨别 live turn。跨进程排他性仍由 OS 文件锁负责。
    local_live_runs: Mutex<std::collections::BTreeSet<String>>,
}

/// 持锁的 RAII 守卫；释放时删除锁文件。
pub struct WriterLockGuard {
    coordinator: Arc<WriterLockCoordinator>,
    thread_id: String,
    path: PathBuf,
    file: Option<File>,
    live_operation_id: Option<String>,
}

impl WriterLockCoordinator {
    /// 锁目录与会话文件所在目录同级（`<home>/thread-writer-locks`）。
    pub fn new(sessions_dir: &Path) -> Self {
        Self {
            directory: sessions_dir
                .parent()
                .unwrap_or(sessions_dir)
                .join(WRITER_LOCK_DIR),
            cleanup_attempted: AtomicBool::new(false),
            local_live_runs: Mutex::new(std::collections::BTreeSet::new()),
        }
    }

    /// 当前进程是否正在执行该 Thread 的 run。状态未知时按活动收敛，避免把
    /// 可能仍在执行的 turn 投影成陈旧操作。
    pub fn has_local_run(&self, thread_id: &str) -> bool {
        self.local_live_runs
            .lock()
            .map(|runs| runs.contains(thread_id))
            .unwrap_or(true)
    }

    /// 快速失败地获取指定会话的写者锁；被其他写者占用时返回
    /// [`SessionError::WriterConflict`]。
    pub fn acquire(self: &Arc<Self>, thread_id: &str) -> Result<WriterLockGuard, SessionError> {
        let _coordination_lock = self.lock_coordination()?;
        if !self.cleanup_attempted.swap(true, Ordering::Relaxed)
            && let Err(error) = self.remove_stale_thread_locks()
        {
            // 清理失败不阻断获取：残留锁文件会被下次清理重试。
            eprintln!("sg: failed to clean up stale thread writer locks: {error}");
        }

        let path = self.directory.join(format!("{thread_id}.lock"));
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|error| SessionError::WriterLock {
                context: format!("failed to open thread writer lock {}", path.display()),
                source: error,
            })?;

        match file.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(SessionError::WriterConflict {
                    thread_id: thread_id.to_string(),
                });
            }
            Err(std::fs::TryLockError::Error(error)) => {
                return Err(SessionError::WriterLock {
                    context: format!("failed to acquire thread writer lock {}", path.display()),
                    source: error,
                });
            }
        }

        Ok(WriterLockGuard {
            coordinator: Arc::clone(self),
            thread_id: thread_id.to_string(),
            path,
            file: Some(file),
            live_operation_id: None,
        })
    }

    /// 获取协调锁（持有期间串行化 stale 清理与锁文件删除）。
    fn lock_coordination(&self) -> Result<File, SessionError> {
        create_owner_only_dir(&self.directory).map_err(|error| SessionError::WriterLock {
            context: format!(
                "failed to create writer lock directory {}",
                self.directory.display()
            ),
            source: io::Error::other(error),
        })?;
        let path = self.directory.join(COORDINATION_LOCK_FILE);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|error| SessionError::WriterLock {
                context: format!("failed to open coordination lock {}", path.display()),
                source: error,
            })?;
        file.lock().map_err(|error| SessionError::WriterLock {
            context: format!("failed to acquire coordination lock {}", path.display()),
            source: error,
        })?;
        Ok(file)
    }

    /// 移除未被任何进程持有的过期锁文件（每个协调器进程至多尝试一次）。
    fn remove_stale_thread_locks(&self) -> io::Result<()> {
        for entry in fs::read_dir(&self.directory)? {
            let entry = entry?;
            let Some(file_name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if !file_name.ends_with(".lock") {
                continue;
            }
            let path = entry.path();
            let file = match OpenOptions::new().read(true).write(true).open(&path) {
                Ok(file) => file,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(_) => continue,
            };
            match file.try_lock() {
                Ok(()) => {
                    drop(file);
                    if let Err(error) = fs::remove_file(&path)
                        && error.kind() != io::ErrorKind::NotFound
                    {
                        eprintln!(
                            "sg: failed to remove stale thread writer lock {}: {error}",
                            path.display()
                        );
                    }
                }
                Err(std::fs::TryLockError::WouldBlock) => {}
                Err(std::fs::TryLockError::Error(_)) => {}
            }
        }
        Ok(())
    }
}

impl WriterLockGuard {
    /// Ledger 追加成功后同步本进程的 live-run 投影。Operation id 必须匹配，
    /// 异常终态不能误清除仍在执行的回合。
    pub(super) fn observe_run(&mut self, operation_id: &str, started: bool) {
        if started {
            self.live_operation_id = Some(operation_id.to_string());
            if let Ok(mut runs) = self.coordinator.local_live_runs.lock() {
                runs.insert(self.thread_id.clone());
            }
        } else if self.live_operation_id.as_deref() == Some(operation_id) {
            self.live_operation_id = None;
            if let Ok(mut runs) = self.coordinator.local_live_runs.lock() {
                runs.remove(&self.thread_id);
            }
        }
    }
}

impl Drop for WriterLockGuard {
    fn drop(&mut self) {
        let coordination_lock = self.coordinator.lock_coordination().ok();
        // 先关闭句柄再删除锁文件：Windows 上打开的文件无法删除。
        drop(self.file.take());
        if coordination_lock.is_some()
            && let Err(error) = fs::remove_file(&self.path)
            && error.kind() != io::ErrorKind::NotFound
        {
            eprintln!(
                "sg: failed to remove thread writer lock {}: {error}",
                self.path.display()
            );
        }
        if self.live_operation_id.is_some()
            && let Ok(mut runs) = self.coordinator.local_live_runs.lock()
        {
            runs.remove(&self.thread_id);
        }
        drop(coordination_lock);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)] // 测试断言惯例
    use std::ffi::OsString;
    use std::fs;
    use std::sync::Arc;

    use tempfile::TempDir;

    use super::*;

    fn lock_dir(home: &TempDir) -> PathBuf {
        home.path().join(WRITER_LOCK_DIR)
    }

    #[test]
    fn writer_locks_reject_competing_owners_and_release_their_files() {
        let home = TempDir::new().expect("temp dir");
        let sessions = home.path().join("sessions");
        let primary = Arc::new(WriterLockCoordinator::new(&sessions));
        let secondary = Arc::new(WriterLockCoordinator::new(&sessions));
        let thread_id = "owner-thread";
        let other_thread_id = "other-thread";

        let owner = primary.acquire(thread_id).expect("acquire writer lock");
        let lock_path = lock_dir(&home).join(format!("{thread_id}.lock"));
        assert!(lock_path.exists());

        let err = match secondary.acquire(thread_id) {
            Ok(_) => panic!("competing owner should fail"),
            Err(err) => err,
        };
        assert!(matches!(err, SessionError::WriterConflict { .. }));
        let other_owner = secondary
            .acquire(other_thread_id)
            .expect("other thread should acquire its own lock");

        drop(owner);
        assert!(!lock_path.exists());
        let next_owner = secondary
            .acquire(thread_id)
            .expect("released thread should accept another owner");
        drop(next_owner);
        drop(other_owner);

        let entries = fs::read_dir(lock_dir(&home))
            .expect("read lock directory")
            .map(|entry| entry.expect("lock directory entry").file_name())
            .collect::<Vec<_>>();
        assert_eq!(entries, vec![OsString::from(COORDINATION_LOCK_FILE)]);
    }

    #[test]
    fn first_acquisition_removes_stale_locks_without_removing_active_locks() {
        let home = TempDir::new().expect("temp dir");
        let sessions = home.path().join("sessions");
        let primary = Arc::new(WriterLockCoordinator::new(&sessions));
        let active_thread_id = "active-thread";
        let active_owner = primary
            .acquire(active_thread_id)
            .expect("acquire active writer lock");

        let stale_thread_id = "stale-thread";
        let stale_path = lock_dir(&home).join(format!("{stale_thread_id}.lock"));
        fs::File::create(&stale_path).expect("create stale writer lock");

        let secondary = Arc::new(WriterLockCoordinator::new(&sessions));
        let secondary_owner = secondary
            .acquire("new-thread")
            .expect("acquire writer lock after cleanup");

        assert!(!stale_path.exists());
        let err = match secondary.acquire(active_thread_id) {
            Ok(_) => panic!("active writer should survive cleanup"),
            Err(err) => err,
        };
        assert!(matches!(err, SessionError::WriterConflict { .. }));

        drop(secondary_owner);
        drop(active_owner);
    }

    #[test]
    fn competing_acquire_across_threads_fails_fast() {
        let home = TempDir::new().expect("temp dir");
        let sessions = home.path().join("sessions");
        let coordinator = Arc::new(WriterLockCoordinator::new(&sessions));
        let _owner = coordinator.acquire("thread-x").expect("first owner");

        let contender = Arc::clone(&coordinator);
        let result = std::thread::spawn(move || contender.acquire("thread-x"))
            .join()
            .expect("contender thread");
        assert!(
            matches!(result, Err(SessionError::WriterConflict { .. })),
            "competing acquire from another thread must fail fast"
        );
    }
}
