//! Thread 的目录操作：创建、定位、修复后重开、只读分页投影与归档。
//!
//! JSONL 会话文件是唯一的持久事实源；这里只提供路径、权限以及打开/修复的统一
//! 入口，不复制会话状态。ThreadCatalog 持有 sessions_dir 和写者锁协调器；目录
//! 布局与路径函数留在本模块；crate 根导出目录名与准备入口。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use singularity_agent::session::{
    ExpectedSession, SessionAccess, SessionData, SessionError, SessionManager, SessionMetadata,
    WriterLockCoordinator,
};
use singularity_model::{DEFAULT_PROVIDER_NAME, split_model_selector};
use singularity_protocol::{ThreadReadPage, ThreadSummary};
use uuid::Uuid;

use crate::history::{IndexedTurn, compaction_terminal, index_turn_history, summarize_thread};
use singularity_protocol::Thread;

pub const SESSIONS_DIR_NAME: &str = "sessions";

/// Thread 目录操作与只读投影的统一入口。
pub struct ThreadCatalog {
    sessions_dir: PathBuf,
    coordinator: Arc<WriterLockCoordinator>,
    cache: Mutex<CatalogCache>,
}

impl ThreadCatalog {
    /// 构造目录入口；写者协调器与 TurnRunner 共用同一个进程内实例，目录本身不认识执行器。
    pub fn new(sessions_dir: PathBuf, coordinator: Arc<WriterLockCoordinator>) -> Self {
        Self {
            sessions_dir,
            coordinator,
            cache: Mutex::new(CatalogCache::default()),
        }
    }
}

pub fn prepare_session_dirs(home: &Path) -> Result<(), String> {
    singularity_core::create_data_dir(&home.join(SESSIONS_DIR_NAME))?;
    Ok(())
}

/// Thread 会话文件的规范位置。
pub fn thread_session_path(sessions_dir: &Path, thread_id: &str) -> PathBuf {
    sessions_dir.join(singularity_agent::session::session_file_name(thread_id))
}

/// 创建新的 Thread（uuid v7 会话文件，属主权限）。传进来的 cwd 只是个起点：会话层把它
/// 归一成绝对路径并写进会话头，返回的 Thread 直接用会话头里记录的字符串，使新建、恢复和
/// 列表三条路径上的同一份事实只有一个写法。
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
        record_thread_settings_metadata(&mut session, &thread)
            .map_err(|error| self.session_error(&thread.thread_id, error))?;
        Ok(thread)
    }
}

/// 在已经打开的唯一会话写者上保存 selector；创建任务和提交设置共用这个入口。
/// 创建或变更任务时追加一次选择；Thread 没有模型覆盖时不记录。
pub(crate) fn record_thread_settings_metadata(
    session: &mut SessionManager,
    thread: &Thread,
) -> Result<(), SessionError> {
    let Some(selector) = thread.model.as_deref() else {
        return Ok(());
    };
    let parts = split_model_selector(selector);
    session
        .append_metadata(SessionMetadata::ThreadSettings {
            provider: parts.provider.unwrap_or(DEFAULT_PROVIDER_NAME).to_string(),
            model: parts.model.unwrap_or_default().to_string(),
            reasoning: parts.effort.map(str::to_string),
        })
        .map(|_| ())
}

/// 重开已有的 Thread 并执行崩溃修复，返回投影后的 Thread。修复语义与 turn 打开路径一致：
/// 没有终态的 run operation 补写一条 synthetic operation_finished（interrupted），已启动但
/// 没落结果的工具调用补写一条 synthetic failed ToolResult，任何工具都只报告「结果未知」、
/// 绝不重放；管理器在投影后关闭，每个 turn 由 runner 按单写者约定重新独占打开。
///
/// `expected_cwd` 是所属目录：它在任何重写或修复之前先与会话头登记的 cwd 比对，所以跨目录
/// 传错 thread id 时不会改动目标文件。
impl ThreadCatalog {
    pub fn resume_thread(
        &self,
        thread_id: &str,
        expected_cwd: &str,
    ) -> Result<Thread, CatalogError> {
        let path = thread_session_path(&self.sessions_dir, thread_id);
        let session = SessionManager::open_existing_with_access(
            &path,
            &self.coordinator,
            ExpectedSession {
                id: thread_id,
                cwd: Some(expected_cwd),
            },
            SessionAccess::RepairWrite,
        )
        .map_err(|error| self.session_error(thread_id, error))?;
        let snapshot = self.cache_snapshot(thread_id, self.stamp(thread_id)?, session.into_data());
        Ok(Thread {
            thread_id: thread_id.to_string(),
            cwd: snapshot.summary.cwd.clone(),
            model: snapshot.summary.model.clone(),
        })
    }
}

/// 列出可恢复的 Thread。「确认文件不存在」和「这次没读成功」是两种不同的事实：前者是文件
/// 确实被移除了，后者只说明这一刻读不出来（活动日志的尾部还没稳定、文件暂时不可读）。读失败
/// 时仅在本进程仍有写者、且错误确属未写完的尾行时沿用已确认有效的目录缓存；其余读取
/// 错误直接报告，避免旧摘要掩盖持久损坏。没有可信旧摘要时整次列表失败，不把读失败当删除。
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
            let entry = entry.map_err(|source| CatalogError::Io {
                path: self.sessions_dir.clone(),
                source,
            })?;
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("jsonl") {
                continue;
            }
            let Some(thread_id) = path.file_stem().and_then(|value| value.to_str()) else {
                continue;
            };
            existing.insert(thread_id.to_string());
            // Windows 的目录枚举已经带回元数据，不必再逐个打开文件查询。
            let summary = entry
                .metadata()
                .map_err(|error| CatalogError::session(thread_id, &path, error.into()))
                .and_then(|metadata| self.stamp_from_metadata(thread_id, metadata))
                .and_then(|stamp| self.read_summary_at(thread_id, stamp));
            match summary {
                Ok(summary) => threads.push(summary),
                // 目录项还在但文件已经不在：这是确认过的移除，不是读失败。
                Err(CatalogError::NotFound(_)) => {}
                Err(error)
                    if matches!(
                        &error,
                        CatalogError::Session {
                            source: SessionError::TailRepairRequired,
                            ..
                        }
                    ) && self.coordinator.has_writer(thread_id) =>
                {
                    match self.lock_cache().summaries.get(thread_id) {
                        Some((_, summary)) => threads.push(summary.clone()),
                        None => return Err(error),
                    }
                }
                Err(error) => return Err(error),
            }
        }
        self.lock_cache()
            .summaries
            .retain(|id, _| existing.contains(id));
        // 目录顺序只在这里产生：按最近更新时间降序；时间相同时按任务 ID 升序。
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

/// 只读地投影一个 Thread；不做崩溃修复，也不写入。
impl ThreadCatalog {
    pub fn read_thread_summary(&self, thread_id: &str) -> Result<ThreadSummary, CatalogError> {
        self.read_summary_at(thread_id, self.stamp(thread_id)?)
    }

    fn read_summary_at(
        &self,
        thread_id: &str,
        stamp: FileStamp,
    ) -> Result<ThreadSummary, CatalogError> {
        if let Some((cached_stamp, summary)) = self.lock_cache().summaries.get(thread_id)
            && *cached_stamp == stamp
        {
            return Ok(summary.clone());
        }
        let session = open_thread_read_only(&self.sessions_dir, thread_id)?;
        let (summary, _) = thread_facts(&session, stamp.live_run);
        self.lock_cache()
            .summaries
            .insert(thread_id.to_string(), (stamp, summary.clone()));
        Ok(summary)
    }
}

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
            ExpectedSession {
                id: thread_id,
                cwd: None,
            },
            SessionAccess::Append,
        )
        .map_err(|error| self.session_error(thread_id, error))?;
        session
            .append_metadata(singularity_agent::session::SessionMetadata::ThreadName {
                name: name.to_string(),
            })
            .map_err(|error| self.session_error(thread_id, error))?;
        Ok(())
    }
}

/// Thread 定位与持久化过程中的错误。
#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    #[error("thread {0} was not found")]
    NotFound(String),
    #[error("thread has an active writer")]
    WriterActive,
    #[error("before turn cursor {0} was not found in the thread history")]
    AnchorNotFound(String),
    #[error("任务名称不能为空。")]
    InvalidName,
    /// 会话里登记的目录不是调用方要求打开的目录；打开在任何写入之前就已经失败。
    #[error("thread {0} does not belong to the requested directory")]
    ScopeMismatch(String),
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
            SessionError::ScopeMismatch { .. } => Self::ScopeMismatch(thread_id.to_string()),
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
    // 只保留最近读过的一份完整 ledger；活动链需要时另持同一个 Arc。
    history: Option<(String, FileStamp, Arc<ThreadSnapshot>)>,
}

/// 同一份不可变 ledger 的摘要和轮次索引；分页只展开请求到的那段条目范围。
pub struct ThreadSnapshot {
    pub summary: ThreadSummary,
    /// 最近一次独立压缩的失败/中断终态：slot 重建后冷读时，靠它恢复当前的操作反馈。
    pub terminal: Option<singularity_protocol::SessionTerminalSnapshot>,
    session: SessionData,
    turns: Vec<IndexedTurn>,
}

impl ThreadSnapshot {
    /// 读取锚点之前的一页轮次：`before_turn` 是按轮的 cursor（`turn:{turnId}`，
    /// 前导组是 `turn:leading`），不是 item id；只展开请求范围内的条目。
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
        self.stamp_from_metadata(thread_id, metadata)
    }

    fn stamp_from_metadata(
        &self,
        thread_id: &str,
        metadata: std::fs::Metadata,
    ) -> Result<FileStamp, CatalogError> {
        Ok(FileStamp {
            len: metadata.len(),
            modified: metadata.modified().map_err(|source| CatalogError::Io {
                path: thread_session_path(&self.sessions_dir, thread_id),
                source,
            })?,
            live_run: self.coordinator.has_local_run(thread_id),
        })
    }

    /// 文件版本没变就复用最近的只读快照；读盘在锁外进行，不挡住其他会话访问缓存。
    pub fn read_snapshot(&self, thread_id: &str) -> Result<Arc<ThreadSnapshot>, CatalogError> {
        let stamp = self.stamp(thread_id)?;
        if let Some((id, version, snapshot)) = &self.lock_cache().history
            && id == thread_id
            && *version == stamp
        {
            return Ok(Arc::clone(snapshot));
        }
        let session = open_thread_read_only(&self.sessions_dir, thread_id)?;
        Ok(self.cache_snapshot(thread_id, stamp, session))
    }

    fn cache_snapshot(
        &self,
        thread_id: &str,
        stamp: FileStamp,
        session: SessionData,
    ) -> Arc<ThreadSnapshot> {
        let (summary, turns) = thread_facts(&session, stamp.live_run);
        let snapshot = Arc::new(ThreadSnapshot {
            summary,
            terminal: compaction_terminal(session.entries()),
            session,
            turns,
        });
        let mut cache = self.lock_cache();
        cache.summaries.insert(
            thread_id.to_string(),
            (stamp.clone(), snapshot.summary.clone()),
        );
        cache.history = Some((thread_id.to_string(), stamp, Arc::clone(&snapshot)));
        snapshot
    }
}

/// 一次遍历同时得到摘要和分页共用的回合事实：轮数、终态和手动停止只有索引这一个来源。
fn thread_facts(session: &SessionData, live_run: bool) -> (ThreadSummary, Vec<IndexedTurn>) {
    let turns = index_turn_history(session.entries(), live_run);
    (summarize_thread(session, &turns), turns)
}

/// 归档会话的子目录（相对 sessions_dir）。删除改成归档保留；列表和摘要的扫描
/// 只读顶层的 .jsonl，因此天然跳过 archived/——这是列表过滤所依赖的前提，改动
/// 扫描方式时必须复核。
pub const ARCHIVED_SESSIONS_DIR_NAME: &str = "archived";

/// 归档 Thread 的会话文件：从 sessions 顶层 rename 进 archived/ 子目录，保留下来而不是物理
/// 删除。整个过程持有写者锁：其他写者正在 append 时直接拒绝（CatalogError::WriterActive），
/// 避免归档窗口里的写入落进已经被移走的文件。这次打开还承担另外两件不能推迟到 rename 之后的
/// 事：校验文件身份与归档目标的 thread 一致，以及在写者锁下完成尾行修复——所以它不是一次可以
/// 换成 try_exists 的多余读取，直接 rename 会改变这两项现有行为。同 id 已经归档，或原文件不
/// 存在时，语义都等同于 CatalogError::NotFound。
impl ThreadCatalog {
    pub fn archive(&self, thread_id: &str) -> Result<(), CatalogError> {
        let path = thread_session_path(&self.sessions_dir, thread_id);
        let archived_dir = self.sessions_dir.join(ARCHIVED_SESSIONS_DIR_NAME);
        std::fs::create_dir_all(&archived_dir).map_err(|source| CatalogError::Io {
            path: archived_dir.clone(),
            source,
        })?;
        let archived = archived_dir.join(singularity_agent::session::session_file_name(thread_id));
        if archived.try_exists().map_err(|source| CatalogError::Io {
            path: archived.clone(),
            source,
        })? {
            return Err(CatalogError::NotFound(thread_id.to_string()));
        }
        let session = SessionManager::open_existing_with_access(
            &path,
            &self.coordinator,
            ExpectedSession {
                id: thread_id,
                cwd: None,
            },
            SessionAccess::Append,
        )
        .map_err(|error| self.session_error(thread_id, error))?;
        // 在释放锁之前先把会话文件挪出原路径：窗口内的新写者 open 原路径会得到
        // NotFound，不会再 append 进这个即将归档的文件。
        std::fs::rename(&path, &archived).map_err(|source| CatalogError::Io { path, source })?;
        drop(session);
        // 归档成功后结束 catalog 对这个会话快照的持有：原路径已经不在，缓存留着只会让整份
        // ledger 常驻。只清理 id 匹配的那条，rename 之前返回的失败路径和其他会话不受影响，
        // 已经拿到 Arc 的读者继续持有自己的引用。
        let mut cache = self.lock_cache();
        cache.summaries.remove(thread_id);
        if cache
            .history
            .as_ref()
            .is_some_and(|(id, _, _)| id == thread_id)
        {
            cache.history = None;
        }
        Ok(())
    }
}
