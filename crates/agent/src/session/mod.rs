//! 线性 JSONL 会话子系统的稳定入口。
//!
//! SessionManager 由执行入口交接写入所有权；SessionData 是读写双方共用的只读事实。
//! schema、文件读写与模型上下文投影分别由 format/file/context 子模块承担。

pub(crate) mod context;
mod file;
mod format;
mod manager;
mod request;

pub use context::ContextView;
pub use format::{
    CURRENT_SESSION_VERSION, CompactionEntry, LedgerRecord, Result, SessionEntry, SessionError,
    SessionMetadata, text_item_id, thinking_item_id, tool_item_id, turn_usage_from_model_usage,
};
pub use manager::{SessionData, SessionManager};
pub use request::RequestContext;
pub(crate) use request::RequestDefinitions;

/// 会话 JSONL 文件名的唯一拼法，创建、查找与归档共用。
pub fn session_file_name(session_id: &str) -> String {
    format!("{session_id}.jsonl")
}

pub(crate) fn new_entry_id() -> String {
    uuid::Uuid::now_v7().to_string()
}

/// 一个 turn 内共享的会话写者。turn 执行与控制面用同一个 SessionManager 实例，因此
/// 写者只有一份；每次追加各自短暂加锁、串行落盘，绝不跨 provider 调用或工具执行持锁。
pub type SessionWriter = std::sync::Arc<std::sync::Mutex<SessionManager>>;

/// 加锁取回会话写者。锁中毒意味着共享会话状态已经损坏，直接 panic 停止，
/// 不做静默恢复（与 inbox::lock_inbox 同一纪律）。
pub fn lock_writer(writer: &SessionWriter) -> std::sync::MutexGuard<'_, SessionManager> {
    writer
        .lock()
        .expect("session writer lock poisoned (fail-stop)")
}

/// 在线程池追加一条持久记录；调用方 await 成功后才能发布依赖它的事件。
pub async fn append_record_async(writer: &SessionWriter, record: LedgerRecord) -> Result<String> {
    with_writer_async(writer, move |writer| writer.append_record(record)).await
}

/// 在线程池中操作共享写者；持久化错误向上传播，内部异常由进程统一终止。
pub async fn with_writer_async<T: Send + 'static>(
    writer: &SessionWriter,
    operation: impl FnOnce(&mut SessionManager) -> Result<T> + Send + 'static,
) -> Result<T> {
    let writer = std::sync::Arc::clone(writer);
    tokio::task::spawn_blocking(move || operation(&mut lock_writer(&writer)))
        .await
        .expect("session writer completes while the runtime is running")
}
