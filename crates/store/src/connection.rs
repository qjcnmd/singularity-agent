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
    pub fn open(path: impl AsRef<Path>) -> StoreResult<Self> {
        let path = path.as_ref();
        let _initialization_lock = acquire_store_initialization_lock(path)?;
        let in_memory = path == Path::new(":memory:");
        let identity_guard = if in_memory {
            None
        } else {
            Some(StoreIdentityGuard::open(path, true)?)
        };
        let runtime_path = identity_guard
            .as_ref()
            .map(|identity_guard| identity_guard.path.clone());
        let connection = if in_memory {
            Connection::open(path)?
        } else {
            let runtime_path = runtime_path.as_deref().ok_or_else(|| {
                StoreError::InvalidState(
                    "file-backed store is missing its canonical runtime path".to_string(),
                )
            })?;
            Connection::open_with_flags(
                runtime_path,
                OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NOFOLLOW,
            )?
        };
        if let Some(identity_guard) = &identity_guard {
            // SQLite opens the already-created file read/write without CREATE。
            // Revalidate the namespace before any pragma can create WAL state。
            identity_guard.verify()?;
        }
        configure_connection(&connection)?;
        validate_connection_pragmas(&connection, in_memory)?;
        let store = Self {
            connection,
            descriptor: SessionStoreDescriptor {
                backend: "sqlite".to_string(),
                path: path.to_string_lossy().to_string(),
                schema_version: SCHEMA_VERSION,
            },
            runtime_path,
            identity_guard,
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
