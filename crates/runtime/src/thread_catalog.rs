//! 持久化 Thread 目录操作。

use std::path::PathBuf;
use std::sync::Arc;

use singularity_agent::session::WriterLockCoordinator;

use crate::objects::Thread;
use crate::runner::TurnRunner;
use crate::store::{ResumeError, ThreadSummary};

/// Thread 列表、创建、恢复与命名的窄目录接缝。
#[derive(Clone)]
pub struct ThreadCatalog {
    sessions_dir: PathBuf,
    coordinator: Arc<WriterLockCoordinator>,
}

impl ThreadCatalog {
    pub fn new(runner: &TurnRunner) -> Self {
        Self {
            sessions_dir: runner.sessions_dir().to_path_buf(),
            coordinator: Arc::clone(runner.coordinator()),
        }
    }

    pub fn list_threads(&self) -> Result<Vec<ThreadSummary>, String> {
        crate::store::list_threads(&self.sessions_dir)
    }

    pub fn resume_thread(&self, thread_id: &str) -> Result<Thread, ResumeError> {
        crate::store::resume_thread(&self.sessions_dir, thread_id, &self.coordinator)
    }

    pub fn create_thread(&self, cwd: &str, model: Option<String>) -> Result<Thread, String> {
        crate::store::create_thread(&self.sessions_dir, cwd, model, &self.coordinator)
    }

    pub fn rename(&self, thread_id: &str, name: &str) -> Result<(), String> {
        crate::store::rename_thread(&self.sessions_dir, thread_id, name, &self.coordinator)
    }

    /// 归档（删除）指定 Thread 的会话：语义见 [`crate::store::archive_thread`]
    /// ——持写者锁移入 `archived/` 子目录而非物理删除，活动写者拒绝。
    pub fn archive(&self, thread_id: &str) -> Result<(), String> {
        crate::store::archive_thread(&self.sessions_dir, thread_id, &self.coordinator)
            .map_err(|error| error.to_string())
    }
}
