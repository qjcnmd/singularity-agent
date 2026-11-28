pub(crate) mod compaction;
pub(crate) mod context;
mod file;
mod format;
mod manager;

use format::Result;

pub use crate::image::SavedImage;
pub use format::{
    CompactionEntry, LedgerRecord, MessageDisplay, SessionEntry, SessionError, SessionMetadata,
    is_streaming_item_of,
};
pub(crate) use format::{text_item_id, thinking_item_id, tool_item_id};
pub use manager::{SessionData, SessionManager, scan_session_entries};

pub fn session_file_name(session_id: &str) -> String {
    format!("{session_id}.jsonl")
}

pub fn session_images_directory(sessions_dir: &std::path::Path, session_id: &str) -> std::path::PathBuf {
    sessions_dir.join("images").join(session_id)
}

pub fn session_outputs_directory(sessions_dir: &std::path::Path, session_id: &str) -> std::path::PathBuf {
    sessions_dir.join("outputs").join(session_id)
}

pub(crate) fn new_entry_id() -> String {
    uuid::Uuid::now_v7().to_string()
}

pub type SessionWriter = std::sync::Arc<std::sync::Mutex<SessionManager>>;

pub fn lock_writer(writer: &SessionWriter) -> std::sync::MutexGuard<'_, SessionManager> {
    writer.lock().expect("session writer lock poisoned")
}

pub(crate) fn with_writer<R>(writer: &SessionWriter, operation: impl FnOnce(&mut SessionManager) -> R) -> R {
    operation(&mut lock_writer(writer))
}

pub async fn with_writer_async<T: Send + 'static>(
    writer: &SessionWriter,
    operation: impl FnOnce(&mut SessionManager) -> Result<T> + Send + 'static,
) -> Result<T> {
    let writer = std::sync::Arc::clone(writer);
    tokio::task::spawn_blocking(move || with_writer(&writer, operation))
        .await
        .expect("session writer completes while the runtime is running")
}
