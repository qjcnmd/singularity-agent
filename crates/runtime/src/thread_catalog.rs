//! Thread 的目录操作：创建、定位、只读分页投影与归档。
//!
//! JSONL 会话文件是唯一的持久事实源；这里只提供路径和打开会话的统一
//! 入口，不复制会话状态。ThreadCatalog 持有 sessions_dir；目录
//! 布局与路径函数留在本模块；crate 根导出目录名。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use singularity_agent::session::{SessionData, SessionError, SessionManager, SessionMetadata};
use singularity_model::parse_model_selector;
use singularity_protocol::{ThreadReadPage, ThreadSummary};
use uuid::Uuid;

use crate::history::{IndexedTurn, index_turn_history, summarize_thread};
use singularity_protocol::Thread;

/// 数据目录内保存会话文件的子目录名。
pub const SESSIONS_DIR_NAME: &str = "sessions";

/// Thread 目录操作与只读投影的统一入口。
pub struct ThreadCatalog {
    sessions_dir: PathBuf,
}

impl ThreadCatalog {
    /// 构造目录入口；会话写入窗口由调用方管理。
    pub fn new(sessions_dir: PathBuf) -> Self {
        Self { sessions_dir }
    }
}

/// Thread 会话文件的规范位置。
pub fn thread_session_path(sessions_dir: &Path, thread_id: &str) -> PathBuf {
    sessions_dir.join(singularity_agent::session::session_file_name(thread_id))
}

impl ThreadCatalog {
    /// 创建 uuid v7 会话文件并保存模型选择；cwd 由会话层归一为绝对路径。
    /// 初始化设置写入失败时清理新文件，清理失败一并报告。
    pub fn create_thread(&self, cwd: &str, model: Option<String>) -> Result<Thread, CatalogError> {
        if let Some(selector) = model.as_deref() {
            parse_model_selector(selector)
                .map_err(|error| CatalogError::InvalidModel(error.to_string()))?;
        }
        let thread_id = Uuid::now_v7().to_string();
        let mut session =
            SessionManager::create_with_id(Path::new(cwd), &self.sessions_dir, &thread_id)
                .map_err(|error| self.session_error(&thread_id, error))?;
        let thread = Thread {
            thread_id,
            cwd: session.cwd_string(),
            model,
        };
        if let Err(creation) = record_thread_settings_metadata(&mut session, &thread) {
            // 新会话尚未交付；释放写者后清理本次创建的文件，避免失败任务留在列表中。
            drop(session);
            let path = thread_session_path(&self.sessions_dir, &thread.thread_id);
            return Err(match std::fs::remove_file(&path) {
                Ok(()) => self.session_error(&thread.thread_id, creation),
                Err(cleanup) => CatalogError::Io {
                    path,
                    source: std::io::Error::new(
                        cleanup.kind(),
                        format!(
                            "initial thread settings could not be saved: {creation}; failed to remove the new session: {cleanup}"
                        ),
                    ),
                },
            });
        }
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
            reasoning: parts.reasoning_variant.map(str::to_string),
        })
        .map(|_| ())
}

impl ThreadCatalog {
    /// 从已保存的历史读取任务目录与模型选择，不修改会话。
    pub fn resume_thread(&self, thread_id: &str) -> Result<Thread, CatalogError> {
        let session = open_thread_read_only(&self.sessions_dir, thread_id)?;
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
        Ok(Thread {
            thread_id: thread_id.to_string(),
            cwd: session.cwd_string(),
            model,
        })
    }
}

impl ThreadCatalog {
    /// 按最近更新时间列出任务；忽略未完成尾行和枚举后已移除的文件，其他读取错误直接返回。
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
            let summary = self.read_summary(thread_id);
            match summary {
                Ok(summary) => threads.push(summary),
                // 目录项还在但文件已经不在：这是确认过的移除，不是读失败。
                Err(CatalogError::NotFound(_)) => {}
                Err(error) => return Err(error),
            }
        }
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

impl ThreadCatalog {
    /// 从会话事实投影任务摘要。
    fn read_summary(&self, thread_id: &str) -> Result<ThreadSummary, CatalogError> {
        let session = open_thread_read_only(&self.sessions_dir, thread_id)?;
        let (summary, _) = thread_facts(&session);
        Ok(summary)
    }
}

impl ThreadCatalog {
    /// 去掉名称首尾空白后追加任务名称；调用方持有该会话的写入窗口。
    pub fn rename(&self, thread_id: &str, name: &str) -> Result<(), CatalogError> {
        let name = name.trim();
        if name.is_empty() {
            return Err(CatalogError::InvalidName);
        }
        let path = thread_session_path(&self.sessions_dir, thread_id);
        let mut session = SessionManager::open_existing(&path, thread_id)
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

    /// 读取不可变历史快照；活动回合可持有它作为历史与增量事件的共同边界。
    pub fn read_snapshot(&self, thread_id: &str) -> Result<Arc<ThreadSnapshot>, CatalogError> {
        let session = open_thread_read_only(&self.sessions_dir, thread_id)?;
        let (summary, turns) = thread_facts(&session);
        Ok(Arc::new(ThreadSnapshot {
            summary,
            session,
            turns,
        }))
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

impl ThreadCatalog {
    /// 在调用方持有的会话写入窗口内将文件移入归档目录。
    /// 保留文件内容，归档任务不再出现在活动目录列表中。
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
        std::fs::rename(&path, &archived)
            .map_err(|error| CatalogError::session(thread_id, &path, error.into()))?;
        Ok(())
    }
}
