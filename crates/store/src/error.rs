//! Typed storage errors and the shared SQLite enum codec.

use super::*;

/// 保留存储、完整性、绑定和执行所有权原因的错误。
#[derive(Debug, Error)]
pub enum StoreError {
    /// SQLite 底层操作失败。
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// 持久化 JSON 编解码失败。
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    /// 请求的持久化记录不存在。
    #[error("record not found: {0}")]
    NotFound(String),
    /// 要创建的记录已存在。
    #[error("record already exists: {0}")]
    AlreadyExists(String),
    /// 数据库 schema 版本高于当前实现支持的版本。
    #[error("unsupported schema version {found}; supported version is {supported}")]
    UnsupportedSchema { found: u32, supported: u32 },
    /// trace payload 与持久化完整性校验不一致。
    #[error("trace integrity check failed: {0}")]
    TraceIntegrity(String),
    /// trace 与其 turn 的身份绑定不一致。
    #[error("trace binding failed: {0}")]
    TraceBinding(#[from] TraceBindingError),
    /// 数据违反 store 的绑定或状态不变量。
    #[error("invalid store state: {0}")]
    InvalidState(String),
    /// 初始化锁无法获取或使用。
    #[error("store initialization lock error: {0}")]
    InitializationLock(#[source] std::io::Error),
    /// workspace 执行所有权锁无法获取或使用。
    #[error("workspace execution lock error: {0}")]
    ExecutionLock(#[source] std::io::Error),
    /// thread 已有另一个非终态 turn。
    #[error("thread {thread_id} already has non-terminal turn {turn_id}")]
    ThreadHasNonterminalTurn { thread_id: String, turn_id: String },
    /// workspace 已有另一个非终态 turn。
    #[error("workspace already has non-terminal turn {turn_id} in thread {thread_id}")]
    WorkspaceHasNonterminalTurn { thread_id: String, turn_id: String },
    /// 终态提交与已接受输入或暂停请求在线性化点发生竞争。
    #[error("turn {turn_id} has pending interactive input or pause control")]
    TurnBoundaryPending { turn_id: String },
}

impl StoreError {
    /// 判断错误是否只是 SQLite 的临时 busy/locked 竞争。
    pub fn is_transient_contention(&self) -> bool {
        matches!(
            self,
            Self::Sqlite(rusqlite::Error::SqliteFailure(error, _))
                if matches!(
                    error.code,
                    rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
                )
        )
    }
}

/// 所有会话存储操作返回的结果类型。
pub type StoreResult<T> = Result<T, StoreError>;

// SQLite enum columns use stable protocol/policy text rather than serde JSON scalars.
pub(crate) trait DbEnum: Clone + Sized {
    const LABEL: &'static str;

    fn to_db_text(&self) -> &'static str;

    fn from_db_text(value: &str) -> Option<Self>;
}

macro_rules! impl_db_enum {
    ($ty:ty, $label:literal) => {
        impl DbEnum for $ty {
            const LABEL: &'static str = $label;

            fn to_db_text(&self) -> &'static str {
                self.as_storage_text()
            }

            fn from_db_text(value: &str) -> Option<Self> {
                <$ty>::from_storage_text(value)
            }
        }
    };
}

impl_db_enum!(ThreadStatus, "thread status");
impl_db_enum!(TurnStatus, "turn status");
impl_db_enum!(ItemKind, "item kind");
impl_db_enum!(ItemStatus, "item status");

pub(crate) fn unknown_db_enum(label: &str, value: &str) -> StoreError {
    StoreError::InvalidState(format!("unknown {label} database value {value:?}"))
}

pub(crate) fn decode_db_enum<T: DbEnum>(value: String, column: usize) -> rusqlite::Result<T> {
    T::from_db_text(&value).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            column,
            rusqlite::types::Type::Text,
            Box::new(unknown_db_enum(T::LABEL, &value)),
        )
    })
}
