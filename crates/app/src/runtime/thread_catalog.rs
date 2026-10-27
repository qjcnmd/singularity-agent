//! 会话文件的创建、打开、列出、读取历史和删除。
//!
//! 会话内容保存在各自的 JSONL 日志文件中，本模块负责定位和读写这些文件。

use std::path::{Path, PathBuf};

use singularity_agent::session::{SessionData, SessionError, SessionManager, session_images_directory};
use singularity_protocol::{ThreadHistory, ThreadSummary};
use uuid::Uuid;

use crate::runtime::history::{index_turn_history, session_usage, summarize_thread};

/// 数据目录内保存会话文件的子目录名。
pub const SESSIONS_DIR_NAME: &str = "sessions";

/// `sessions/` 目录下的会话文件；不持有写者，写入由打开它的 Conversation 负责。
pub struct ThreadCatalog {
    sessions_dir: PathBuf,
}

impl ThreadCatalog {
    pub fn new(sessions_dir: PathBuf) -> Self {
        Self { sessions_dir }
    }

    /// 用 UUID v7 新建会话文件，写入文件头和初始模型选择；文件已存在时报错。
    pub fn create_thread(
        &self,
        cwd: &str,
        model: singularity_protocol::ModelSelection,
    ) -> Result<SessionManager, CatalogError> {
        let thread_id = Uuid::now_v7().to_string();
        SessionManager::create_with_id(Path::new(cwd), &self.sessions_dir, &thread_id, model)
            .map_err(|error| self.session_error(&thread_id, error))
    }

    /// 以写入方式打开日志并截掉未写完的末尾；返回的写者交给 Conversation 长期持有。
    pub fn open_session(&self, thread_id: &str) -> Result<SessionManager, CatalogError> {
        let path = thread_session_path(&self.sessions_dir, thread_id);
        SessionManager::open_existing(&path, thread_id).map_err(|error| self.session_error(thread_id, error))
    }

    /// 按更新时间列出当前会话；扫描期间已删除的文件不再列入。
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
                Ok(session) => threads.push(summarize_thread(&session)),
                Err(CatalogError::NotFound(_)) => {}
                Err(error) => return Err(error),
            }
        }
        // 按最近更新时间从新到旧排列，时间相同时按任务 ID 排序。
        threads.sort_by(|left, right| {
            right.updated_at.cmp(&left.updated_at).then_with(|| left.thread_id.cmp(&right.thread_id))
        });
        Ok(threads)
    }

    /// 读取未打开任务的图片，只接受日志里实际引用的图片 ID。
    pub fn image_data(&self, thread_id: &str, image_id: &str) -> Result<String, CatalogError> {
        // 日志能打开时，后面的失败归为图片读取失败，不报告为任务不存在。
        open_thread_read_only(&self.sessions_dir, thread_id)?
            .image_data(image_id)
            .map_err(|source| CatalogError::Session {
                path: thread_session_path(&self.sessions_dir, thread_id),
                source,
            })
    }

    /// 停止任务后删除日志和附件；附件删除失败时保留日志，便于重试。
    pub fn delete(&self, thread_id: &str) -> Result<(), CatalogError> {
        Uuid::parse_str(thread_id).map_err(|error| {
            self.session_error(thread_id, SessionError::InvalidSession(error.to_string()))
        })?;
        let path = thread_session_path(&self.sessions_dir, thread_id);
        let images = session_images_directory(&self.sessions_dir, thread_id);
        match std::fs::remove_dir_all(&images) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => return Err(CatalogError::Io { path: images, source }),
        }
        std::fs::remove_file(&path).map_err(|error| CatalogError::session(thread_id, &path, error.into()))
    }

    fn session_error(&self, thread_id: &str, source: SessionError) -> CatalogError {
        CatalogError::session(thread_id, &thread_session_path(&self.sessions_dir, thread_id), source)
    }
}

/// 任务对应的会话文件路径。
pub fn thread_session_path(sessions_dir: &Path, thread_id: &str) -> PathBuf {
    sessions_dir.join(singularity_agent::session::session_file_name(thread_id))
}

fn open_thread_read_only(sessions_dir: &Path, thread_id: &str) -> Result<SessionData, CatalogError> {
    let path = thread_session_path(sessions_dir, thread_id);
    let session = SessionData::open(&path).map_err(|error| CatalogError::session(thread_id, &path, error))?;
    session
        .verify_session_id(thread_id)
        .map_err(|error| CatalogError::session(thread_id, &path, error))?;
    Ok(session)
}

/// 查找、打开或删除会话文件时的错误。
#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    #[error("thread {0} was not found")]
    NotFound(String),
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

/// 从会话日志生成完整历史和任务摘要。
pub fn read_thread_history(session: &SessionData) -> Result<ThreadHistory, CatalogError> {
    let summary = summarize_thread(session);
    let indexed_turns = index_turn_history(session.entries());
    let mut usage =
        session_usage(&session.entries()[..indexed_turns.first().map_or(0, |turn| turn.entries.start)]);
    let turns = indexed_turns
        .iter()
        .map(|turn| turn.project(session, &mut usage))
        .collect::<Result<_, _>>()
        .map_err(|error| CatalogError::session(&summary.thread_id, session.path(), error))?;
    Ok(ThreadHistory { summary, turns })
}
