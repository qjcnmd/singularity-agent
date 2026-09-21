//! 线性 JSONL 会话子系统的稳定入口。
//!
//! SessionManager 持有写者锁，是唯一的写入方；SessionData 是读写双方共用的只读事实。
//! 对外合同由本模块重新导出，schema、文件读写、上下文投影、崩溃恢复与 operation
//! 归约分别由 format/file/context/repair/operation 子模块承担。调用方只依赖这里。

pub(crate) mod context;
mod file;
mod format;
mod manager;
mod operation;
mod repair;
mod request;
mod writer_lock;

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

pub use context::ContextView;
pub use format::{
    CURRENT_SESSION_VERSION, CompactionEntry, LedgerRecord, OperationKind, Result, SessionEntry,
    SessionError, SessionMetadata, text_item_id, thinking_item_id, tool_item_id,
    turn_usage_from_model_usage,
};
pub use manager::{ExpectedSession, SessionAccess, SessionData, SessionManager};
pub use operation::{OperationState, reduce_operations};
pub use repair::REPAIR_UNKNOWN_OUTCOME;
pub use request::RequestContext;
pub use writer_lock::{WriterLockCoordinator, WriterLockGuard};

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
#[allow(clippy::expect_used)]
pub fn lock_writer(writer: &SessionWriter) -> std::sync::MutexGuard<'_, SessionManager> {
    writer
        .lock()
        .expect("session writer lock poisoned (fail-stop)")
}
