//! 线性 JSONL Session 子系统的稳定 façade。
//!
//! SessionManager 持锁拥有写入能力，SessionData 提供共同的只读事实；公开合同由本模块
//! 重新导出，而 format/file/context/repair/operation 子模块承载各自的 schema、
//! I/O、上下文、恢复与归约接缝，projection 派生会话摘要。客户端只依赖这里的 façade。

pub(crate) mod context;
mod file;
mod format;
mod manager;
mod operation;
mod projection;
mod repair;
mod request;
mod writer_lock;

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

pub use context::ContextView;
pub use format::{
    CURRENT_SESSION_VERSION, CompactionEntry, ControlChannel, ControlDisposition, ControlRequest,
    LedgerRecord, OperationKind, Result, SessionEntry, SessionError, SessionMetadata, control_id,
    tool_item_id, turn_usage_from_model_usage,
};
pub use manager::{SessionAccess, SessionData, SessionManager};
pub use operation::{
    OperationState, UnresolvedTool, open_operations, reduce_controls, reduce_operations,
};
pub use projection::project_session;
pub use repair::REPAIR_UNKNOWN_OUTCOME;
pub use request::RequestContext;
pub use writer_lock::{WriterLockCoordinator, WriterLockGuard};

/// 单个 turn 的共享会话写者：turn 执行与控制面共用同一
/// SessionManager 实例（单一写者所有权），各操作短暂加锁串行追加，
/// 绝不跨 provider/工具调用持锁。控制接受与执行追加经同一实例落盘，
/// 不存在绕过 SessionManager 的第二写者。
pub type SessionWriter = std::sync::Arc<std::sync::Mutex<SessionManager>>;

/// 加锁取回会话写者；Mutex 中毒 = 共享会话状态损坏 → fail-stop，
/// 不静默恢复（与 inbox::lock_inbox 同一纪律）。
#[allow(clippy::expect_used)]
pub fn lock_writer(writer: &SessionWriter) -> std::sync::MutexGuard<'_, SessionManager> {
    writer
        .lock()
        .expect("session writer lock poisoned (fail-stop)")
}

#[cfg(test)]
pub(crate) use file::AppendLimits;

#[cfg(test)]
mod tests;
