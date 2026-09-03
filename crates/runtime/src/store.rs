//! Thread 目录操作：创建、定位、修复重开、只读分页投影与归档。
//!
//! JSONL 会话文件是唯一持久事实源；这里只做路径、权限与打开/修复的统一
//! 入口，不复制会话状态。[`ThreadCatalog`] 吸收 `sessions_dir` 与写者锁协调器，
//! 布局与纯函数（[`SESSIONS_DIR_NAME`]、[`thread_session_path`]、
//! [`prepare_session_dirs`]）经 crate 根导出。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use singularity_agent::session::{
    SessionAccess, SessionEntry, SessionError, SessionManager, WriterLockCoordinator,
    project_session,
};
use singularity_protocol::ThreadTurn;
use uuid::Uuid;

use crate::history::project_turn_history;
use crate::objects::Thread;
use crate::runner::TurnRunner;

/// 进程级写者锁协调器：TurnRunner 构造一次并贯穿所有会话打开路径，stale
/// 清理每进程只发生一次。从 store 层向下传给 `SessionManager`。
pub type ThreadLockCoordinator = Arc<WriterLockCoordinator>;

/// Thread 摘要的唯一结构由会话层拥有（JSONL 派生事实的投影者），runtime
/// 只转发导出；全链路（store、目录、TUI）共用同一类型。
pub use singularity_agent::session::ThreadSummary;

pub const SESSIONS_DIR_NAME: &str = "sessions";

/// Thread 目录操作与只读投影的入口。
#[derive(Clone)]
pub struct ThreadCatalog {
    sessions_dir: PathBuf,
    coordinator: ThreadLockCoordinator,
}

impl ThreadCatalog {
    pub fn new(runner: &TurnRunner) -> Self {
        Self {
            sessions_dir: runner.sessions_dir().to_path_buf(),
            coordinator: Arc::clone(runner.coordinator()),
        }
    }

    #[cfg(test)]
    pub(crate) fn from_parts(sessions_dir: PathBuf, coordinator: ThreadLockCoordinator) -> Self {
        Self {
            sessions_dir,
            coordinator,
        }
    }
}

/// 创建 home 下的 sessions 目录（Unix 收紧为属主专用）。
pub fn prepare_session_dirs(home: &Path) -> Result<(), String> {
    singularity_core::create_owner_only_dir(&home.join(SESSIONS_DIR_NAME))?;
    Ok(())
}

/// Thread 会话文件的规范位置。
pub fn thread_session_path(sessions_dir: &Path, thread_id: &str) -> PathBuf {
    sessions_dir.join(format!("{thread_id}.jsonl"))
}

/// 创建新 Thread（uuid v7 会话文件，属主权限）。
///
/// 传入的 `cwd` 只是起点：会话层把它归一为绝对路径并写入会话头，返回的
/// Thread 直接采用会话头记录的字符串，因此新建、恢复与列表三条路径上的
/// 同一事实共享一个写法。
impl ThreadCatalog {
    pub fn create_thread(&self, cwd: &str, model: Option<String>) -> Result<Thread, String> {
        let thread_id = Uuid::now_v7().to_string();
        let session = SessionManager::create_with_id_with_coordinator(
            Path::new(cwd),
            &self.sessions_dir,
            &thread_id,
            &self.coordinator,
        )
        .map_err(|_| "failed to create session file".to_string())?;
        singularity_core::ensure_owner_only_file(session.path())?;
        Ok(Thread {
            thread_id,
            cwd: session.cwd_string(),
            model,
        })
    }
}

/// 重开既有 Thread 并执行崩溃修复；返回投影后的 Thread。
///
/// 修复语义与 turn 打开路径一致：未终态的 run operation 补写 synthetic
/// `operation_finished`（interrupted），已启动而未落结果的 `replay: never` 工具
/// 补写 synthetic failed ToolResult，绝不重放。管理器在投影后关闭；每个 turn
/// 由 runner 按单写者合同重新独占打开。
impl ThreadCatalog {
    pub fn resume_thread(&self, thread_id: &str) -> Result<Thread, ResumeError> {
        let path = thread_session_path(&self.sessions_dir, thread_id);
        if !path.exists() {
            return Err(ResumeError::NotFound(thread_id.to_string()));
        }
        let session = SessionManager::open_existing_with_access(
            &path,
            &self.coordinator,
            thread_id,
            SessionAccess::RepairWrite,
        )
        .map_err(|error| ResumeError::Store(error.to_string()))?;
        singularity_agent::session::context::ContextView::validate(&session)
            .map_err(|error| ResumeError::Store(error.to_string()))?;
        let projection = project_session(&session, false);
        let thread = Thread {
            thread_id: thread_id.to_string(),
            cwd: session.cwd_string(),
            model: projection.model,
        };
        Ok(thread)
    }
}

/// 会话列表项：头部事实 + 文件 mtime。列表只读每个文件的首行，
/// 不解析条目、不做聚合；完整投影由单文件入口（`read_thread_summary`）按需承担。
#[derive(Debug, Clone, PartialEq)]
pub struct ThreadListing {
    pub thread_id: String,
    pub cwd: String,
    pub created_at: String,
    pub updated_at: String,
}

/// 列出可恢复 Thread；损坏或非规范文件不会阻断其余会话。
impl ThreadCatalog {
    pub fn list_threads(&self) -> Result<Vec<ThreadListing>, String> {
        if !self.sessions_dir.exists() {
            return Ok(Vec::new());
        }
        let entries = std::fs::read_dir(&self.sessions_dir)
            .map_err(|error| format!("failed to list sessions: {error}"))?;
        let mut threads = Vec::new();
        for entry in entries {
            let Ok(entry) = entry else { continue };
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("jsonl") {
                continue;
            }
            let Ok(header) = singularity_agent::session::read_session_header(&path) else {
                continue;
            };
            if path.file_stem().and_then(|value| value.to_str()) != Some(header.session_id.as_str())
            {
                continue;
            }
            let Some(updated_at) = singularity_agent::session::file::file_modified_iso(&path)
            else {
                continue;
            };
            threads.push(ThreadListing {
                thread_id: header.session_id,
                cwd: header.cwd,
                created_at: header.created_at,
                updated_at,
            });
        }
        threads.sort_by(|left, right| {
            right
                .updated_at
                .cmp(&left.updated_at)
                .then_with(|| left.thread_id.cmp(&right.thread_id))
        });
        Ok(threads)
    }
}

fn open_thread_read_only(
    sessions_dir: &Path,
    thread_id: &str,
) -> Result<SessionManager, ResumeError> {
    let path = thread_session_path(sessions_dir, thread_id);
    if !path.exists() {
        return Err(ResumeError::NotFound(thread_id.to_string()));
    }
    let session = SessionManager::open_existing_read_only(&path)
        .map_err(|error| ResumeError::Store(error.to_string()))?;
    session
        .verify_session_id(thread_id)
        .map_err(|error| ResumeError::Store(error.to_string()))?;
    Ok(session)
}

/// 只读投影一个 Thread；不执行崩溃修复或写入。
impl ThreadCatalog {
    pub fn read_thread_summary(&self, thread_id: &str) -> Result<ThreadSummary, ResumeError> {
        let session = open_thread_read_only(&self.sessions_dir, thread_id)?;
        Ok(project_session(
            &session,
            self.coordinator.has_local_run(thread_id),
        ))
    }
}

/// 为 Thread 追加名称 metadata；JSONL 仍是唯一事实源。
impl ThreadCatalog {
    pub fn rename(&self, thread_id: &str, name: &str) -> Result<(), String> {
        let name = name.trim();
        if name.is_empty() {
            return Err("thread name must not be empty".to_string());
        }
        let path = thread_session_path(&self.sessions_dir, thread_id);
        let mut session = SessionManager::open_existing_with_access(
            &path,
            &self.coordinator,
            thread_id,
            SessionAccess::Append,
        )
        .map_err(|error| error.to_string())?;
        session
            .append_metadata(singularity_agent::session::SessionMetadata::thread_name(
                name,
            ))
            .map_err(|error| error.to_string())?;
        Ok(())
    }
}

/// Thread 定位与持久化错误。
#[derive(Debug, thiserror::Error)]
pub enum ResumeError {
    #[error("thread {0} was not found")]
    NotFound(String),
    #[error("thread has an active writer")]
    WriterActive,
    #[error("before item {0} was not found in the thread history")]
    AnchorNotFound(String),
    #[error("{0}")]
    Store(String),
}

/// thread/read 的一页只读投影：摘要 + 最近一次 compaction 摘要 + 按轮分页的
/// 公开历史。
#[derive(Debug, Clone, PartialEq)]
pub struct ThreadReadPage {
    pub summary: ThreadSummary,
    /// 最近一次 compaction 摘要；无 compaction 时为 None。
    pub compaction_summary: Option<String>,
    /// 本页轮次，按会话顺序（旧→新）排列。
    pub turns: Vec<ThreadTurn>,
}

/// 单次无锁只读解析完成 thread/read 的全部投影：摘要 + 分页条目 +
/// 状态/用量投影。分页是单向往回读：默认返回最新 `limit` 轮；给
/// `before_item`（上一页最旧轮内任意 item 的公开 id）则定位其所属轮，
/// 返回该轮之前的 `limit` 轮。未知锚点返回 [`ResumeError::AnchorNotFound`]。
///
impl ThreadCatalog {
    pub fn paged_read(
        &self,
        thread_id: &str,
        limit: usize,
        before_item: Option<&str>,
    ) -> Result<ThreadReadPage, ResumeError> {
        let session = open_thread_read_only(&self.sessions_dir, thread_id)?;
        let entries = session.entries();
        let live_run = self.coordinator.has_local_run(thread_id);
        let summary = project_session(&session, live_run);
        let compaction_summary = entries.iter().rev().find_map(|entry| match entry {
            SessionEntry::Compaction { compaction, .. } => Some(compaction.summary.clone()),
            _ => None,
        });
        let mut turns = project_turn_history(entries, live_run);
        let page_end = match before_item {
            None => turns.len(),
            Some(anchor) => match turns
                .iter()
                .position(|turn| turn.items.iter().any(|item| item.id() == anchor))
            {
                Some(index) => index,
                None => return Err(ResumeError::AnchorNotFound(anchor.to_string())),
            },
        };
        let page_start = page_end.saturating_sub(limit);
        turns = turns[page_start..page_end].to_vec();
        Ok(ThreadReadPage {
            summary,
            compaction_summary,
            turns,
        })
    }
}

/// 归档会话的子目录（相对 sessions_dir）：删除改为归档保留，列表/摘要
/// 扫描只读顶层 `.jsonl`，对 `archived/` 天然跳过——这是列表过滤的耦合
/// 前提，改动扫描方式时必须复核。
pub const ARCHIVED_SESSIONS_DIR_NAME: &str = "archived";

/// 归档 Thread 的会话文件：从 sessions 顶层 rename 进 `archived/` 子目录，
/// 归档保留而非物理删除。持写者锁完成：其他写者正在 append 时拒绝
///（[`ResumeError::WriterActive`]），避免归档窗口内写入落入 unlinked inode。
/// 同 id 已归档或原文件不存在时语义等同 [`ResumeError::NotFound`]。
impl ThreadCatalog {
    pub fn archive(&self, thread_id: &str) -> Result<(), ResumeError> {
        let path = thread_session_path(&self.sessions_dir, thread_id);
        if !path.exists() {
            return Err(ResumeError::NotFound(thread_id.to_string()));
        }
        let archived_dir = self.sessions_dir.join(ARCHIVED_SESSIONS_DIR_NAME);
        if let Err(error) = std::fs::create_dir_all(&archived_dir) {
            return Err(ResumeError::Store(format!(
                "failed to create archive directory {}: {error}",
                archived_dir.display()
            )));
        }
        let archived = archived_dir.join(format!("{thread_id}.jsonl"));
        if archived.exists() {
            // 同 id 已归档：语义等同 NotFound（重复归档无新动作）。
            return Err(ResumeError::NotFound(thread_id.to_string()));
        }
        let session = SessionManager::open_existing_with_access(
            &path,
            &self.coordinator,
            thread_id,
            SessionAccess::Append,
        )
        .map_err(|error| match error {
            SessionError::WriterConflict { .. } => ResumeError::WriterActive,
            other => ResumeError::Store(other.to_string()),
        })?;
        // 锁释放前先把会话文件挪出原路径：窗口内新写者 open 原路径得
        // NotFound，不会再 append 进即将归档的文件。
        if let Err(error) = std::fs::rename(&path, &archived) {
            // Windows 可能拒绝移动当前进程仍打开的会话文件。释放句柄后重试
            // 归档；仍失败再返回包含两段原因的错误。
            drop(session);
            return std::fs::rename(&path, &archived).map_err(|retry_error| {
                ResumeError::Store(format!(
                    "failed to archive session rollout {}: {error}; {retry_error}",
                    path.display()
                ))
            });
        }
        drop(session);
        Ok(())
    }
}
