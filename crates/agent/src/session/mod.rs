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
pub use repository::{SessionRead, SessionRepository};

#[cfg(test)]
pub(crate) use file::{AppendLimits, normalize_cwd_string};

#[cfg(test)]
fn parse_session_lines_with_limits(
    file: &std::path::Path,
    max_file_bytes: usize,
    max_line_bytes: usize,
    max_content_entries: usize,
) -> Result<()> {
    self::file::parse_session_lines_with_limits(
        file,
        max_file_bytes,
        max_line_bytes,
        max_content_entries,
    )
    .map(|_| ())
}

#[cfg(test)]
use crate::message::{AgentMessage, AgentMessageRole, ContentBlock};

#[cfg(test)]
use serde_json::{Map, Value, json};

#[cfg(test)]
use singularity_model::ModelToolParseStatus;

#[cfg(test)]
#[path = "../session_tests.rs"]
mod tests;
