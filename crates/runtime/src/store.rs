//! Thread 目录操作：创建、定位、修复重开、只读分页投影与归档。
//!
//! JSONL 会话文件是唯一持久事实源；这里只做路径、权限与打开/修复的统一
//! 入口，不复制会话状态。ThreadCatalog 吸收 sessions_dir 与写者锁协调器，
//! 布局与纯函数（SESSIONS_DIR_NAME、thread_session_path、
//! prepare_session_dirs）经 crate 根导出。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use singularity_agent::session::{
    SessionAccess, SessionData, SessionEntry, SessionError, SessionManager, WriterLockCoordinator,
    project_session,
};
use singularity_protocol::{ThreadReadPage, ThreadSummary};
use uuid::Uuid;

use crate::history::{IndexedTurn, index_turn_history};
use crate::runner::TurnRunner;
use singularity_protocol::Thread;

/// 进程级写者锁协调器：TurnRunner 构造一次并贯穿所有会话打开路径。
pub type ThreadLockCoordinator = Arc<WriterLockCoordinator>;

pub const SESSIONS_DIR_NAME: &str = "sessions";

/// Thread 目录操作与只读投影的入口。
#[derive(Clone)]
pub struct ThreadCatalog {
    sessions_dir: PathBuf,
    coordinator: ThreadLockCoordinator,
    cache: Arc<Mutex<CatalogCache>>,
}

impl ThreadCatalog {
    pub fn new(runner: &TurnRunner) -> Self {
        Self {
            sessions_dir: runner.sessions_dir().to_path_buf(),
            coordinator: Arc::clone(runner.coordinator()),
            cache: Arc::new(Mutex::new(CatalogCache::default())),
        }
    }

    #[cfg(test)]
    pub(crate) fn from_parts(sessions_dir: PathBuf, coordinator: ThreadLockCoordinator) -> Self {
        Self {
            sessions_dir,
            coordinator,
            cache: Arc::new(Mutex::new(CatalogCache::default())),
        }
    }
}

/// 创建 home 下的 sessions 目录。
pub fn prepare_session_dirs(home: &Path) -> Result<(), String> {
    singularity_core::create_data_dir(&home.join(SESSIONS_DIR_NAME))?;
    Ok(())
}

/// Thread 会话文件的规范位置。
pub fn thread_session_path(sessions_dir: &Path, thread_id: &str) -> PathBuf {
    sessions_dir.join(format!("{thread_id}.jsonl"))
}

/// 创建新 Thread（uuid v7 会话文件，属主权限）。
///
/// 传入的 cwd 只是起点：会话层把它归一为绝对路径并写入会话头，返回的
/// Thread 直接采用会话头记录的字符串，因此新建、恢复与列表三条路径上的
/// 同一事实共享一个写法。
impl ThreadCatalog {
    pub fn create_thread(&self, cwd: &str, model: Option<String>) -> Result<Thread, CatalogError> {
        let thread_id = Uuid::now_v7().to_string();
        let mut session = SessionManager::create_with_id_with_coordinator(
            Path::new(cwd),
            &self.sessions_dir,
            &thread_id,
            &self.coordinator,
        )
        .map_err(|error| self.session_error(&thread_id, error))?;
        let thread = Thread {
            thread_id,
            cwd: session.cwd_string(),
            model,
        };
        crate::runner::record_thread_settings_metadata(&mut session, &thread)
            .map_err(|error| self.session_error(&thread.thread_id, error))?;
        Ok(thread)
    }
}

/// 重开既有 Thread 并执行崩溃修复；返回投影后的 Thread。
///
/// 修复语义与 turn 打开路径一致：未终态的 run operation 补写 synthetic
/// operation_finished（interrupted），已启动而未落结果的 replay: never 工具
/// 补写 synthetic failed ToolResult，绝不重放。管理器在投影后关闭；每个 turn
/// 由 runner 按单写者合同重新独占打开。
impl ThreadCatalog {
    pub fn resume_thread(&self, thread_id: &str) -> Result<Thread, CatalogError> {
        let path = thread_session_path(&self.sessions_dir, thread_id);
        let session = SessionManager::open_existing_with_access(
            &path,
            &self.coordinator,
            thread_id,
            SessionAccess::RepairWrite,
        )
        .map_err(|error| self.session_error(thread_id, error))?;
        let projection = project_session(&session, false);
        let thread = Thread {
            thread_id: thread_id.to_string(),
            cwd: session.cwd_string(),
            model: projection.model,
        };
        Ok(thread)
    }
}

/// 列出可恢复 Thread；损坏或非规范文件不会阻断其余会话。
impl ThreadCatalog {
    pub fn list_threads(&self) -> Result<Vec<ThreadSummary>, CatalogError> {
        let entries = match std::fs::read_dir(&self.sessions_dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => {
                return Err(CatalogError::Io {
                    path: self.sessions_dir.clone(),
                    source,
                });
            }
        };
        let mut threads = Vec::new();
        let mut existing = HashSet::new();
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    eprintln!("could not read session directory entry: {error}");
                    continue;
                }
            };
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("jsonl") {
                continue;
            }
            let Some(thread_id) = path.file_stem().and_then(|value| value.to_str()) else {
                continue;
            };
            existing.insert(thread_id.to_string());
            match self.read_thread_summary(thread_id) {
                Ok(summary) => threads.push(summary),
                Err(CatalogError::NotFound(_)) => {}
                Err(error) => {
                    eprintln!("could not list session {}: {error}", path.display());
                }
            }
        }
        self.lock_cache()
            .summaries
            .retain(|id, _| existing.contains(id));
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
) -> Result<SessionData, CatalogError> {
    let path = thread_session_path(sessions_dir, thread_id);
    let session =
        SessionData::open(&path).map_err(|error| CatalogError::session(thread_id, &path, error))?;
    session
        .verify_session_id(thread_id)
        .map_err(|error| CatalogError::session(thread_id, &path, error))?;
    Ok(session)
}

/// 只读投影一个 Thread；不执行崩溃修复或写入。
impl ThreadCatalog {
    pub fn read_thread_summary(&self, thread_id: &str) -> Result<ThreadSummary, CatalogError> {
        let stamp = self.stamp(thread_id)?;
        if let Some((cached_stamp, summary)) = self.lock_cache().summaries.get(thread_id)
            && *cached_stamp == stamp
        {
            return Ok(summary.clone());
        }
        let session = open_thread_read_only(&self.sessions_dir, thread_id)?;
        let summary = project_session(&session, stamp.live_run);
        self.lock_cache()
            .summaries
            .insert(thread_id.to_string(), (stamp, summary.clone()));
        Ok(summary)
    }
}

/// 为 Thread 追加名称 metadata；JSONL 仍是唯一事实源。
impl ThreadCatalog {
    pub fn rename(&self, thread_id: &str, name: &str) -> Result<(), CatalogError> {
        let name = name.trim();
        if name.is_empty() {
            return Err(CatalogError::InvalidName);
        }
        let path = thread_session_path(&self.sessions_dir, thread_id);
        let mut session = SessionManager::open_existing_with_access(
            &path,
            &self.coordinator,
            thread_id,
            SessionAccess::Append,
        )
        .map_err(|error| self.session_error(thread_id, error))?;
        session
            .append_metadata(singularity_agent::session::SessionMetadata::thread_name(
                name,
            ))
            .map_err(|error| self.session_error(thread_id, error))?;
        Ok(())
    }
}

/// Thread 定位与持久化错误。
#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    #[error("thread {0} was not found")]
    NotFound(String),
    #[error("thread has an active writer")]
    WriterActive,
    #[error("before item {0} was not found in the thread history")]
    AnchorNotFound(String),
    #[error("任务名称不能为空。")]
    InvalidName,
    #[error("session {}: {source}", path.display())]
    Session {
        path: PathBuf,
        #[source]
        source: SessionError,
    },
    #[error("{}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

impl CatalogError {
    fn session(thread_id: &str, path: &Path, source: SessionError) -> Self {
        match source {
            SessionError::WriterConflict { .. } => Self::WriterActive,
            SessionError::Io(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Self::NotFound(thread_id.to_string())
            }
            source => Self::Session {
                path: path.to_path_buf(),
                source,
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileStamp {
    len: u64,
    modified: SystemTime,
    live_run: bool,
}

#[derive(Default)]
struct CatalogCache {
    summaries: HashMap<String, (FileStamp, ThreadSummary)>,
    // 只保留最近读取的一份完整 ledger；活动链按需另持同一 Arc。
    history: Option<(String, FileStamp, Arc<ThreadSnapshot>)>,
}

/// 同一不可变 ledger 的摘要和轮次索引，分页只展开所请求的条目范围。
pub struct ThreadSnapshot {
    pub summary: ThreadSummary,
    session: SessionData,
    turns: Vec<IndexedTurn>,
    compaction_summary: Option<String>,
}

impl ThreadSnapshot {
    pub fn page(
        &self,
        limit: usize,
        before_turn: Option<&str>,
    ) -> Result<ThreadReadPage, CatalogError> {
        let end = match before_turn {
            None => self.turns.len(),
            Some(anchor) => self
                .turns
                .iter()
                .position(|turn| turn.cursor() == anchor)
                .ok_or_else(|| CatalogError::AnchorNotFound(anchor.to_string()))?,
        };
        let start = end.saturating_sub(limit);
        let turns = self.turns[start..end]
            .iter()
            .map(|turn| turn.project(&self.session))
            .collect();
        Ok(ThreadReadPage {
            summary: self.summary.clone(),
            compaction_summary: self.compaction_summary.clone(),
            turns,
            next_cursor: (limit > 0 && start > 0).then(|| self.turns[start].cursor()),
        })
    }
}

impl ThreadCatalog {
    fn session_error(&self, thread_id: &str, source: SessionError) -> CatalogError {
        CatalogError::session(
            thread_id,
            &thread_session_path(&self.sessions_dir, thread_id),
            source,
        )
    }

    #[allow(clippy::expect_used)]
    fn lock_cache(&self) -> std::sync::MutexGuard<'_, CatalogCache> {
        self.cache.lock().expect("catalog cache lock poisoned")
    }

    fn stamp(&self, thread_id: &str) -> Result<FileStamp, CatalogError> {
        let path = thread_session_path(&self.sessions_dir, thread_id);
        let metadata = std::fs::metadata(&path)
            .map_err(|source| CatalogError::session(thread_id, &path, source.into()))?;
        Ok(FileStamp {
            len: metadata.len(),
            modified: metadata
                .modified()
                .map_err(|source| CatalogError::Io { path, source })?,
            live_run: self.coordinator.has_local_run(thread_id),
        })
    }

    /// 按文件版本复用最近的只读快照；锁外读盘，不阻塞其他会话的缓存访问。
    pub fn read_snapshot(&self, thread_id: &str) -> Result<Arc<ThreadSnapshot>, CatalogError> {
        let stamp = self.stamp(thread_id)?;
        if let Some((id, version, snapshot)) = &self.lock_cache().history
            && id == thread_id
            && *version == stamp
        {
            return Ok(Arc::clone(snapshot));
        }
        let session = open_thread_read_only(&self.sessions_dir, thread_id)?;
        let entries = session.entries();
        let snapshot = Arc::new(ThreadSnapshot {
            summary: project_session(&session, stamp.live_run),
            turns: index_turn_history(entries, stamp.live_run),
            compaction_summary: entries.iter().rev().find_map(|entry| match entry {
                SessionEntry::Compaction { compaction, .. } => Some(compaction.summary.clone()),
                _ => None,
            }),
            session,
        });
        let mut cache = self.lock_cache();
        cache.summaries.insert(
            thread_id.to_string(),
            (stamp.clone(), snapshot.summary.clone()),
        );
        cache.history = Some((thread_id.to_string(), stamp, Arc::clone(&snapshot)));
        Ok(snapshot)
    }
}

/// 归档会话的子目录（相对 sessions_dir）：删除改为归档保留，列表/摘要
/// 扫描只读顶层 .jsonl，对 archived/ 天然跳过——这是列表过滤的耦合
/// 前提，改动扫描方式时必须复核。
pub const ARCHIVED_SESSIONS_DIR_NAME: &str = "archived";

/// 归档 Thread 的会话文件：从 sessions 顶层 rename 进 archived/ 子目录，
/// 归档保留而非物理删除。持写者锁完成：其他写者正在 append 时拒绝
///（CatalogError::WriterActive），避免归档窗口内写入落入 unlinked inode。
/// 同 id 已归档或原文件不存在时语义等同 CatalogError::NotFound。
impl ThreadCatalog {
    pub fn archive(&self, thread_id: &str) -> Result<(), CatalogError> {
        let path = thread_session_path(&self.sessions_dir, thread_id);
        let archived_dir = self.sessions_dir.join(ARCHIVED_SESSIONS_DIR_NAME);
        std::fs::create_dir_all(&archived_dir).map_err(|source| CatalogError::Io {
            path: archived_dir.clone(),
            source,
        })?;
        let archived = archived_dir.join(format!("{thread_id}.jsonl"));
        if archived.try_exists().map_err(|source| CatalogError::Io {
            path: archived.clone(),
            source,
        })? {
            // 同 id 已归档：语义等同 NotFound（重复归档无新动作）。
            return Err(CatalogError::NotFound(thread_id.to_string()));
        }
        let session = SessionManager::open_existing_with_access(
            &path,
            &self.coordinator,
            thread_id,
            SessionAccess::Append,
        )
        .map_err(|error| self.session_error(thread_id, error))?;
        // 锁释放前先把会话文件挪出原路径：窗口内新写者 open 原路径得
        // NotFound，不会再 append 进即将归档的文件。
        std::fs::rename(&path, &archived).map_err(|source| CatalogError::Io { path, source })?;
        drop(session);
        Ok(())
    }
}
