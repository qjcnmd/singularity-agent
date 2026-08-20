use std::collections::HashSet;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use super::{AppServerError, AppServerResult};
use serde_json::Value;
use singularity_agent::session::{SessionError, SessionManager, SessionMetadataKind};
use singularity_core::user_singularity_home;
use singularity_store::{
    SessionRecord, SessionStore, ensure_owner_only_dir, ensure_owner_only_file, now_iso,
};

const MAX_DISCOVERED_SESSION_HEADER_BYTES: usize = 16 * 1024 * 1024;

/// 从 JSONL rollout 的 header 重建 SQLite 的轻量索引投影。
///
/// 启动发现只读取每个文件的首行，不解析正文、不追加 repair 条目，也不让单个
/// 损坏文件阻断其它可用会话。JSONL 仍是唯一事实源；目标会话真正打开时再做
/// interrupted/orphan repair 并刷新该会话的 SQLite 投影。
pub fn rebuild_session_index_from_jsonl(
    store: &SessionStore,
    sessions_dir: &Path,
) -> AppServerResult<()> {
    let mut rebuilt_ids = HashSet::new();
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
        let Some(header) = (match discover_session_header(&path) {
            Ok(header) => header,
            Err(error) => {
                eprintln!(
                    "skipping unreadable session during discovery {}: {error}",
                    path.display()
                );
                continue;
            }
        }) else {
            continue;
        };
        let existing = store.get_session(&header.session_id).ok();
        let cwd = header
            .cwd
            .or_else(|| existing.as_ref().map(|record| record.cwd.clone()))
            .unwrap_or_else(|| {
                std::env::current_dir()
                    .unwrap_or_else(|_| PathBuf::from("."))
                    .to_string_lossy()
                    .to_string()
            });
        let record = SessionRecord {
            session_id: header.session_id.clone(),
            rollout_path: path.to_string_lossy().to_string(),
            cwd,
            title: existing.as_ref().and_then(|record| record.title.clone()),
            model: existing.as_ref().and_then(|record| record.model.clone()),
            status: existing.as_ref().and_then(|record| record.status),
            created_at: header
                .timestamp
                .or_else(|| existing.as_ref().map(|record| record.created_at.clone()))
                .unwrap_or_else(now_iso),
            updated_at: existing
                .as_ref()
                .map(|record| record.updated_at.clone())
                .unwrap_or_else(now_iso),
            token_usage: existing
                .as_ref()
                .map(|record| record.token_usage.clone())
                .unwrap_or_else(|| serde_json::json!({})),
        };
        store.upsert_session(&record)?;
        rebuilt_ids.insert(header.session_id);
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

struct DiscoveredSessionHeader {
    session_id: String,
    cwd: Option<String>,
    timestamp: Option<String>,
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
    let record = SessionRecord {
        session_id: session.session_id().to_string(),
        rollout_path: session.path().to_string_lossy().to_string(),
        cwd: session.cwd().to_string_lossy().to_string(),
        title: existing.as_ref().and_then(|record| record.title.clone()),
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

fn discover_session_header(path: &Path) -> AppServerResult<Option<DiscoveredSessionHeader>> {
    let file = std::fs::File::open(path)
        .map_err(|error| AppServerError::Session(SessionError::Io(error)))?;
    let mut reader = BufReader::new(file);
    let line = read_bounded_discovery_header(&mut reader, path)?;
    let line = std::str::from_utf8(&line)
        .map_err(|error| {
            AppServerError::Session(SessionError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                error,
            )))
        })?
        .trim_end_matches(['\r', '\n']);
    if line.is_empty() {
        return Err(AppServerError::Session(SessionError::InvalidSession(
            format!("session header is missing: {}", path.display()),
        )));
    }
    let value: Value = serde_json::from_str(line)?;
    if value.get("type").and_then(Value::as_str) != Some("session") {
        return Ok(None);
    }
    let Some(id) = value.get("id").and_then(Value::as_str) else {
        return Err(AppServerError::Session(SessionError::InvalidHeader(
            "session header id is missing".to_string(),
        )));
    };
    if id.trim().is_empty() {
        return Err(AppServerError::Session(SessionError::InvalidHeader(
            "session header id is empty".to_string(),
        )));
    }
    let cwd = match value.get("cwd") {
        None | Some(Value::Null) => None,
        Some(Value::String(cwd)) => Some(cwd.clone()),
        Some(_) => {
            return Err(AppServerError::Session(SessionError::InvalidHeader(
                "session header cwd must be a string".to_string(),
            )));
        }
    };
    let timestamp = value
        .get("timestamp")
        .and_then(Value::as_str)
        .map(str::to_string);
    Ok(Some(DiscoveredSessionHeader {
        session_id: id.to_string(),
        cwd,
        timestamp,
    }))
}

/// Read exactly the discovery header without allowing a missing newline to
/// force an unbounded `String` allocation. The limit includes a CRLF/LF
/// terminator to preserve the former `read_line` limit semantics.
fn read_bounded_discovery_header<R: BufRead>(
    reader: &mut R,
    path: &Path,
) -> AppServerResult<Vec<u8>> {
    let mut line = Vec::with_capacity(4096);
    loop {
        let buffer = reader
            .fill_buf()
            .map_err(|error| AppServerError::Session(SessionError::Io(error)))?;
        if buffer.is_empty() {
            return Ok(line);
        }
        let newline = buffer.iter().position(|byte| *byte == b'\n');
        let consumed = newline.map_or(buffer.len(), |position| position + 1);
        if line.len().saturating_add(consumed) > MAX_DISCOVERED_SESSION_HEADER_BYTES {
            return Err(AppServerError::Session(SessionError::InvalidSession(
                format!(
                    "session header exceeds bounded line limit: {}",
                    path.display()
                ),
            )));
        }
        line.extend_from_slice(&buffer[..consumed]);
        reader.consume(consumed);
        if newline.is_some() {
            return Ok(line);
        }
    }
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
