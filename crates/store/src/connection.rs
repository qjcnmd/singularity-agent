//! SQLite connection configuration and protected file opening.

use super::*;

/// SQLite 索引的公开描述及其支持的模式版本。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionStoreDescriptor {
    /// 存储后端名称。
    pub backend: String,
    /// 数据库路径或 SQLite 特殊路径。
    pub path: String,
    /// 当前支持的 schema 版本。
    pub schema_version: u32,
}

/// 只保存会话元数据索引的 SQLite store；JSONL rollout 是唯一权威正文。
pub struct SessionStore {
    pub(crate) connection: Connection,
    pub(crate) descriptor: SessionStoreDescriptor,
    pub(crate) runtime_path: Option<PathBuf>,
    pub(crate) identity_guard: Option<StoreIdentityGuard>,
}

impl SessionStore {
    /// 打开 SQLite 索引，配置安全失败的 `pragma`，并执行模式检查/初始化。
    /// 若检测到可恢复的结构损坏（malformed/not-a-database、unsupported schema、schema structure mismatch），
    /// 将损坏库与 sidecar 文件原子隔离备份并创建新库。
    pub fn open(path: impl AsRef<Path>) -> StoreResult<Self> {
        Self::open_with_initialization(path, |_| Ok(()))
    }

    /// Open an index and run the caller's rebuild/initialization callback while
    /// the same stable `<db>.init.lock` remains held. The callback is invoked
    /// after any quarantine and current-schema creation, so a failed rebuild
    /// leaves both the quarantine evidence and the new partial database
    /// visible to diagnostics instead of reporting a false successful startup.
    pub fn open_with_initialization<F>(path: impl AsRef<Path>, initialize: F) -> StoreResult<Self>
    where
        F: FnOnce(&SessionStore) -> StoreResult<()>,
    {
        let path = path.as_ref();
        if path == Path::new(":memory:") {
            let store = Self::open_in_memory()?;
            initialize(&store)?;
            return Ok(store);
        }
        let _initialization_lock = acquire_store_initialization_lock(path)?;
        let store = match Self::open_file_backed(path) {
            Ok(store) => store,
            Err(error) if error.is_recoverable_corruption() => {
                quarantine_corrupted_store_files(path)?;
                Self::open_file_backed(path)?
            }
            Err(error) => return Err(error),
        };
        initialize(&store)?;
        Ok(store)
    }

    fn open_in_memory() -> StoreResult<Self> {
        let connection = Connection::open(":memory:")?;
        configure_connection(&connection)?;
        validate_connection_pragmas(&connection, true)?;
        let store = Self {
            connection,
            descriptor: SessionStoreDescriptor {
                backend: "sqlite".to_string(),
                path: ":memory:".to_string(),
                schema_version: SCHEMA_VERSION,
            },
            runtime_path: None,
            identity_guard: None,
        };
        migration::initialize_or_validate_schema(&store.connection)?;
        Ok(store)
    }

    fn open_file_backed(path: &Path) -> StoreResult<Self> {
        let identity_guard = StoreIdentityGuard::open(path, true)?;
        let runtime_path = identity_guard.path.clone();
        let connection = Connection::open_with_flags(
            &runtime_path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        identity_guard.verify()?;
        configure_connection(&connection)?;
        validate_connection_pragmas(&connection, false)?;
        let store = Self {
            connection,
            descriptor: SessionStoreDescriptor {
                backend: "sqlite".to_string(),
                path: path.to_string_lossy().to_string(),
                schema_version: SCHEMA_VERSION,
            },
            runtime_path: Some(runtime_path),
            identity_guard: Some(identity_guard),
        };
        migration::initialize_or_validate_schema(&store.connection)?;
        if let Some(identity_guard) = &store.identity_guard {
            identity_guard.verify()?;
        }
        Ok(store)
    }

    /// 从已经初始化的 file-backed store 派生 worker 专用连接。
    ///
    /// 该入口不接受路径，只能使用当前 store 已固定的规范路径；它执行结构校验。
    /// `:memory:` store 没有可安全派生的独立连接。
    pub fn trusted_reopen(&self) -> StoreResult<Self> {
        let runtime_path = self.runtime_path.clone().ok_or_else(|| {
            StoreError::InvalidState(
                "trusted store reopen requires a file-backed initialized store".to_string(),
            )
        })?;
        let original_guard = self.identity_guard.as_ref().ok_or_else(|| {
            StoreError::InvalidState(
                "trusted store reopen requires a protected file identity".to_string(),
            )
        })?;
        original_guard.verify()?;
        let identity_guard = StoreIdentityGuard::open(&runtime_path, false)?;
        if identity_guard.identity != original_guard.identity {
            return Err(StoreError::InvalidState(
                "trusted store reopen resolved a different file identity".to_string(),
            ));
        }
        let connection = Connection::open_with_flags(
            &runtime_path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        identity_guard.verify()?;
        original_guard.verify()?;
        configure_connection(&connection)?;
        validate_connection_pragmas(&connection, false)?;
        migration::validate_current_schema(&connection)?;
        identity_guard.verify()?;
        original_guard.verify()?;
        Ok(Self {
            connection,
            descriptor: self.descriptor.clone(),
            runtime_path: Some(runtime_path),
            identity_guard: Some(identity_guard),
        })
    }

    /// 返回存储后端及 schema 版本描述。
    pub fn descriptor(&self) -> &SessionStoreDescriptor {
        &self.descriptor
    }
}

pub(crate) fn configure_connection(connection: &Connection) -> StoreResult<()> {
    connection.busy_timeout(Duration::from_millis(SQLITE_BUSY_TIMEOUT_MS))?;
    connection.pragma_update(None, SQLITE_SECURE_DELETE_PRAGMA, "ON")?;
    connection.pragma_update(None, SQLITE_FOREIGN_KEYS_PRAGMA, "ON")?;
    connection.pragma_update(None, SQLITE_JOURNAL_MODE_PRAGMA, SQLITE_JOURNAL_MODE_WAL)?;
    Ok(())
}

// 确认每个 store connection 都处于受支持的 SQLite 运行时配置。
pub(crate) fn validate_connection_pragmas(
    connection: &Connection,
    in_memory: bool,
) -> StoreResult<()> {
    let busy_timeout: i64 = connection.query_row("pragma busy_timeout", [], |row| row.get(0))?;
    if busy_timeout != SQLITE_BUSY_TIMEOUT_MS as i64 {
        return Err(StoreError::InvalidState(
            "store busy_timeout pragma is invalid".to_string(),
        ));
    }
    let foreign_keys: i64 = connection.query_row("pragma foreign_keys", [], |row| row.get(0))?;
    if foreign_keys != 1 {
        return Err(StoreError::InvalidState(
            "store foreign_keys pragma is disabled".to_string(),
        ));
    }
    let secure_delete: i64 = connection.query_row("pragma secure_delete", [], |row| row.get(0))?;
    if secure_delete == 0 {
        return Err(StoreError::InvalidState(
            "store secure_delete pragma is disabled".to_string(),
        ));
    }
    let journal_mode: String = connection.query_row("pragma journal_mode", [], |row| row.get(0))?;
    let expected_journal_mode = if in_memory {
        "memory"
    } else {
        SQLITE_JOURNAL_MODE_WAL
    };
    if !journal_mode.eq_ignore_ascii_case(expected_journal_mode) {
        return Err(StoreError::InvalidState(format!(
            "store journal_mode pragma is {journal_mode:?}, expected {expected_journal_mode:?}"
        )));
    }
    Ok(())
}

// 在 schema 初始化期间以独占文件锁串行化同一数据库。
pub(crate) fn acquire_store_initialization_lock(path: &Path) -> StoreResult<Option<File>> {
    if path == Path::new(":memory:") {
        return Ok(None);
    }
    let mut lock_path = path.as_os_str().to_os_string();
    lock_path.push(".init.lock");
    let lock_path = PathBuf::from(lock_path);
    let lock_file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(lock_path)
        .map_err(StoreError::InitializationLock)?;
    let deadline = Instant::now() + Duration::from_millis(SQLITE_BUSY_TIMEOUT_MS);
    loop {
        match lock_file.try_lock() {
            Ok(()) => return Ok(Some(lock_file)),
            Err(error) => {
                let error = std::io::Error::from(error);
                if error.kind() != std::io::ErrorKind::WouldBlock {
                    return Err(StoreError::InitializationLock(error));
                }
                if Instant::now() >= deadline {
                    return Err(StoreError::InvalidState(
                        "timed out waiting for store initialization lock".to_string(),
                    ));
                }
                thread::sleep(Duration::from_millis(STORE_INITIALIZATION_LOCK_RETRY_MS));
            }
        }
    }
}

/// 原子隔离损坏的 SQLite 主库与 sidecar 文件（`-wal`, `-shm`）。
/// 使用同一 backup identity 命名，保留在原目录供审计排查，不自动删除。
pub fn quarantine_corrupted_store_files(path: &Path) -> StoreResult<PathBuf> {
    quarantine_corrupted_store_files_with(path, |source, destination| {
        std::fs::rename(source, destination)
    })
}

fn quarantine_corrupted_store_files_with<F>(path: &Path, mut rename: F) -> StoreResult<PathBuf>
where
    F: FnMut(&Path, &Path) -> std::io::Result<()>,
{
    let now = now_iso().replace(':', "-");
    let backup_id = format!("{now}.{}", uuid::Uuid::new_v4());
    let mut backup_path = path.as_os_str().to_os_string();
    backup_path.push(format!(".corrupt.{backup_id}"));
    let backup_path = PathBuf::from(backup_path);

    let mut wal_src = path.as_os_str().to_os_string();
    wal_src.push("-wal");
    let wal_src = PathBuf::from(wal_src);

    let mut shm_src = path.as_os_str().to_os_string();
    shm_src.push("-shm");
    let shm_src = PathBuf::from(shm_src);

    if path.exists() {
        rename(path, &backup_path).map_err(|error| {
            StoreError::Quarantine(std::io::Error::new(
                error.kind(),
                format!("main database quarantine rename failed: {error}"),
            ))
        })?;
    }
    if wal_src.exists() {
        let mut wal_dst = backup_path.as_os_str().to_os_string();
        wal_dst.push("-wal");
        let wal_dst = PathBuf::from(wal_dst);
        rename(&wal_src, &wal_dst).map_err(|error| {
            StoreError::Quarantine(std::io::Error::new(
                error.kind(),
                format!("WAL sidecar quarantine rename failed: {error}"),
            ))
        })?;
    }
    if shm_src.exists() {
        let mut shm_dst = backup_path.as_os_str().to_os_string();
        shm_dst.push("-shm");
        let shm_dst = PathBuf::from(shm_dst);
        rename(&shm_src, &shm_dst).map_err(|error| {
            StoreError::Quarantine(std::io::Error::new(
                error.kind(),
                format!("SHM sidecar quarantine rename failed: {error}"),
            ))
        })?;
    }

    Ok(backup_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn quarantine_sidecar_failure_is_typed_and_preserves_backup_identity() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("index.sqlite3");
        let wal = directory.path().join("index.sqlite3-wal");
        let shm = directory.path().join("index.sqlite3-shm");
        std::fs::write(&path, b"corrupt-main").expect("main");
        std::fs::write(&wal, b"corrupt-wal").expect("wal");
        std::fs::write(&shm, b"corrupt-shm").expect("shm");

        let failed = AtomicBool::new(false);
        let error = quarantine_corrupted_store_files_with(&path, |source, destination| {
            if source == wal.as_path() && !failed.swap(true, Ordering::SeqCst) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "injected sidecar rename failure",
                ));
            }
            std::fs::rename(source, destination)
        })
        .expect_err("sidecar failure must fail closed");
        let text = error.to_string();
        assert!(
            text.contains("WAL sidecar quarantine rename failed"),
            "{text}"
        );
        assert!(
            !path.exists(),
            "main must remain quarantined after sidecar failure"
        );
        assert!(wal.exists(), "failed sidecar must remain for diagnosis");
        assert!(shm.exists(), "untouched sidecar must remain for diagnosis");
        let backups = std::fs::read_dir(directory.path())
            .expect("read directory")
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|candidate| {
                candidate
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("index.sqlite3.corrupt."))
            })
            .collect::<Vec<_>>();
        assert_eq!(
            backups.len(),
            1,
            "backup identity must be unique: {backups:?}"
        );
        assert!(backups[0].exists());
    }
}
