//! 持久化 Thread 目录操作的唯一入口。
//!
//! `ThreadCatalog` 承载 `sessions_dir` 与进程级写者锁协调器这对参数，客户端
//! 只需持有目录实例；目录操作（创建、列表、恢复、重命名、归档）与只读投影
//! （摘要、分页历史）全部经由本接缝，`store` 模块的实现函数不对 crate 外
//! 暴露。会话正文的持久化事实源仍是 JSONL（见 `crate::store` 模块文档）。

use std::path::PathBuf;
use std::sync::Arc;

use singularity_agent::session::WriterLockCoordinator;

use crate::objects::Thread;
use crate::runner::TurnRunner;
use crate::store::{ResumeError, ThreadListing, ThreadReadPage, ThreadSummary};

/// Thread 目录操作与只读投影的窄接缝。
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

    /// 创建新 Thread（uuid v7 会话文件，属主权限）。
    pub fn create_thread(&self, cwd: &str, model: Option<String>) -> Result<Thread, String> {
        crate::store::create_thread(&self.sessions_dir, cwd, model, &self.coordinator)
    }

    /// 列出可恢复 Thread（头部元数据列表，只读各文件首行）；损坏或非规范
    /// 文件不会阻断其余会话。
    pub fn list_threads(&self) -> Result<Vec<ThreadListing>, String> {
        crate::store::list_threads(&self.sessions_dir)
    }

    /// 重开既有 Thread 并执行崩溃修复。
    pub fn resume_thread(&self, thread_id: &str) -> Result<Thread, ResumeError> {
        crate::store::resume_thread(&self.sessions_dir, thread_id, &self.coordinator)
    }

    /// 为 Thread 追加名称 metadata；JSONL 仍是唯一事实源。
    pub fn rename(&self, thread_id: &str, name: &str) -> Result<(), String> {
        crate::store::rename_thread(&self.sessions_dir, thread_id, name, &self.coordinator)
    }

    /// 归档（删除）指定 Thread 的会话：语义见 [`crate::store::archive_thread`]
    /// ——持写者锁移入 `archived/` 子目录而非物理删除，活动写者拒绝。
    /// typed [`ResumeError`] 原样透传，客户端据此区分 not-found 与写者占用。
    pub fn archive(&self, thread_id: &str) -> Result<(), ResumeError> {
        crate::store::archive_thread(&self.sessions_dir, thread_id, &self.coordinator)
    }

    /// 只读投影单个 Thread 的列表级摘要；不修复或修改会话。
    pub fn read_thread_summary(&self, thread_id: &str) -> Result<ThreadSummary, ResumeError> {
        crate::store::read_thread_summary(
            &self.sessions_dir,
            thread_id,
            self.coordinator.has_local_run(thread_id),
        )
    }

    /// `thread/read` 的按轮分页只读投影（摘要 + compaction 摘要 + 历史页）。
    pub fn paged_read(
        &self,
        thread_id: &str,
        limit: usize,
        before_item: Option<&str>,
    ) -> Result<ThreadReadPage, ResumeError> {
        crate::store::paged_read(
            &self.sessions_dir,
            thread_id,
            limit,
            before_item,
            self.coordinator.has_local_run(thread_id),
        )
    }
}
