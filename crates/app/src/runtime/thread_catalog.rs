use std::path::{Path, PathBuf};

use agent::session::{
    SessionData, SessionError, SessionManager, scan_session_entries, session_images_directory,
    session_outputs_directory,
};
use protocol::ThreadSummary;
use uuid::Uuid;

use crate::runtime::history::ThreadSummaryScan;

pub(crate) const SESSIONS_DIR_NAME: &str = "sessions";

pub(crate) struct ThreadCatalog {
    sessions_dir: PathBuf,
}

impl ThreadCatalog {
    pub(crate) fn new(sessions_dir: PathBuf) -> Self {
        Self { sessions_dir }
    }

    pub(crate) fn create_thread(
        &self,
        cwd: &str,
        model: protocol::ModelSelection,
        disabled_modules: Vec<String>,
    ) -> Result<SessionManager, CatalogError> {
        let thread_id = Uuid::now_v7();
        SessionManager::create_with_id(Path::new(cwd), &self.sessions_dir, thread_id, model, disabled_modules)
            .map_err(|error| self.session_error(&thread_id.to_string(), error))
    }

    pub(crate) fn open_session(&self, thread_id: &str) -> Result<SessionManager, CatalogError> {
        let path = checked_session_path(&self.sessions_dir, thread_id)?;
        SessionManager::open_existing(&path, thread_id).map_err(|error| self.session_error(thread_id, error))
    }

    pub(crate) fn list_threads(&self) -> Result<Vec<ThreadSummary>, CatalogError> {
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
            let path = checked_session_path(&self.sessions_dir, thread_id)?;
            let mut summary = ThreadSummaryScan::default();
            match scan_session_entries(&path, thread_id, |entry| summary.observe(&entry))
                .map_err(|error| CatalogError::session(thread_id, &path, error))
            {
                Ok((cwd, created_at)) => {
                    threads.push(summary.finish(thread_id, app_core::display_path(&cwd), created_at));
                }
                Err(CatalogError::NotFound(_)) => {}
                Err(error) => return Err(error),
            }
        }
        threads.sort_by(|left, right| {
            right.updated_at.cmp(&left.updated_at).then_with(|| left.thread_id.cmp(&right.thread_id))
        });
        Ok(threads)
    }

    pub(crate) fn image_data(&self, thread_id: &str, image_id: &str) -> Result<String, CatalogError> {
        open_thread_read_only(&self.sessions_dir, thread_id)?
            .image_data(image_id)
            .map_err(|source| CatalogError::Session {
                path: thread_session_path(&self.sessions_dir, thread_id),
                source,
            })
    }

    pub(crate) fn delete(&self, thread_id: &str) -> Result<(), CatalogError> {
        let path = checked_session_path(&self.sessions_dir, thread_id)?;
        for directory in [
            session_images_directory(&self.sessions_dir, thread_id),
            session_outputs_directory(&self.sessions_dir, thread_id),
        ] {
            if let Err(source) = remove_dir_ignoring_not_found(&directory) {
                return Err(CatalogError::Io { path: directory, source });
            }
        }
        std::fs::remove_file(&path).map_err(|error| CatalogError::session(thread_id, &path, error.into()))
    }

    fn session_error(&self, thread_id: &str, source: SessionError) -> CatalogError {
        CatalogError::session(thread_id, &thread_session_path(&self.sessions_dir, thread_id), source)
    }
}

fn remove_dir_ignoring_not_found(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn thread_session_path(sessions_dir: &Path, thread_id: &str) -> PathBuf {
    sessions_dir.join(agent::session::session_file_name(thread_id))
}

fn checked_session_path(sessions_dir: &Path, thread_id: &str) -> Result<PathBuf, CatalogError> {
    let path = thread_session_path(sessions_dir, thread_id);
    let parsed = Uuid::parse_str(thread_id).map_err(|error| {
        CatalogError::session(thread_id, &path, SessionError::InvalidSession(error.to_string()))
    })?;
    if parsed.to_string() != thread_id {
        return Err(CatalogError::session(
            thread_id,
            &path,
            SessionError::InvalidSession(format!(
                "session id must be a canonical lowercase UUID: {thread_id}"
            )),
        ));
    }
    Ok(path)
}

fn open_thread_read_only(sessions_dir: &Path, thread_id: &str) -> Result<SessionData, CatalogError> {
    let path = checked_session_path(sessions_dir, thread_id)?;
    SessionData::open(&path, Some(thread_id)).map_err(|error| CatalogError::session(thread_id, &path, error))
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum CatalogError {
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
