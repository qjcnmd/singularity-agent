#![forbid(unsafe_code)]

//! Thread/Turn 的生命周期协调与进程内 turn 执行管线。
//!
//! runtime 是评估入口（--json）和 Web 工作台共用的唯一执行层：TurnRunner 负责单个 turn 的
//! 完整生命周期；Conversation 在它之上维护一个 Thread 的长驻状态：单活动 turn 的不变量、
//! steer/followUp 注入、取消，以及设置何时生效。
//!
//! 职责边界：
//! - 上下文与压缩留在 singularity_agent::compaction；
//! - 工具留在 singularity_agent::tools（singularity_agent::tools::ToolRegistrySnapshot）；
//! - Provider 的选择与请求留在 singularity_model（dyn Provider 就是模型接缝）；
//! - 会话的 JSONL 持久化留在 singularity_agent::session；
//! - 协议层提供事件与公共对象这些共享类型；文本渲染、JSONL 输出和序列化
//!   由各个客户端自己完成。
//!
//! 事件的事实源是 singularity_protocol::TurnEvent：文本渲染和 JSONL 输出各自
//! 消费同一个枚举，任何一方失败都只影响自己的投影。

mod conversation;
mod error;
mod runner;
mod workspace_store;

mod assistant_items;
mod history;
mod thread_catalog;

pub use conversation::{
    Conversation, ConversationControlError, ConversationError, ConversationSnapshot,
    FollowUpPromotion, TurnReservation,
};
pub use error::{TurnFailureCause, TurnRunError};
pub use runner::{CompactionOutcome, CompactionRunError, TurnOutcome, TurnRunner};
/// 进程内的写者协调器：装配入口创建一次，交给 TurnRunner 和 ThreadCatalog 共用。
pub use singularity_agent::session::WriterLockCoordinator;
pub use singularity_agent::tools::bash::ensure_available as ensure_bash_available;
pub use thread_catalog::{
    CatalogError, SESSIONS_DIR_NAME, ThreadCatalog, ThreadSnapshot, prepare_session_dirs,
};
pub use workspace_store::{WorkspaceError, WorkspaceStore};

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

#[cfg(test)]
mod tests;
