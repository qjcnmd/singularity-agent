#![forbid(unsafe_code)]

//! Thread/Turn 生命周期协调与进程内 turn 执行管线。
//!
//! runtime 是评估入口（--json）与 Web 工作台共享的唯一执行层：
//! TurnRunner 负责单个 turn 的完整生命周期（会话打开/修复、项目指令装配、
//! Agent 执行、事件投影、终态落盘），Conversation 在其上维护一个 Thread 的
//! 长驻状态：单活动 turn 不变量、steer/followUp 注入、取消、设置生效时序。
//!
//! 职责边界：
//! - Context/Compaction 保留在 singularity_agent::compaction；
//! - 工具保留在 singularity_agent::tools（singularity_agent::tools::ToolRegistrySnapshot）;
//! - Provider 选择与请求保留在 singularity_model（dyn Provider 即模型接缝）；
//! - 会话 JSONL 持久化保留在 singularity_agent::session；
//! - 协议层提供事件与公共对象的共享类型，runtime 经 events（事件出口）与
//!   objects（公开对象）两条接缝模块暴露给客户端；文本渲染、JSONL
//!   输出与序列化由各客户端完成。
//!
//! 事件事实源：events::TurnEvent。文本渲染与 JSONL 输出各自消费同一
//! 枚举，任何一方的失败只影响自身投影。

mod conversation;
mod error;
pub mod events;
pub mod objects;
mod runner;
mod workspace_store;

mod assistant_items;
mod history;
mod store;
mod terminal;

// 根导出只列有命名消费者的条目（cli 与集成测试经根路径或接缝模块实际引用）；
// 事件类型走 events，公开对象类型走 objects，各自单一访问路径。
pub use conversation::{
    Conversation, ConversationControlError, ConversationError, FollowUpPromotion, TurnReservation,
};
pub use error::{TurnFailureCause, TurnRunError};
pub use runner::{CompactionRunError, TurnOutcome, TurnRunner};
pub use singularity_agent::tools::bash::ensure_available as ensure_bash_available;
pub use store::{
    CatalogError, SESSIONS_DIR_NAME, ThreadCatalog, ThreadSnapshot, prepare_session_dirs,
};
pub use workspace_store::{WORKBENCH_FILE_NAME, WorkspaceError, WorkspaceStore};

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

#[cfg(test)]
mod tests;
