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
    SessionRecord, SessionStatus, SessionStore, StoreError, ensure_owner_only_dir,
    ensure_owner_only_file, now_iso,
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
                status: SessionStatus::Active,
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
