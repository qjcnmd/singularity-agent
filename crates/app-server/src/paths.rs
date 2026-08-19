//! User-level session layout (`~/.singularity/sessions/` + `index.sqlite3`) and
//! one-shot migration from legacy project-local `.singularity/agent-sessions/`.

use std::collections::HashSet;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};

use super::{AppServerError, AppServerResult};
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use singularity_agent::session::{SessionError, SessionManager, SessionMetadataKind};
use singularity_core::user_singularity_home;
use singularity_store::{
    SessionRecord, SessionStore, StoreError, ensure_owner_only_dir, ensure_owner_only_file, now_iso,
};
use uuid::Uuid;

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

    /// 创建会话目录、备份目录与索引所在目录；Unix 上收紧为 0700，
    /// Windows 按 Pi 策略不做额外 ACL 管理（由目录 ACL 决定）。
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

#[derive(Debug, Deserialize)]
struct LegacyHeader {
    id: String,
    #[serde(default)]
    timestamp: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
}

/// 将 `<cwd>/.singularity/agent-sessions/*.jsonl` 迁移到用户会话目录。
///
/// 顺序：完整 preflight（header/destination/index/旧 SQLite 数据）→ 备份所有
/// 将清理的旧对象并写 manifest → 复制 `<uuid>.jsonl` 并写索引 → 全部成功后才
/// 清理旧项目文件。destination 与索引已存在时只接受“内容 hash 一致”的续跑；
/// 同 UUID 不同内容、旧 SQLite 含数据、未知对象都 fail closed，绝不覆盖。
pub fn migrate_legacy_project_sessions(
    paths: &AppPaths,
    store: &SessionStore,
    project_cwd: &Path,
) -> Result<usize, String> {
    let legacy_state_dir = project_cwd.join(".singularity");
    let legacy_dir = legacy_state_dir.join("agent-sessions");
    let mut sources = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&legacy_dir) {
        for entry in entries {
            let entry =
                entry.map_err(|error| format!("failed to read legacy session entry: {error}"))?;
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            if path.extension().and_then(|extension| extension.to_str()) != Some("jsonl") {
                continue;
            }
            sources.push(path);
        }
    } else if legacy_dir.exists() {
        return Err(format!(
            "legacy session directory is not readable: {}",
            legacy_dir.display()
        ));
    }
    sources.sort();

    let mut legacy_objects = sources.clone();
    for name in LEGACY_SQLITE_OBJECTS {
        let path = legacy_state_dir.join(name);
        if path.is_file() {
            legacy_objects.push(path);
        }
    }
    legacy_objects.sort();
    if legacy_objects.is_empty() {
        return Ok(0);
    }

    // 旧 SQLite 若含任何用户行，立即停止；lock/WAL/SHM 只随主库整体备份。
    let main_sqlite = legacy_state_dir.join("rust-app-server.sqlite3");
    if main_sqlite.is_file() {
        let report = singularity_store::inspect_legacy_sqlite(&main_sqlite).map_err(|error| {
            format!(
                "cannot inspect legacy SQLite {}; stopping without changes: {error}",
                main_sqlite.display()
            )
        })?;
        if report.user_rows != 0 {
            return Err(format!(
                "legacy SQLite {} contains {} user data row(s); stopping without changes",
                main_sqlite.display(),
                report.user_rows
            ));
        }
    }

    let current_cwd = project_cwd
        .canonicalize()
        .unwrap_or_else(|_| project_cwd.to_path_buf());
    let mut planned = Vec::new();
    for source in &sources {
        let (header, cwd, created_at) = read_legacy_header(source, &current_cwd)?;
        let destination = paths.sessions_dir.join(format!("{}.jsonl", header.id));
        if planned
            .iter()
            .any(|plan: &PlannedSession| plan.session_id == header.id)
        {
            return Err(format!(
                "migration conflict: duplicate session id {}",
                header.id
            ));
        }
        if destination.exists() && hash_file(&destination)? != hash_file(source)? {
            return Err(format!(
                "migration conflict: destination exists with different content for {} ({})",
                source.display(),
                destination.display()
            ));
        }
        planned.push(PlannedSession {
            session_id: header.id,
            source: source.clone(),
            destination,
            cwd,
            created_at,
        });
    }

    // preflight 索引：不存在则稍后插入；存在则必须指向同一 rollout 内容。
    let mut indexed = Vec::new();
    for plan in &planned {
        match store.get_session(&plan.session_id) {
            Ok(record) => {
                let record_path = Path::new(&record.rollout_path);
                if record_path != plan.destination {
                    return Err(format!(
                        "migration conflict: session index {} points to {} instead of {}",
                        plan.session_id,
                        record.rollout_path,
                        plan.destination.display()
                    ));
                }
                if hash_file(record_path)? != hash_file(&plan.destination)? {
                    return Err(format!(
                        "migration conflict: indexed rollout content differs for {}",
                        plan.session_id
                    ));
                }
                indexed.push(plan.session_id.clone());
            }
            Err(StoreError::NotFound(_)) => {}
            Err(error) => return Err(format!("migration index preflight failed: {error}")),
        }
    }

    let backup_dir = paths
        .backups_dir
        .join(format!("pre-migration-{}", now_iso_safe()));
    create_owner_only_dir(&backup_dir)?;
    let mut manifest = MigrationManifest {
        entries: Vec::new(),
    };

    // 先备份所有将被清理的旧对象；任一失败都不会删除任何旧文件。
    for object in &legacy_objects {
        let source_name = object
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| format!("legacy object has no UTF-8 name: {}", object.display()))?;
        let backup = backup_dir.join(source_name);
        let hash = copy_verified(object, &backup)?;
        ensure_owner_only_file(&backup).map_err(|error| error.to_string())?;
        manifest.entries.push(MigrationManifestEntry {
            source_name: source_name.to_string(),
            sha256: hash,
            destination: None,
            session_id: None,
        });
    }

    let mut migrated = 0usize;
    for plan in &planned {
        let already_indexed = indexed.contains(&plan.session_id);
        let (hash, copied) = if plan.destination.exists() {
            (hash_file(&plan.destination)?, false)
        } else {
            let hash = copy_verified(&plan.source, &plan.destination)?;
            ensure_owner_only_file(&plan.destination).map_err(|error| error.to_string())?;
            (hash, true)
        };
        if !already_indexed {
            let rollout_path = plan
                .destination
                .canonicalize()
                .unwrap_or_else(|_| plan.destination.clone())
                .to_string_lossy()
                .to_string();
            let record = SessionRecord {
                session_id: plan.session_id.clone(),
                rollout_path,
                cwd: plan.cwd.clone(),
                title: None,
                model: None,
                // 迁移行无法从旧索引得知最近 turn 终态；按「尚无可展示 turn」
                // 以 null 入库，不得伪装成运行中或任何终态。
                status: None,
                created_at: plan.created_at.clone(),
                updated_at: plan.created_at.clone(),
                token_usage: Value::Object(serde_json::Map::new()),
            };
            store.insert_session(&record).map_err(|error| {
                format!(
                    "failed to index migrated session {}: {error}",
                    plan.session_id
                )
            })?;
        }
        if copied {
            let source_name = plan
                .destination
                .file_name()
                .and_then(|name| name.to_str())
                .expect("destination has file name");
            manifest.entries.push(MigrationManifestEntry {
                source_name: source_name.to_string(),
                sha256: hash,
                destination: Some(plan.destination.to_string_lossy().to_string()),
                session_id: Some(plan.session_id.clone()),
            });
        }
        migrated += 1;
    }

    let manifest_path = backup_dir.join("manifest.json");
    let mut manifest_file = singularity_core::create_owner_only_file(&manifest_path)
        .map_err(|error| format!("failed to create migration manifest: {error}"))?;
    manifest_file
        .write_all(
            serde_json::to_string_pretty(&manifest)
                .map_err(|error| format!("failed to serialize migration manifest: {error}"))?
                .as_bytes(),
        )
        .map_err(|error| format!("failed to write migration manifest: {error}"))?;
    manifest_file
        .flush()
        .map_err(|error| format!("failed to flush migration manifest: {error}"))?;
    drop(manifest_file);

    // 全部对象已备份、全部会话已复制/校验并写入索引，才清理旧项目对象。
    for object in &legacy_objects {
        std::fs::remove_file(object).map_err(|error| {
            format!(
                "failed to clean legacy project object {}: {error}",
                object.display()
            )
        })?;
    }
    if legacy_dir
        .read_dir()
        .is_ok_and(|mut entries| entries.next().is_none())
    {
        let _ = std::fs::remove_dir(&legacy_dir);
    }
    if legacy_state_dir
        .read_dir()
        .is_ok_and(|mut entries| entries.next().is_none())
    {
        let _ = std::fs::remove_dir(&legacy_state_dir);
    }
    Ok(migrated)
}

struct PlannedSession {
    session_id: String,
    source: PathBuf,
    destination: PathBuf,
    cwd: String,
    created_at: String,
}

#[derive(serde::Serialize)]
struct MigrationManifest {
    entries: Vec<MigrationManifestEntry>,
}

#[derive(serde::Serialize)]
struct MigrationManifestEntry {
    source_name: String,
    sha256: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    destination: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<String>,
}

const LEGACY_SQLITE_OBJECTS: &[&str] = &[
    "rust-app-server.sqlite3",
    "rust-app-server.sqlite3-wal",
    "rust-app-server.sqlite3-shm",
    "rust-app-server.sqlite3.init.lock",
];

fn read_legacy_header(
    path: &Path,
    current_cwd: &Path,
) -> Result<(LegacyHeader, String, String), String> {
    const MAX_HEADER_BYTES: usize = 1024 * 1024;
    let file = File::open(path)
        .map_err(|error| format!("failed to open legacy session {}: {error}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    let read = reader.read_until(b'\n', &mut line).map_err(|error| {
        format!(
            "failed to read legacy session header {}: {error}",
            path.display()
        )
    })?;
    if read == 0 || read > MAX_HEADER_BYTES {
        return Err(format!(
            "unrecognized legacy session file (empty or oversized header): {}",
            path.display()
        ));
    }
    if line.last() == Some(&b'\n') {
        line.pop();
        if line.last() == Some(&b'\r') {
            line.pop();
        }
    }
    let header: LegacyHeader = serde_json::from_slice(&line).map_err(|error| {
        format!(
            "unrecognized legacy session header in {}: {error}",
            path.display()
        )
    })?;
    Uuid::parse_str(&header.id).map_err(|error| {
        format!(
            "unrecognized legacy session id {} in {}: {error}",
            header.id,
            path.display()
        )
    })?;
    let cwd = match header
        .cwd
        .as_deref()
        .filter(|cwd| !cwd.trim().is_empty())
        .and_then(|cwd| Path::new(cwd).canonicalize().ok())
        .or_else(|| Some(current_cwd.to_path_buf()))
    {
        Some(cwd) => cwd.to_string_lossy().to_string(),
        None => current_cwd.to_string_lossy().to_string(),
    };
    let created_at = header.timestamp.clone().unwrap_or_else(now_iso);
    Ok((header, cwd, created_at))
}

fn copy_verified(source: &Path, destination: &Path) -> Result<String, String> {
    let mut source_file = File::open(source)
        .map_err(|error| format!("failed to open copy source {}: {error}", source.display()))?;
    let mut destination_file =
        singularity_core::create_owner_only_file(destination).map_err(|error| {
            format!(
                "failed to create owner-only copy destination {}: {error}",
                destination.display()
            )
        })?;
    std::io::copy(&mut source_file, &mut destination_file).map_err(|error| {
        format!(
            "failed to copy {} to {}: {error}",
            source.display(),
            destination.display()
        )
    })?;
    destination_file
        .flush()
        .map_err(|error| format!("failed to flush {}: {error}", destination.display()))?;
    drop(destination_file);
    let source_hash = hash_file(source)?;
    let destination_hash = hash_file(destination)?;
    if source_hash != destination_hash {
        return Err(format!(
            "copy verification failed for {} -> {}",
            source.display(),
            destination.display()
        ));
    }
    Ok(destination_hash)
}

fn hash_file(path: &Path) -> Result<String, String> {
    let mut file = File::open(path)
        .map_err(|error| format!("failed to open {} for hashing: {error}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| format!("failed to hash {}: {error}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

fn now_iso_safe() -> String {
    now_iso().replace([':', '.'], "-")
}
