use std::collections::HashSet;
use std::path::{Path, PathBuf};

use super::{AppServerError, AppServerResult};
use singularity_agent::message::AgentMessageRole;
use singularity_agent::session::{SessionEntryType, SessionManager, SessionMetadataKind};
use singularity_core::user_singularity_home;
use singularity_store::{
    SessionRecord, SessionStore, ensure_owner_only_dir, ensure_owner_only_file, now_iso,
};

/// 从 JSONL rollout 的完整结构重建 SQLite 的轻量索引投影。
///
/// 启动扫描以有界、只读、no-repair 方式解析每个 session 文件。严格验证
/// 文件名/UUID、版本号、header 及 entry 线性结构。跳过单个坏 rollout，不移动、
/// 不修改且不阻断其它会话。从 JSONL 权威恢复 session id、cwd、created/updated、
/// 最新 model、最新 terminal status、最新 factual usage 及首条 user message title。
/// 随后清理已无对应 JSONL 的 ghost index rows。
pub fn rebuild_session_index_from_jsonl(
    store: &SessionStore,
    sessions_dir: &Path,
) -> AppServerResult<()> {
    let mut rebuilt_ids = HashSet::new();
    if !sessions_dir.exists() {
        return Ok(());
    }
    let entries = std::fs::read_dir(sessions_dir)
        .map_err(|error| AppServerError::Workspace(format!("failed to read sessions: {error}")))?;
    for entry in entries {
        let entry = entry.map_err(|error| {
            AppServerError::Workspace(format!("failed to enumerate sessions: {error}"))
        })?;
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("jsonl") {
            continue;
        }
        let file_stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        let metadata = match std::fs::metadata(&path) {
            Ok(m) => m,
            Err(error) => {
                eprintln!(
                    "skipping unreadable session file {}: {error}",
                    path.display()
                );
                continue;
            }
        };
        if metadata.len() > 512 * 1024 * 1024 {
            eprintln!(
                "skipping oversized session file {}: {} bytes",
                path.display(),
                metadata.len()
            );
            continue;
        }
        let session = match SessionManager::open_existing_read_only(&path) {
            Ok(session) => session,
            Err(error) => {
                eprintln!(
                    "skipping invalid session rollout {}: {error}",
                    path.display()
                );
                continue;
            }
        };
        if session.session_id() != file_stem {
            eprintln!(
                "skipping mismatched session id in rollout {}: expected {}, got {}",
                path.display(),
                file_stem,
                session.session_id()
            );
            continue;
        }
        let metadata_entries = session.metadata_entries();
        let model = metadata_entries
            .iter()
            .rev()
            .find(|entry| entry.kind() == SessionMetadataKind::ThreadSettings)
            .and_then(|entry| {
                let model = entry.field_string("model")?;
                let provider = entry.field_string("provider").unwrap_or_default();
                let selector = if provider.is_empty() {
                    model.to_string()
                } else {
                    format!("{provider}/{model}")
                };
                Some(match entry.field_string("reasoning") {
                    Some(reasoning) if !reasoning.is_empty() => format!("{selector}#{reasoning}"),
                    _ => selector,
                })
            });
        let status = metadata_entries
            .iter()
            .rev()
            .find_map(|entry| match entry.kind() {
                SessionMetadataKind::TurnStarted => Some(singularity_store::SessionStatus::Active),
                SessionMetadataKind::TurnCompleted => {
                    Some(singularity_store::SessionStatus::Completed)
                }
                SessionMetadataKind::TurnFailed => Some(singularity_store::SessionStatus::Failed),
                SessionMetadataKind::TurnInterrupted => {
                    Some(singularity_store::SessionStatus::Interrupted)
                }
                _ => None,
            });
        let token_usage = metadata_entries
            .iter()
            .rev()
            .find(|entry| entry.kind() == SessionMetadataKind::Usage)
            .and_then(|entry| entry.field("usage").cloned())
            .unwrap_or_else(|| serde_json::json!({}));
        let title = session.entries().iter().find_map(|entry| {
            if let SessionEntryType::Message(msg) = &entry.entry_type
                && msg.role == AgentMessageRole::User
            {
                let text = msg.content_text();
                let title = crate::dispatch::title_from_input(&text);
                if !title.is_empty() {
                    return Some(title);
                }
            }
            None
        });
        let created_at = session.created_at().to_string();
        let updated_at = session
            .entries()
            .last()
            .and_then(|e| e.timestamp.clone())
            .unwrap_or_else(|| created_at.clone());
        let record = SessionRecord {
            session_id: session.session_id().to_string(),
            rollout_path: path.to_string_lossy().to_string(),
            cwd: session.cwd().to_string_lossy().to_string(),
            title,
            model,
            status,
            created_at,
            updated_at,
            token_usage,
        };
        store.upsert_session(&record)?;
        rebuilt_ids.insert(session.session_id().to_string());
    }
    for record in store.list_sessions()? {
        if !rebuilt_ids.contains(&record.session_id)
            && Path::new(&record.rollout_path).parent() == Some(sessions_dir)
        {
            store.delete_session(&record.session_id)?;
        }
    }
    Ok(())
}

pub(super) fn refresh_session_index_from_open_session(
    store: &SessionStore,
    session: &SessionManager,
) -> AppServerResult<SessionRecord> {
    let existing = store.get_session(session.session_id()).ok();
    let metadata = session.metadata_entries();
    let model = metadata
        .iter()
        .rev()
        .find(|entry| entry.kind() == SessionMetadataKind::ThreadSettings)
        .and_then(|entry| {
            let model = entry.field_string("model")?;
            let provider = entry.field_string("provider").unwrap_or_default();
            let selector = if provider.is_empty() {
                model.to_string()
            } else {
                format!("{provider}/{model}")
            };
            Some(match entry.field_string("reasoning") {
                Some(reasoning) if !reasoning.is_empty() => format!("{selector}#{reasoning}"),
                _ => selector,
            })
        })
        .or_else(|| existing.as_ref().and_then(|record| record.model.clone()));
    let status = metadata
        .iter()
        .rev()
        .find_map(|entry| match entry.kind() {
            SessionMetadataKind::TurnStarted => Some(singularity_store::SessionStatus::Active),
            SessionMetadataKind::TurnCompleted => Some(singularity_store::SessionStatus::Completed),
            SessionMetadataKind::TurnFailed => Some(singularity_store::SessionStatus::Failed),
            SessionMetadataKind::TurnInterrupted => {
                Some(singularity_store::SessionStatus::Interrupted)
            }
            _ => None,
        })
        .or_else(|| existing.as_ref().and_then(|record| record.status));
    let token_usage = metadata
        .iter()
        .rev()
        .find(|entry| entry.kind() == SessionMetadataKind::Usage)
        .and_then(|entry| entry.field("usage").cloned())
        .or_else(|| existing.as_ref().map(|record| record.token_usage.clone()))
        .unwrap_or_else(|| serde_json::json!({}));
    let title = session
        .entries()
        .iter()
        .find_map(|entry| {
            if let SessionEntryType::Message(msg) = &entry.entry_type
                && msg.role == AgentMessageRole::User
            {
                let text = msg.content_text();
                let title = crate::dispatch::title_from_input(&text);
                if !title.is_empty() {
                    return Some(title);
                }
            }
            None
        })
        .or_else(|| existing.as_ref().and_then(|record| record.title.clone()));
    let record = SessionRecord {
        session_id: session.session_id().to_string(),
        rollout_path: session.path().to_string_lossy().to_string(),
        cwd: session.cwd().to_string_lossy().to_string(),
        title,
        model,
        status,
        created_at: existing
            .as_ref()
            .map(|record| record.created_at.clone())
            .unwrap_or_else(now_iso),
        updated_at: now_iso(),
        token_usage,
    };
    store.upsert_session(&record)?;
    Ok(record)
}

pub(super) fn canonical_thread_cwd(cwd: Option<&str>) -> Result<String, String> {
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

pub(super) fn workspace_path(thread: &singularity_protocol::Thread) -> Result<PathBuf, String> {
    let cwd = thread
        .cwd
        .as_deref()
        .filter(|cwd| !cwd.trim().is_empty())
        .ok_or_else(|| "thread does not have an absolute workspace".to_string())?;
    let path = Path::new(cwd);
    if !path.is_absolute() {
        return Err("thread does not have an absolute workspace".to_string());
    }
    Ok(path.to_path_buf())
}

/// 持久化状态的原始投影：仅供内部（打开会话、provider 配置）使用；
/// wire 可见的 thread 摘要必须经过 `AppServer::project_thread`。
pub fn thread_from_record(record: &SessionRecord) -> singularity_protocol::Thread {
    singularity_protocol::Thread {
        thread_id: record.session_id.clone(),
        model: record.model.clone(),
        cwd: Some(record.cwd.clone()),
        last_turn_status: match record.status {
            None => None,
            Some(singularity_store::SessionStatus::Active) => {
                Some(singularity_protocol::ThreadStatus::Active)
            }
            Some(singularity_store::SessionStatus::Completed) => {
                Some(singularity_protocol::ThreadStatus::Completed)
            }
            Some(singularity_store::SessionStatus::Failed) => {
                Some(singularity_protocol::ThreadStatus::Failed)
            }
            Some(singularity_store::SessionStatus::Interrupted) => {
                Some(singularity_protocol::ThreadStatus::Interrupted)
            }
        },
    }
}

pub const SESSIONS_DIR_NAME: &str = "sessions";
pub const BACKUPS_DIR_NAME: &str = "backups";
pub const INDEX_FILE_NAME: &str = "index.sqlite3";

/// `~/.singularity` 下由本次架构固定下来的路径集合。
#[derive(Debug, Clone)]
pub struct AppPaths {
    pub home_dir: PathBuf,
    pub index_path: PathBuf,
    pub sessions_dir: PathBuf,
    pub backups_dir: PathBuf,
}

impl AppPaths {
    pub fn resolve() -> Result<Self, String> {
        let home = user_singularity_home()
            .ok_or_else(|| "cannot resolve SINGULARITY_HOME for session index".to_string())?;
        Ok(Self {
            index_path: home.join(INDEX_FILE_NAME),
            sessions_dir: home.join(SESSIONS_DIR_NAME),
            backups_dir: home.join(BACKUPS_DIR_NAME),
            home_dir: home,
        })
    }

    /// 创建会话目录、备份目录与索引所在目录（在 Unix 系统上收紧为 0700 权限）。
    pub fn prepare(&self) -> Result<(), String> {
        create_owner_only_dir(&self.home_dir)?;
        create_owner_only_dir(&self.sessions_dir)?;
        create_owner_only_dir(&self.backups_dir)?;
        Ok(())
    }

    pub fn ensure_index_owner_only(&self) -> Result<(), String> {
        ensure_owner_only_file(&self.index_path).map_err(|error| error.to_string())
    }
}

pub fn create_owner_only_dir(path: &Path) -> Result<(), String> {
    std::fs::create_dir_all(path)
        .map_err(|error| format!("failed to create {}: {error}", path.display()))?;
    ensure_owner_only_dir(path).map_err(|error| error.to_string())
}
