//! 线性 JSONL Session 子系统的稳定 façade。
//!
//! `SessionManager` 仍是唯一的可变生命周期 owner；其公开合同由本模块
//! 重新导出，而 format/file/context/repair/repository 子模块承载各自的
//! schema、I/O、上下文、恢复和仓储接缝。客户端只依赖这里的 façade。

mod manager;

pub mod context;
pub mod file;
pub mod format;
pub mod repair;
pub mod repository;

pub use file::now_iso;
pub use format::{
    CURRENT_SESSION_VERSION, CompactionEntry, Result, SessionEntry, SessionEntryType, SessionError,
    SessionMetadata, SessionMetadataKind,
};
pub use manager::SessionManager;
pub use repository::{SessionRepository, ThreadRead};

#[cfg(test)]
pub(crate) use file::{AppendLimits, normalize_cwd_string};

#[cfg(test)]
use crate::message::{AgentMessage, AgentMessageRole, ContentBlock};

#[cfg(test)]
use serde_json::{Value, json};

#[cfg(test)]
#[path = "../session_tests.rs"]
mod tests;
