#![forbid(unsafe_code)]

//! Thread/Turn 生命周期协调与进程内 turn 执行管线。
//!
//! runtime 是无交互入口（`--print`/`--json`）与交互式 TUI 共享的唯一执行层：
//! [`TurnRunner`] 负责单个 turn 的完整生命周期（会话打开/修复、项目指令装配、
//! Agent 执行、事件投影、终态落盘），[`Conversation`] 在其上维护一个 Thread 的
//! 长驻状态：单活动 turn 不变量、steer/followUp 注入、取消、设置生效时序。
//!
//! 职责边界：
//! - Context/Compaction 保留在 `singularity_agent::compaction`；
//! - 工具保留在 `singularity_agent::tools`（[`singularity_agent::tools::ToolRegistry`]）;
//! - Provider 选择与请求保留在 `singularity_model`（`dyn Provider` 即模型接缝）；
//! - 会话 JSONL 持久化保留在 `singularity_agent::session`；
//! - 协议 wire 与 JSON-RPC 属于 app-server 适配器；runtime 只依赖 protocol 的
//!   公开历史类型（`HistoryItem`/`ThreadTurn`），JSON-RPC 接线仍在 app-server。
//!
//! 事件事实源：[`TurnEvent`]。文本渲染、JSONL 输出、TUI 与协议投影各自消费
//! 同一枚举，任何一方的失败只影响自身投影。

pub mod conversation;
pub mod error;
pub mod events;
pub mod objects;
pub mod runner;
pub mod thread_catalog;

mod assistant_items;
mod history;
mod store;
mod terminal;

pub use conversation::{
    Conversation, ConversationError, ReasoningPatch, SettingsApplyTiming, SettingsPatch,
    TurnControls, TurnReservation,
};
pub use error::{TurnFailure, TurnFailureCause, TurnFailureStage, TurnRunError};
pub use events::{TurnEvent, TurnEventSink};
pub use objects::{ProviderConfigurationStatus, Thread, Turn, TurnModelUsage, TurnStatus};
pub use runner::{TurnOutcome, TurnParams, TurnRunner};
pub use singularity_agent::compaction::CompactionOutcome;
pub use singularity_agent::tools::bash::ensure_available as ensure_bash_available;
pub use singularity_core::{
    create_owner_only_dir, ensure_singularity_home_outside_workspace, user_singularity_home,
};
pub use singularity_model::{
    ModelSelectorParts, Provider, ProviderConfigSnapshot, split_model_selector,
};
pub use singularity_protocol::{HistoryItem, ThreadTurn};
pub use store::{
    ResumeError, SESSIONS_DIR_NAME, ThreadReadPage, ThreadSummary, canonical_thread_cwd,
    prepare_session_dirs, thread_session_path,
};
pub use thread_catalog::ThreadCatalog;

#[cfg(test)]
#[path = "../tests/conversation_tests.rs"]
mod conversation_tests;
