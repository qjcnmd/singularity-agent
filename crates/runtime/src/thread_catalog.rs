//! Thread 的目录操作：创建、定位、只读分页投影与归档。
//!
//! JSONL 会话文件是持久事实的落点，本模块提供路径和打开会话的入口，不复制会话状态。
//! 目录布局与路径函数留在本模块，目录名在 crate 根导出。

use std::path::{Path, PathBuf};

use singularity_agent::session::{SessionData, SessionError, SessionManager, SessionMetadata};
use singularity_model::parse_model_selector;
use singularity_protocol::{ThreadReadPage, ThreadSummary};
use uuid::Uuid;

use crate::history::{IndexedTurn, index_turn_history, summarize_thread};

/// 数据目录内保存会话文件的子目录名。
pub const SESSIONS_DIR_NAME: &str = "sessions";

/// 归档会话的子目录（相对 sessions_dir）。删除改成归档保留；列表和摘要只扫描顶层的
/// .jsonl，archived/ 下的文件不会进结果，列表过滤依赖这一点，改动扫描方式时要复核。
pub const ARCHIVED_SESSIONS_DIR_NAME: &str = "archived";

/// Thread 目录操作与只读投影的统一入口。
pub struct ThreadCatalog {
    sessions_dir: PathBuf,
}

impl ThreadCatalog {
    /// 构造目录入口；会话写入窗口由调用方管理。
    pub fn new(sessions_dir: PathBuf) -> Self {
        Self { sessions_dir }
    }

    /// 创建 uuid v7 会话文件并保存模型选择；cwd 由会话层归一为绝对路径。
    /// 初始化设置写入失败时清理新文件，清理失败一并报告。
    pub fn create_thread(&self, cwd: &str, model: Option<String>) -> Result<SessionManager, CatalogError> {
        if let Some(selector) = model.as_deref() {
            parse_model_selector(selector).map_err(|error| CatalogError::InvalidModel(error.to_string()))?;
        }
        let thread_id = Uuid::now_v7().to_string();
        let mut session = SessionManager::create_with_id(Path::new(cwd), &self.sessions_dir, &thread_id)
            .map_err(|error| self.session_error(&thread_id, error))?;
        if let Some(selector) = model.as_deref()
            && let Err(creation) = record_thread_settings_metadata(&mut session, selector)
        {
            // 新会话尚未交付；释放写者后清理本次创建的文件，不把失败任务留在列表中。
            drop(session);
            let path = thread_session_path(&self.sessions_dir, &thread_id);
            return Err(match std::fs::remove_file(&path) {
                Ok(()) => self.session_error(&thread_id, creation),
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
        Ok(session)
    }

    /// 打开并修复日志尾部，写者随后交给 Conversation 长期持有。
    pub fn open_session(&self, thread_id: &str) -> Result<SessionManager, CatalogError> {
        let path = thread_session_path(&self.sessions_dir, thread_id);
        SessionManager::open_existing(&path, thread_id).map_err(|error| self.session_error(thread_id, error))
    }

    /// 按更新时间列出当前会话；扫描期间已归档的文件不再列入。
    pub fn list_threads(&self) -> Result<Vec<ThreadSummary>, CatalogError> {
        let entries = match std::fs::read_dir(&self.sessions_dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => {
                return Err(CatalogError::Io { path: self.sessions_dir.clone(), source });
            }
        };
        let mut threads = Vec::new();
        for entry in entries {
            let entry =
                entry.map_err(|source| CatalogError::Io { path: self.sessions_dir.clone(), source })?;
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("jsonl") {
                continue;
            }
            let Some(thread_id) = path.file_stem().and_then(|value| value.to_str()) else {
                continue;
            };
            match open_thread_read_only(&self.sessions_dir, thread_id) {
                Ok(session) => {
                    let (summary, _) = thread_facts(&session);
                    threads.push(summary);
                }
                Err(CatalogError::NotFound(_)) => {}
                Err(error) => return Err(error),
            }
        }
        // 目录顺序只在本模块产生：按最近更新时间降序，时间相同时按任务 ID 升序。
        threads.sort_by(|left, right| {
            right.updated_at.cmp(&left.updated_at).then_with(|| left.thread_id.cmp(&right.thread_id))
        });
        Ok(threads)
    }

    /// 从当前持久历史读取图片，只接受任务实际引用的身份。
    pub fn image_data(&self, thread_id: &str, image_id: &str) -> Result<String, CatalogError> {
        // 会话已成功打开；后续读取失败属于图片，不再判断为任务缺失。
        open_thread_read_only(&self.sessions_dir, thread_id)?
            .image_data(image_id)
            .map_err(|source| CatalogError::Session {
                path: thread_session_path(&self.sessions_dir, thread_id),
                source,
            })
    }

    /// 调用方阻止新操作并等待活动写者释放后，将文件移入归档目录。
    /// 保留文件内容，归档任务不再出现在活动目录列表中。
    pub fn archive(&self, thread_id: &str) -> Result<(), CatalogError> {
        Uuid::parse_str(thread_id).map_err(|error| {
            self.session_error(thread_id, SessionError::InvalidSession(error.to_string()))
        })?;
        let path = thread_session_path(&self.sessions_dir, thread_id);
        let archived_dir = self.sessions_dir.join(ARCHIVED_SESSIONS_DIR_NAME);
        std::fs::create_dir_all(&archived_dir)
            .map_err(|source| CatalogError::Io { path: archived_dir.clone(), source })?;
        let archived = archived_dir.join(singularity_agent::session::session_file_name(thread_id));
        if archived
            .try_exists()
            .map_err(|source| CatalogError::Io { path: archived.clone(), source })?
        {
            return Err(CatalogError::NotFound(thread_id.to_string()));
        }
        std::fs::rename(&path, &archived)
            .map_err(|error| CatalogError::session(thread_id, &path, error.into()))?;
        Ok(())
    }

    fn session_error(&self, thread_id: &str, source: SessionError) -> CatalogError {
        CatalogError::session(thread_id, &thread_session_path(&self.sessions_dir, thread_id), source)
    }
}

/// Thread 会话文件的规范位置。
pub fn thread_session_path(sessions_dir: &Path, thread_id: &str) -> PathBuf {
    sessions_dir.join(singularity_agent::session::session_file_name(thread_id))
}

/// 在已经打开的会话写者上保存 selector；创建任务和提交设置共用这个入口。
/// 创建或变更任务时追加一次已校验的选择。
pub(crate) fn record_thread_settings_metadata(
    session: &mut SessionManager,
    selector: &str,
) -> Result<(), SessionError> {
    let parts = parse_model_selector(selector).expect("thread model was validated before saving");
    session
        .append_metadata(SessionMetadata::ThreadSettings {
            provider: parts.provider_name.to_string(),
            model: parts.model_name.to_string(),
            reasoning: parts.reasoning_variant.map(str::to_string),
        })
        .map(|_| ())
}

fn open_thread_read_only(sessions_dir: &Path, thread_id: &str) -> Result<SessionData, CatalogError> {
    let path = thread_session_path(sessions_dir, thread_id);
    let session = SessionData::open(&path).map_err(|error| CatalogError::session(thread_id, &path, error))?;
    session
        .verify_session_id(thread_id)
        .map_err(|error| CatalogError::session(thread_id, &path, error))?;
    Ok(session)
}

/// 一次遍历同时得到摘要和分页共用的回合事实：轮数、终态和手动停止只有索引这一个来源。
fn thread_facts(session: &SessionData) -> (ThreadSummary, Vec<IndexedTurn>) {
    let turns = index_turn_history(session.entries());
    (summarize_thread(session, &turns), turns)
}

/// Thread 定位与持久化过程中的错误。
#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    #[error("thread {0} was not found")]
    NotFound(String),
    #[error("before turn cursor {0} was not found in the thread history")]
    AnchorNotFound(String),
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
            source => Self::Session { path: path.to_path_buf(), source },
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
    /// 从已加载的会话事实建立分页索引。
    pub fn new(session: SessionData) -> Self {
        let (summary, turns) = thread_facts(&session);
        Self { summary, session, turns }
    }
    /// 读取锚点之前的一页轮次：`before_turn` 是按轮的 cursor（`turn:{turnId}`，
    /// 前导组是 `turn:leading`），不是 item id；只展开请求范围内的条目。
    pub fn page(&self, limit: usize, before_turn: Option<&str>) -> Result<ThreadReadPage, CatalogError> {
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
            .collect::<Result<_, _>>()
            .map_err(|error| CatalogError::session(&self.summary.thread_id, self.session.path(), error))?;
        Ok(ThreadReadPage {
            summary: self.summary.clone(),
            turns,
            next_cursor: (start > 0).then(|| self.turns[start].cursor()),
        })
    }
}
