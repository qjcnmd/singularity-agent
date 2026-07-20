//! SQLite connection configuration and execution-ownership coordination.

use super::*;

/// SQLite 存储的公开描述及其支持的模式版本。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionStoreDescriptor {
    /// 存储后端名称。
    pub backend: String,
    /// 数据库路径或 SQLite 特殊路径。
    pub path: String,
    /// 当前支持的 schema 版本。
    pub schema_version: u32,
}

/// 负责 turn 生命周期、approval、追踪、产物和恢复的持久化 SQLite 存储。
pub struct SessionStore {
    pub(crate) connection: Connection,
    pub(crate) descriptor: SessionStoreDescriptor,
    pub(crate) runtime_path: Option<PathBuf>,
    pub(crate) identity_guard: Option<StoreIdentityGuard>,
}

/// 由进程持有、用于串行化线程或工作区执行的所有权保护。
pub struct WorkspaceExecutionGuard {
    pub(crate) execution_scope: WorkspaceExecutionScope,
    pub(crate) store_path: PathBuf,
    pub(crate) _lock_file: File,
}

// 执行所有权锁的粒度：优先 workspace，缺少 cwd 时使用 thread。
pub(crate) enum WorkspaceExecutionScope {
    // 以 canonical workspace 路径作为跨 thread 的执行锁范围。
    Workspace(String),
    // 无 workspace 时退化为 thread 级执行锁范围。
    Thread(String),
}

impl SessionStore {
    /// 打开 SQLite 存储，配置安全失败的 `pragma`，并执行模式检查/迁移。
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
            // SQLite opens the already-created file read/write without CREATE.
            // Revalidate the namespace before any pragma can create WAL state.
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

    /// 从已经初始化的 file-backed store 派生 request-worker 专用连接。
    ///
    /// 该入口不接受路径或 fast flag，只能使用当前 store 已固定的规范路径；
    /// 它执行 schema/marker/constraint/index/FK 结构校验，实际行仍由各读写
    /// 事务边界解码和验证。`:memory:` store 没有可安全派生的独立连接。
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
        migration::validate_v12_structure(&connection)?;
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

    /// 尝试认领执行所有权，并在返回前恢复已遗弃的工作。
    pub fn try_begin_workspace_execution(
        &self,
        thread_id: &str,
    ) -> StoreResult<Option<WorkspaceExecutionGuard>> {
        let thread = self.get_thread(thread_id)?;
        let store_path = self.runtime_path.clone().ok_or_else(|| {
            StoreError::ExecutionLock(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "workspace execution ownership requires a file-backed store",
            ))
        })?;
        let execution_scope = workspace_execution_scope(&thread);
        let lock_file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(workspace_execution_lock_path(&store_path, &execution_scope))
            .map_err(StoreError::ExecutionLock)?;
        match lock_file.try_lock() {
            Ok(()) => {
                let guard = WorkspaceExecutionGuard {
                    execution_scope,
                    store_path,
                    _lock_file: lock_file,
                };
                self.recover_abandoned_workspace_execution(&guard)?;
                Ok(Some(guard))
            }
            Err(error) => {
                let error = std::io::Error::from(error);
                if error.kind() == std::io::ErrorKind::WouldBlock {
                    Ok(None)
                } else {
                    Err(StoreError::ExecutionLock(error))
                }
            }
        }
    }

    /// 重新认领进程所有者已不存在的持久化非终态工作。
    pub fn recover_unowned_workspace_executions(&self) -> StoreResult<()> {
        let mut statement = self.connection.prepare(
            "select thread_id from turns
             where status not in (?1, ?2, ?3)
             union
             select thread_id from pending_tool_calls where execution_state = 'executing'
             order by thread_id",
        )?;
        let thread_ids = statement
            .query_map(
                params![
                    TurnStatus::Completed.to_db_text(),
                    TurnStatus::Failed.to_db_text(),
                    TurnStatus::Interrupted.to_db_text(),
                ],
                |row| row.get::<_, String>(0),
            )?
            .collect::<Result<Vec<_>, _>>()?;
        drop(statement);
        for thread_id in thread_ids {
            let _guard = self.try_begin_workspace_execution(&thread_id)?;
        }
        Ok(())
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

// 根据 thread cwd 选择 workspace 或 thread 级执行锁范围。
pub(crate) fn workspace_execution_scope(thread: &Thread) -> WorkspaceExecutionScope {
    match thread.cwd.as_deref().filter(|cwd| !cwd.trim().is_empty()) {
        Some(cwd) => WorkspaceExecutionScope::Workspace(cwd.to_string()),
        None => WorkspaceExecutionScope::Thread(thread.thread_id.clone()),
    }
}

// 生成执行所有权锁文件的稳定路径。
pub(crate) fn workspace_execution_lock_path(
    store_path: &Path,
    execution_scope: &WorkspaceExecutionScope,
) -> PathBuf {
    let lock_identity = match execution_scope {
        WorkspaceExecutionScope::Workspace(workspace) => format!("workspace:{workspace}"),
        WorkspaceExecutionScope::Thread(thread_id) => format!("thread:{thread_id}"),
    };
    let digest = Sha256::digest(lock_identity.as_bytes());
    let mut lock_path = store_path.as_os_str().to_os_string();
    lock_path.push(format!(".workspace-{digest:x}.lock"));
    PathBuf::from(lock_path)
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

// 将可选 SQL sequence 安全转换为下一个 u64 sequence。
