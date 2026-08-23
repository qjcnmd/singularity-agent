//! Thread 的持久化入口：创建、定位、修复重开与元数据投影。
//!
//! JSONL 会话文件是唯一持久事实源；这里只做路径、权限与打开/修复的统一
//! 入口，不复制会话状态。

use std::path::{Path, PathBuf};

use singularity_agent::session::{SessionManager, SessionMetadataKind};
use singularity_core::user_singularity_home;
use uuid::Uuid;

use crate::error::TurnFailureCause;
use crate::objects::{Thread, ThreadStatus};

pub const SESSIONS_DIR_NAME: &str = "sessions";
pub const BACKUPS_DIR_NAME: &str = "backups";

/// 解析默认的会话目录（`SINGULARITY_HOME/sessions`）。
pub fn default_sessions_dir() -> Result<PathBuf, String> {
    let home =
        user_singularity_home().ok_or_else(|| "cannot resolve SINGULARITY_HOME".to_string())?;
    Ok(home.join(SESSIONS_DIR_NAME))
}

/// 创建 home 下的 sessions 与 backups 目录（Unix 收紧为属主专用）。
pub fn prepare_session_dirs(home: &Path) -> Result<(), String> {
    singularity_core::create_owner_only_dir(&home.join(SESSIONS_DIR_NAME))?;
    singularity_core::create_owner_only_dir(&home.join(BACKUPS_DIR_NAME))?;
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
    let thread = Thread {
        thread_id: thread_id.to_string(),
        cwd: session.cwd().to_string_lossy().to_string(),
        model: persisted_model_selector(&session),
        last_turn_status: persisted_thread_status(&session),
    };
    Ok(thread)
}

/// 从最新 `thread_settings` metadata 投影模型 selector。
pub fn persisted_model_selector(session: &SessionManager) -> Option<String> {
    session.metadata_entries().iter().rev().find_map(|entry| {
        if entry.kind() != SessionMetadataKind::ThreadSettings {
            return None;
        }
        let provider = entry.field_string("provider");
        let model = entry.field_string("model")?;
        Some(match provider {
            Some(provider) => format!("{provider}/{model}"),
            None => model.to_string(),
        })
    })
}

/// 从最新终态 metadata 投影 Thread 状态；无终态记录时为 None。
fn persisted_thread_status(session: &SessionManager) -> Option<ThreadStatus> {
    session.metadata_entries().iter().rev().find_map(|entry| {
        let kind = entry.kind();
        if kind.matches_turn_terminal() {
            match kind {
                SessionMetadataKind::TurnCompleted => Some(ThreadStatus::Completed),
                SessionMetadataKind::TurnFailed => Some(ThreadStatus::Failed),
                _ => Some(ThreadStatus::Interrupted),
            }
        } else {
            None
        }
    })
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
