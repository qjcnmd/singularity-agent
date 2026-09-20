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
    ExpectedSession, SessionAccess, SessionData, SessionError, SessionManager, SessionMetadata,
    WriterLockCoordinator,
};
use singularity_model::{DEFAULT_PROVIDER_NAME, split_model_selector};
use singularity_protocol::{ThreadReadPage, ThreadSummary};
use uuid::Uuid;

use crate::history::{IndexedTurn, compaction_terminal, index_turn_history, summarize_thread};
use singularity_protocol::Thread;

pub const SESSIONS_DIR_NAME: &str = "sessions";

/// Thread 目录操作与只读投影的入口。
pub struct ThreadCatalog {
    sessions_dir: PathBuf,
    coordinator: Arc<WriterLockCoordinator>,
    cache: Mutex<CatalogCache>,
}

impl ThreadCatalog {
    /// 会话存储的两项依赖由装配入口显式创建并传入：sessions 目录，以及与
    /// TurnRunner 共用的同一个进程内写者协调器。目录不认识执行器。
    pub fn new(sessions_dir: PathBuf, coordinator: Arc<WriterLockCoordinator>) -> Self {
        Self {
            sessions_dir,
            coordinator,
            cache: Mutex::new(CatalogCache::default()),
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
    sessions_dir.join(singularity_agent::session::session_file_name(thread_id))
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
        record_thread_settings_metadata(&mut session, &thread)
            .map_err(|error| self.session_error(&thread.thread_id, error))?;
        Ok(thread)
    }
}

/// 在已打开的唯一会话写者上保存 selector，任务创建和设置提交共用此入口。
/// 创建或变更任务时追加选择；Thread 无模型覆盖时不记录。
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

/// 重开既有 Thread 并执行崩溃修复；返回投影后的 Thread。
///
/// 修复语义与 turn 打开路径一致：未终态的 run operation 补写 synthetic
/// operation_finished（interrupted），已启动而未落结果的工具调用补写 synthetic
/// failed ToolResult；任何工具都只报告未知结果、绝不重放。管理器在投影后关闭；
/// 每个 turn 由 runner 按单写者合同重新独占打开。
///
/// `expected_cwd` 是要打开的所属目录：它在任何重写或修复之前与会话头部登记的
/// cwd 比对，因此跨目录传错 thread id 时不会改动目标文件。
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

/// 列出可恢复 Thread。
///
/// 「确认文件不存在」与「本次未能读取」是两种不同事实：前者是目录里真实的
/// 移除，后者只说明这一刻读不出（活动日志的尾部尚未稳定、文件暂时不可读）。
/// 读失败时沿用已确认有效的目录缓存——那份已提交事实仍代表该会话存在；没有
/// 可信旧摘要时整次列表失败，由调用方沿既有重同步路径报告，绝不返回一份
/// 「看似完整却缺项」的成功快照，让读侧把它当成删除。
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
            // Windows 目录枚举已携带元数据，不再逐文件重新打开查询。
            let summary = entry
                .metadata()
                .map_err(|error| CatalogError::session(thread_id, &path, error.into()))
                .and_then(|metadata| self.stamp_from_metadata(thread_id, metadata))
                .and_then(|stamp| self.read_summary_at(thread_id, stamp));
            match summary {
                Ok(summary) => threads.push(summary),
                // 目录项存在而文件已不在：这是已确认的移除，不是读失败。
                Err(CatalogError::NotFound(_)) => {}
                Err(error) => match self.lock_cache().summaries.get(thread_id) {
                    Some((_, summary)) => threads.push(summary.clone()),
                    None => return Err(error),
                },
            }
        }
        self.lock_cache()
            .summaries
            .retain(|id, _| existing.contains(id));
        // 目录顺序的唯一生产点：按最近更新时间降序，同一时间按任务 ID 升序。
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

/// Thread 定位与持久化错误。
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
    /// 会话登记的目录不是调用方要求打开的目录；打开在任何写入前已经失败。
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
    // 只保留最近读取的一份完整 ledger；活动链按需另持同一 Arc。
    history: Option<(String, FileStamp, Arc<ThreadSnapshot>)>,
}

/// 同一不可变 ledger 的摘要和轮次索引，分页只展开所请求的条目范围。
pub struct ThreadSnapshot {
    pub summary: ThreadSummary,
    /// 最近一次独立压缩的失败/中断终态：slot 重建后的冷读据此恢复当前操作反馈。
    pub terminal: Option<singularity_protocol::SessionTerminalSnapshot>,
    session: SessionData,
    turns: Vec<IndexedTurn>,
}

impl ThreadSnapshot {
    /// 读取锚点之前的一页轮次：`before_turn` 是按轮 cursor
    /// （`turn:{turnId}`，前导组为 `turn:leading`），不是 item id；只展开
    /// 所请求范围内的条目。
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
            // 当前操作反馈同样由账本派生，不另存一份：slot 重建后的冷读据此
            // 恢复最近独立压缩的终态。
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

/// 一次遍历得到摘要与分页共用的回合事实：轮数、终态与手动停止只有索引一个来源。
fn thread_facts(session: &SessionData, live_run: bool) -> (ThreadSummary, Vec<IndexedTurn>) {
    let turns = index_turn_history(session.entries(), live_run);
    (summarize_thread(session, &turns), turns)
}

/// 归档会话的子目录（相对 sessions_dir）：删除改为归档保留，列表/摘要
/// 扫描只读顶层 .jsonl，对 archived/ 天然跳过——这是列表过滤的耦合
/// 前提，改动扫描方式时必须复核。
pub const ARCHIVED_SESSIONS_DIR_NAME: &str = "archived";

/// 归档 Thread 的会话文件：从 sessions 顶层 rename 进 archived/ 子目录，
/// 归档保留而非物理删除。持写者锁完成：其他写者正在 append 时拒绝
///（CatalogError::WriterActive），避免归档窗口内写入落入 unlinked inode。
/// 这次打开同时承担另外两件不能推迟到 rename 之后的事：校验文件身份与
/// 归档目标 thread 一致，以及在写者锁下完成尾行修复——因此它不是可以
/// 换成 try_exists 的多余读取，直接 rename 会改变这两项现有行为。
/// 同 id 已归档或原文件不存在时语义等同 CatalogError::NotFound。
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
            // 同 id 已归档：语义等同 NotFound（重复归档无新动作）。
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
        // 锁释放前先把会话文件挪出原路径：窗口内新写者 open 原路径得
        // NotFound，不会再 append 进即将归档的文件。
        std::fs::rename(&path, &archived).map_err(|source| CatalogError::Io { path, source })?;
        drop(session);
        // 归档成功后由同一 owner 结束 catalog 对该会话快照的持有：原路径已不在，
        // 缓存再留着只会让整份 ledger 常驻。只清理匹配 id 的条目；rename 之前
        // 返回的失败路径不清缓存，其它会话的快照也不受影响。已经拿到 Arc 的读者
        // 继续持有自己的引用，不被强制销毁。
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
