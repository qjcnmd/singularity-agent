//! JSONL 会话日志：每行一条记录，只追加不修改。
//!
//! SessionManager 是唯一能写日志的对象，由打开任务的 Conversation 持有；SessionData 是
//! 解析后的只读内容，列表和预览也用它。格式定义在 format，文件解析在 file，
//! 生成模型上下文在 context。

pub(crate) mod context;
mod file;
mod format;
mod manager;

use format::Result;

pub use format::{
    CompactionEntry, LedgerRecord, SessionEntry, SessionError, SessionMetadata, text_item_id,
    thinking_item_id, tool_item_id,
};
pub use manager::{SessionData, SessionManager};

/// 生成会话 JSONL 文件名：`{session_id}.jsonl`。
pub fn session_file_name(session_id: &str) -> String {
    format!("{session_id}.jsonl")
}

/// 会话图片的保存、读取和删除共用的目录。
pub fn session_images_directory(sessions_dir: &std::path::Path, session_id: &str) -> std::path::PathBuf {
    sessions_dir.join("images").join(session_id)
}

pub(crate) fn new_entry_id() -> String {
    uuid::Uuid::now_v7().to_string()
}

/// 共享的会话写者；只在读取或追加时短暂加锁。
pub type SessionWriter = std::sync::Arc<std::sync::Mutex<SessionManager>>;

/// 给会话写者加锁；锁中毒说明之前已经 panic，直接终止。
pub fn lock_writer(writer: &SessionWriter) -> std::sync::MutexGuard<'_, SessionManager> {
    writer.lock().expect("session writer lock poisoned")
}

/// 在阻塞线程池中使用会话写者，文件 IO 不占用异步运行时线程。
pub async fn with_writer_async<T: Send + 'static>(
    writer: &SessionWriter,
    operation: impl FnOnce(&mut SessionManager) -> Result<T> + Send + 'static,
) -> Result<T> {
    let writer = std::sync::Arc::clone(writer);
    tokio::task::spawn_blocking(move || operation(&mut lock_writer(&writer)))
        .await
        .expect("session writer completes while the runtime is running")
}
