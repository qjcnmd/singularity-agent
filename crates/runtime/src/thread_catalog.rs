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
    SessionAccess, SessionData, SessionError, SessionManager, SessionMetadata,
    WriterLockCoordinator,
};
use singularity_model::parse_model_selector;
use singularity_protocol::{ThreadReadPage, ThreadSummary};
use uuid::Uuid;

use crate::history::{IndexedTurn, index_turn_history, summarize_thread};
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
        if let Some(selector) = model.as_deref() {
            parse_model_selector(selector)
                .map_err(|error| CatalogError::InvalidModel(error.to_string()))?;
        }
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
    let parts = parse_model_selector(selector).expect("thread model was validated before saving");
    session
        .append_metadata(SessionMetadata::ThreadSettings {
            provider: parts.provider_name.to_string(),
            model: parts.model_name.to_string(),
            reasoning: parts.reasoning_effort.map(str::to_string),
        })
        .map(|_| ())
}

/// 重开已有的 Thread 并执行崩溃修复，返回投影后的 Thread。修复语义与 turn 打开路径一致：
/// 没有终态的 run operation 补写一条 synthetic operation_finished（interrupted），已启动但
/// 没落结果的工具调用补写一条 synthetic failed ToolResult，任何工具都只报告「结果未知」、
/// 绝不重放；管理器在投影后关闭，每个 turn 由 runner 按单写者约定重新独占打开。
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
        let model = session.entries().iter().rev().find_map(|entry| {
            let singularity_agent::session::SessionEntry::Metadata {
                metadata:
                    SessionMetadata::ThreadSettings {
                        provider,
                        model,
                        reasoning,
                    },
                ..
            } = entry
            else {
                return None;
            };
            Some(singularity_model::compose_model_selector(
                provider,
                model,
                reasoning.as_deref(),
            ))
        });
        let snapshot = self.cache_snapshot(thread_id, self.stamp(thread_id)?, session.into_data());
        Ok(Thread {
            thread_id: thread_id.to_string(),
            cwd: snapshot.summary.cwd.clone(),
            model,
        })
    }
}

/// 列出完整记录投影的 Thread。忽略尚未写完的尾行，其他读取错误直接报告；
/// 目录枚举后文件已移除时跳过该项。
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
        let (summary, _) = thread_facts(&session);
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
            thread_id,
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
    #[error("{0}")]
    InvalidModel(String),
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
            next_cursor: (start > 0).then(|| self.turns[start].cursor()),
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
        let (summary, turns) = thread_facts(&session);
        let snapshot = Arc::new(ThreadSnapshot {
            summary,
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
fn thread_facts(session: &SessionData) -> (ThreadSummary, Vec<IndexedTurn>) {
    let turns = index_turn_history(session.entries());
    (summarize_thread(session, &turns), turns)
}

/// 归档会话的子目录（相对 sessions_dir）。删除改成归档保留；列表和摘要的扫描
/// 只读顶层的 .jsonl，因此天然跳过 archived/——这是列表过滤所依赖的前提，改动
/// 扫描方式时必须复核。
pub const ARCHIVED_SESSIONS_DIR_NAME: &str = "archived";

/// 持有会话写者锁，将文件移入归档目录；历史内容留到打开时解析。
impl ThreadCatalog {
    pub fn archive(&self, thread_id: &str) -> Result<(), CatalogError> {
        Uuid::parse_str(thread_id).map_err(|error| {
            self.session_error(thread_id, SessionError::InvalidSession(error.to_string()))
        })?;
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
        let _writer = self
            .coordinator
            .acquire(thread_id)
            .map_err(|error| self.session_error(thread_id, error))?;
        std::fs::rename(&path, &archived)
            .map_err(|error| CatalogError::session(thread_id, &path, error.into()))?;
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
