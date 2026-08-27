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
pub mod store;

mod history;

pub use conversation::{
    Conversation, ConversationError, ReasoningPatch, SettingsApplyTiming, SettingsPatch,
    TurnControls, TurnReservation, compose_merged_selector,
};
pub use error::{
    ProviderFailureKind, TurnFailure, TurnFailureCause, TurnFailureStage, TurnRunError,
};
pub use events::{TurnEvent, TurnEventSink};
pub use objects::{ProviderStatus, Thread, ThreadStatus, Turn, TurnStatus, TurnUsage};
pub use runner::{TurnOutcome, TurnParams, TurnRunner};
pub use singularity_agent::compaction::CompactionOutcome;
pub use singularity_agent::tools::bash::ensure_available as ensure_bash_available;
pub use singularity_core::{
    create_owner_only_dir, ensure_singularity_home_outside_workspace, user_singularity_home,
};
pub use singularity_model::{
    ModelSelectorParts, Provider, ProviderConfigSnapshot, split_model_selector,
};
pub use store::{
    ResumeError, ThreadLockCoordinator, ThreadReadPage, ThreadSummary, canonical_thread_cwd,
    create_thread, delete_thread, list_threads, paged_read, read_thread_summary, rename_thread,
    resume_thread, thread_session_path,
};

/// 测试支撑面：供依赖 crate 的集成测试构造会话、注入 provider 与复用核心
/// 常量，不进入生产依赖图（`test-support` feature）。
#[cfg(feature = "test-support")]
pub mod test_support {
    pub use singularity_agent::message::{AgentMessage, AgentMessageRole, ContentBlock};
    pub use singularity_agent::session::{
        SessionManager, SessionMetadata, SessionMetadataKind, TurnTerminalStatus,
    };
    pub use singularity_core::{
        CancellationToken, PROJECT_INSTRUCTIONS_MAX_FILE_BYTES,
        ensure_singularity_home_outside_workspace, find_workspace_root,
    };
    pub use singularity_model::{
        ModelError, ModelErrorKind, ModelTurnRequest, ModelTurnResponse, ModelTurnStatus, Provider,
        ProviderConfigSnapshot, ProviderError, ProviderProtocolContract, ProviderReasoningReplay,
    };
}

#[cfg(test)]
#[path = "../tests/conversation_tests.rs"]
mod conversation_tests;
