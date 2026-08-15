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
    /// 数据违反索引或文件状态不变量。
    #[error("invalid store state: {0}")]
    InvalidState(String),
    /// 初始化锁无法获取或使用。
    #[error("store initialization lock error: {0}")]
    InitializationLock(#[source] std::io::Error),
}

/// 所有会话索引操作返回的结果类型。
pub type StoreResult<T> = Result<T, StoreError>;
