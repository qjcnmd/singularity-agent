//! 线性 JSONL 会话子系统的稳定入口。
//!
//! SessionManager 由执行入口交接写入所有权；SessionData 是读写双方共用的只读事实。
//! schema、文件读写与模型上下文投影分别由 format/file/context 子模块承担。

pub(crate) mod context;
mod file;
mod format;
mod manager;
mod request;

pub use format::{
    CURRENT_SESSION_VERSION, CompactionEntry, LedgerRecord, Result, SessionEntry, SessionError,
    SessionMetadata, text_item_id, thinking_item_id, tool_item_id, turn_usage_from_model_usage,
};
pub use manager::{CommittedEntry, SessionData, SessionManager};
pub use request::RequestContext;
pub(crate) use request::RequestDefinitions;

/// 生成会话 JSONL 文件名：`{session_id}.jsonl`。
pub fn session_file_name(session_id: &str) -> String {
    format!("{session_id}.jsonl")
}

pub(crate) fn new_entry_id() -> String {
    uuid::Uuid::now_v7().to_string()
}

/// 线程安全的共享会话写入器（仅在读取或追加时短暂加锁）。
pub type SessionWriter = std::sync::Arc<std::sync::Mutex<SessionManager>>;

/// 获取写者互斥锁（锁中毒时直接 fail-stop panic）。
pub fn lock_writer(writer: &SessionWriter) -> std::sync::MutexGuard<'_, SessionManager> {
    writer.lock().expect("session writer lock poisoned (fail-stop)")
}

/// 在阻塞任务线程池中异步追加一条账本记录。
pub async fn append_record_async(writer: &SessionWriter, record: LedgerRecord) -> Result<CommittedEntry> {
    with_writer_async(writer, move |writer| writer.append_record(record)).await
}

/// 在阻塞任务线程池中操作共享会话写者。
pub async fn with_writer_async<T: Send + 'static>(
    writer: &SessionWriter,
    operation: impl FnOnce(&mut SessionManager) -> Result<T> + Send + 'static,
) -> Result<T> {
    let writer = std::sync::Arc::clone(writer);
    tokio::task::spawn_blocking(move || operation(&mut lock_writer(&writer)))
        .await
        .expect("session writer completes while the runtime is running")
}
