//! Typed errors for the lightweight session index.

use thiserror::Error;

/// 保留存储、完整性、绑定和文件身份验证原因的错误。
#[derive(Debug, Error)]
pub enum StoreError {
    /// SQLite 底层操作失败。
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// 持久化 JSON 编解码失败。
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    /// 请求的索引记录不存在。
    #[error("record not found: {0}")]
    NotFound(String),
    /// 要创建的索引记录已存在。
    #[error("record already exists: {0}")]
    AlreadyExists(String),
    /// 数据库 schema 版本不是当前实现支持的版本。
    #[error("unsupported schema version {found}; supported version is {supported}")]
    UnsupportedSchema { found: u32, supported: u32 },
    /// 当前 schema 结构校验失败（表、列、索引不匹配）。
    #[error("schema structure mismatch: {0}")]
    SchemaStructure(String),
    /// 数据违反索引或文件状态不变量。
    #[error("invalid store state: {0}")]
    InvalidState(String),
    /// 初始化锁无法获取或使用。
    #[error("store initialization lock error: {0}")]
    InitializationLock(#[source] std::io::Error),
    /// 数据库损坏隔离与备份失败。
    #[error("quarantine failure: {0}")]
    Quarantine(#[source] std::io::Error),
}

impl StoreError {
    /// 判断错误是否属于可自动隔离并重建的 SQLite 结构损坏。
    /// 仅 malformed/not-a-database、unsupported schema、当前 schema 结构校验失败可恢复。
    /// 权限、路径、no-follow/file identity、初始化锁、rename/backup、disk full、busy/lock timeout 和一般 I/O 错误显式失败。
    pub fn is_recoverable_corruption(&self) -> bool {
        match self {
            StoreError::UnsupportedSchema { .. } => true,
            StoreError::SchemaStructure(_) => true,
            StoreError::Sqlite(rusqlite::Error::SqliteFailure(ffi_err, _)) => {
                matches!(
                    ffi_err.extended_code,
                    rusqlite::ffi::SQLITE_CORRUPT
                        | rusqlite::ffi::SQLITE_NOTADB
                        | rusqlite::ffi::SQLITE_CORRUPT_VTAB
                        | rusqlite::ffi::SQLITE_CORRUPT_SEQUENCE
                        | rusqlite::ffi::SQLITE_CORRUPT_INDEX
                ) || matches!(
                    ffi_err.code,
                    rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase
                )
            }
            _ => false,
        }
    }
}

/// 所有会话索引操作返回的结果类型。
pub type StoreResult<T> = Result<T, StoreError>;
