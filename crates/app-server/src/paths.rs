//! User-level session layout (`~/.singularity/sessions/` + `index.sqlite3`) and
//! one-shot migration from legacy project-local `.singularity/agent-sessions/`.

use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use singularity_core::user_singularity_home;
use singularity_store::{
    SessionRecord, SessionStatus, SessionStore, ensure_owner_only_dir, ensure_owner_only_file,
    now_iso,
};
use uuid::Uuid;

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

    /// 创建并收紧会话目录、备份目录权限；索引文件在 store 打开后收紧。
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
/// 安全顺序：全部 header/destination 预检 → 复制到 backups 并校验 → 复制为
/// `<uuid>.jsonl` 并校验 → 写入索引 → 全部成功后才删除旧项目文件。任何冲突或
/// 无法识别的文件都立即停止，不覆盖、不清理。
pub fn migrate_legacy_project_sessions(
    paths: &AppPaths,
    store: &SessionStore,
    project_cwd: &Path,
) -> Result<usize, String> {
    let legacy_dir = project_cwd.join(".singularity").join("agent-sessions");
    let entries = match std::fs::read_dir(&legacy_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => {
            return Err(format!(
                "failed to inspect legacy session directory {}: {error}",
                legacy_dir.display()
            ));
        }
    };
    let mut sources = Vec::new();
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
    sources.sort();
    if sources.is_empty() {
        return Ok(0);
    }

    // 预检所有文件，全部可识别且无冲突后才开始复制。
    let current_cwd = project_cwd
        .canonicalize()
        .unwrap_or_else(|_| project_cwd.to_path_buf());
    let mut planned = Vec::new();
    for source in &sources {
        let (header, cwd, created_at) = read_legacy_header(source, &current_cwd)?;
        let destination = paths.sessions_dir.join(format!("{}.jsonl", header.id));
        if destination.exists() {
            return Err(format!(
                "migration conflict: destination already exists for {} ({})",
                source.display(),
                destination.display()
            ));
        }
        if store.get_session(&header.id).is_ok() {
            return Err(format!(
                "migration conflict: session index already contains {}",
                header.id
            ));
        }
        if planned
            .iter()
            .any(|(id, _, _): &(String, PathBuf, String)| id == &header.id)
        {
            return Err(format!(
                "migration conflict: duplicate session id {}",
                header.id
            ));
        }
        planned.push((header.id, destination, cwd));
        let _ = created_at;
    }

    let backup_dir = paths
        .backups_dir
        .join(format!("pre-migration-{}", now_iso_safe()));
    create_owner_only_dir(&backup_dir)?;

    let mut migrated = 0usize;
    for ((source, (session_id, destination, cwd)), (header, _, created_at)) in
        sources.iter().zip(planned.iter()).zip(
            sources
                .iter()
                .map(|source| read_legacy_header(source, &current_cwd))
                .collect::<Result<Vec<_>, _>>()?,
        )
    {
        let source_name = source
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| {
                format!(
                    "legacy session has no UTF-8 file name: {}",
                    source.display()
                )
            })?;
        let backup = backup_dir.join(source_name);
        copy_verified(source, &backup)?;
        ensure_owner_only_file(&backup).map_err(|error| error.to_string())?;
        copy_verified(source, destination)?;
        ensure_owner_only_file(destination).map_err(|error| error.to_string())?;

        let rollout_path = destination
            .canonicalize()
            .unwrap_or_else(|_| destination.clone())
            .to_string_lossy()
            .to_string();
        let record = SessionRecord {
            session_id: session_id.clone(),
            rollout_path,
            cwd: cwd.clone(),
            title: None,
            model: None,
            status: SessionStatus::Active,
            created_at: created_at.clone(),
            updated_at: created_at.clone(),
            token_usage: Value::Object(serde_json::Map::new()),
        };
        store
            .insert_session(&record)
            .map_err(|error| format!("failed to index migrated session {session_id}: {error}"))?;
        let _ = header;
        migrated += 1;
    }

    // 全部文件已验证并写入索引，才清理项目内旧数据。
    for source in &sources {
        std::fs::remove_file(source).map_err(|error| {
            format!(
                "failed to clean legacy session {}: {error}",
                source.display()
            )
        })?;
    }
    if legacy_dir
        .read_dir()
        .is_ok_and(|mut entries| entries.next().is_none())
    {
        let _ = std::fs::remove_dir(&legacy_dir);
    }
    for name in [
        "rust-app-server.sqlite3",
        "rust-app-server.sqlite3-wal",
        "rust-app-server.sqlite3-shm",
        "rust-app-server.sqlite3.init.lock",
    ] {
        let path = project_cwd.join(".singularity").join(name);
        if path.is_file() {
            std::fs::remove_file(&path).map_err(|error| {
                format!(
                    "failed to clean legacy project state {}: {error}",
                    path.display()
                )
            })?;
        }
    }
    let project_state = project_cwd.join(".singularity");
    if project_state
        .read_dir()
        .is_ok_and(|mut entries| entries.next().is_none())
    {
        let _ = std::fs::remove_dir(&project_state);
    }
    Ok(migrated)
}

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

fn copy_verified(source: &Path, destination: &Path) -> Result<(), String> {
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
    Ok(())
}

fn hash_file(path: &Path) -> Result<Vec<u8>, String> {
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
    Ok(hasher.finalize().to_vec())
}

fn now_iso_safe() -> String {
    now_iso().replace([':', '.'], "-")
}
