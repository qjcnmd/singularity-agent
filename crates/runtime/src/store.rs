//! Thread 的持久化入口：创建、定位、修复重开与元数据投影。
//!
//! JSONL 会话文件是唯一持久事实源；这里只做路径、权限与打开/修复的统一
//! 入口，不复制会话状态。

use std::path::{Path, PathBuf};

use singularity_agent::session::{SessionManager, SessionProjectionStatus, project_session};
use uuid::Uuid;

use crate::error::TurnFailureCause;
use crate::objects::{Thread, ThreadStatus};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadSummary {
    pub thread_id: String,
    pub cwd: String,
    pub title: Option<String>,
    pub model: Option<String>,
    pub status: Option<ThreadStatus>,
    pub created_at: String,
    pub updated_at: String,
    pub token_usage: serde_json::Value,
    pub turn_count: usize,
    pub total_tokens: u64,
}

pub const SESSIONS_DIR_NAME: &str = "sessions";

/// 创建 home 下的 sessions 目录（Unix 收紧为属主专用）。
pub fn prepare_session_dirs(home: &Path) -> Result<(), String> {
    singularity_core::create_owner_only_dir(&home.join(SESSIONS_DIR_NAME))?;
    Ok(())
}

fn session_file(sessions_dir: &Path, thread_id: &str) -> PathBuf {
    sessions_dir.join(format!("{thread_id}.jsonl"))
}

/// Thread 会话文件的规范位置。
pub fn thread_session_path(sessions_dir: &Path, thread_id: &str) -> PathBuf {
    session_file(sessions_dir, thread_id)
}

/// 归一化并校验新 Thread 的工作目录；缺省取当前目录。
pub fn canonical_thread_cwd(cwd: Option<&str>) -> Result<String, String> {
    let path = match cwd {
        Some(cwd) if !cwd.trim().is_empty() => Path::new(cwd).to_path_buf(),
        Some(_) => return Err("thread cwd must not be empty".to_string()),
        None => std::env::current_dir()
            .map_err(|error| format!("failed to read current directory: {error}"))?,
    };
    let canonical =
        std::fs::canonicalize(&path).map_err(|_| "failed to bind thread cwd".to_string())?;
    canonical
        .to_str()
        .map(str::to_string)
        .ok_or_else(|| "thread cwd is not valid UTF-8".to_string())
}

/// 创建新 Thread（uuid v7 会话文件，属主权限）。
pub fn create_thread(
    sessions_dir: &Path,
    cwd: &str,
    model: Option<String>,
) -> Result<Thread, String> {
    let thread_id = Uuid::now_v7().to_string();
    let session = SessionManager::create_with_id(Path::new(cwd), sessions_dir, &thread_id)
        .map_err(|_| "failed to create session file".to_string())?;
    singularity_core::ensure_owner_only_file(session.path())?;
    Ok(Thread {
        thread_id,
        cwd: cwd.to_string(),
        model,
        last_turn_status: None,
    })
}

/// 重开既有 Thread 并执行崩溃修复；返回投影后的 Thread。
///
/// 修复语义与 turn 打开路径一致：未终态 `turn_started` 补写 synthetic
/// interrupted，孤立 tool call 补写 synthetic failed ToolResult。管理器在
/// 投影后关闭；每个 turn 由 runner 按单写者合同重新独占打开。
pub fn resume_thread(sessions_dir: &Path, thread_id: &str) -> Result<Thread, ResumeError> {
    let path = session_file(sessions_dir, thread_id);
    if !path.exists() {
        return Err(ResumeError::NotFound(thread_id.to_string()));
    }
    let mut session = SessionManager::open_existing(&path)
        .map_err(|error| ResumeError::Store(error.to_string()))?;
    if session.session_id() != thread_id {
        return Err(ResumeError::Store(format!(
            "rollout header id {} does not match requested thread id {thread_id}",
            session.session_id()
        )));
    }
    session
        .repair_interrupted_turns()
        .map_err(|error| ResumeError::Store(error.to_string()))?;
    session
        .repair_orphaned_tool_calls()
        .map_err(|error| ResumeError::Store(error.to_string()))?;
    let projection = project_session(&session);
    let thread = Thread {
        thread_id: thread_id.to_string(),
        cwd: session.cwd().to_string_lossy().to_string(),
        model: projection.model,
        last_turn_status: projection.status.and_then(|status| match status {
            SessionProjectionStatus::Completed => Some(ThreadStatus::Completed),
            SessionProjectionStatus::Failed => Some(ThreadStatus::Failed),
            SessionProjectionStatus::Interrupted => Some(ThreadStatus::Interrupted),
            SessionProjectionStatus::Active => None,
        }),
    };
    Ok(thread)
}

/// 列出可恢复 Thread；损坏或非规范文件不会阻断其余会话。
pub fn list_threads(sessions_dir: &Path) -> Result<Vec<ThreadSummary>, String> {
    if !sessions_dir.exists() {
        return Ok(Vec::new());
    }
    let entries = std::fs::read_dir(sessions_dir)
        .map_err(|error| format!("failed to list sessions: {error}"))?;
    let mut threads = Vec::new();
    for entry in entries {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("jsonl") {
            continue;
        }
        let Ok(session) = SessionManager::open_existing_read_only(&path) else {
            continue;
        };
        if path.file_stem().and_then(|value| value.to_str()) != Some(session.session_id()) {
            continue;
        }
        threads.push(thread_summary(&session));
    }
    threads.sort_by(|left, right| {
        right
            .updated_at
            .cmp(&left.updated_at)
            .then_with(|| left.thread_id.cmp(&right.thread_id))
    });
    Ok(threads)
}

/// 只读投影一个 Thread；不执行崩溃修复或写入。
pub fn read_thread_summary(
    sessions_dir: &Path,
    thread_id: &str,
) -> Result<ThreadSummary, ResumeError> {
    let path = session_file(sessions_dir, thread_id);
    if !path.exists() {
        return Err(ResumeError::NotFound(thread_id.to_string()));
    }
    let session = SessionManager::open_existing_read_only(&path)
        .map_err(|error| ResumeError::Store(error.to_string()))?;
    if session.session_id() != thread_id {
        return Err(ResumeError::Store(format!(
            "rollout header id {} does not match requested thread id {thread_id}",
            session.session_id()
        )));
    }
    Ok(thread_summary(&session))
}

fn thread_summary(session: &SessionManager) -> ThreadSummary {
    let projection = project_session(session);
    ThreadSummary {
        thread_id: projection.session_id,
        cwd: projection.cwd,
        title: projection.title,
        model: projection.model,
        status: projection.status.map(|status| match status {
            SessionProjectionStatus::Active => ThreadStatus::Active,
            SessionProjectionStatus::Completed => ThreadStatus::Completed,
            SessionProjectionStatus::Failed => ThreadStatus::Failed,
            SessionProjectionStatus::Interrupted => ThreadStatus::Interrupted,
        }),
        created_at: projection.created_at,
        updated_at: projection.updated_at,
        token_usage: projection
            .latest_usage
            .unwrap_or_else(|| serde_json::json!({})),
        turn_count: projection.turn_count,
        total_tokens: projection.total_tokens,
    }
}

/// 为 Thread 追加名称 metadata；JSONL 仍是唯一事实源。
pub fn rename_thread(sessions_dir: &Path, thread_id: &str, name: &str) -> Result<(), String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("thread name must not be empty".to_string());
    }
    let path = thread_session_path(sessions_dir, thread_id);
    let mut session = SessionManager::open_existing(&path).map_err(|error| error.to_string())?;
    if session.session_id() != thread_id {
        return Err("thread id does not match session header".to_string());
    }
    session
        .append_metadata(
            singularity_agent::session::SessionMetadata::thread_name(name)
                .map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string())?;
    Ok(())
}

/// 把 [`TurnFailureCause`] 中与存储相关的失败映射为统一文本。
impl From<TurnFailureCause> for String {
    fn from(cause: TurnFailureCause) -> Self {
        cause.as_str().to_string()
    }
}

/// Thread 定位与持久化错误。
#[derive(Debug, thiserror::Error)]
pub enum ResumeError {
    #[error("thread {0} was not found")]
    NotFound(String),
    #[error("{0}")]
    Store(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_session(sessions_dir: &Path, thread_id: &str, timestamp: &str) {
        let header = serde_json::json!({
            "type": "session",
            "version": 1,
            "id": thread_id,
            "timestamp": timestamp,
            "cwd": sessions_dir,
        });
        std::fs::write(
            sessions_dir.join(format!("{thread_id}.jsonl")),
            format!("{header}\n"),
        )
        .expect("write session");
    }

    #[test]
    fn list_threads_orders_by_updated_at_descending_then_id() {
        let temp = tempfile::tempdir().expect("temp dir");
        let first = "01914f6b-0000-7000-8000-000000000001";
        let second = "01914f6b-0000-7000-8000-000000000002";
        let newest = "01914f6b-0000-7000-8000-000000000003";
        write_session(temp.path(), second, "2026-08-20T00:00:00.000Z");
        write_session(temp.path(), newest, "2026-08-21T00:00:00.000Z");
        write_session(temp.path(), first, "2026-08-20T00:00:00.000Z");

        let threads = list_threads(temp.path()).expect("list threads");
        let ids = threads
            .iter()
            .map(|thread| thread.thread_id.as_str())
            .collect::<Vec<_>>();

        assert_eq!(ids, vec![newest, first, second]);
    }
}
