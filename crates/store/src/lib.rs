#![deny(unsafe_code)]

//! 由 SQLite 支持的会话、turn、approval、追踪、产物和恢复状态。
//!
//! 变更操作使用事务和显式绑定，使 approval 检查点、turn 结果和执行所有权能够恢复，
//! 且无需重放未知的外部副作用。

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::path::{Component, Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::ambient_authority;
use cap_std::fs::{Dir as CapabilityDir, OpenOptions as CapabilityOpenOptions};
use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params,
    params_from_iter,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use singularity_core::{contains_sensitive_text, is_protected_path};
use singularity_policy::{
    ApprovalDecision, ApprovalOutcome, ApprovalPolicy, ApprovalRequest, CommandScopeDigest,
    PermissionProfileName, PermissionResource, ToolId, WorkspaceRelativePath,
};
use singularity_protocol::{
    ArtifactRef, Item, ItemKind, ItemStatus, Thread, ThreadStatus, TraceBindingError, TraceEvent,
    Turn, TurnStatus,
};
/// 供上层重建 conversation history 的 protocol 类型。
pub use singularity_protocol::{ConversationMessage, ConversationRole};
use thiserror::Error;
use uuid::Uuid;

// Windows does not expose the handle file identity through stable
// `std::fs::MetadataExt` methods.  Keep the small FFI surface private to the
// store and expose only a safe, validated identity result to the runtime.
#[cfg(windows)]
#[allow(unsafe_code)]
mod windows_file_identity {
    use std::fs::File;
    use std::io;
    use std::mem::MaybeUninit;
    use std::os::windows::io::{AsRawHandle, RawHandle};

    #[repr(C)]
    struct FileTime {
        _low_date_time: u32,
        _high_date_time: u32,
    }

    #[repr(C)]
    struct ByHandleFileInformation {
        file_attributes: u32,
        _creation_time: FileTime,
        _last_access_time: FileTime,
        _last_write_time: FileTime,
        volume_serial_number: u32,
        _file_size_high: u32,
        _file_size_low: u32,
        number_of_links: u32,
        file_index_high: u32,
        file_index_low: u32,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        #[link_name = "GetFileInformationByHandle"]
        fn get_file_information_by_handle(
            file: RawHandle,
            information: *mut ByHandleFileInformation,
        ) -> i32;
    }

    pub(super) fn read(file: &File) -> io::Result<(u32, u64, u32, u32)> {
        let mut information = MaybeUninit::<ByHandleFileInformation>::zeroed();
        // SAFETY: `file` owns a live Windows handle and `information` points to
        // writable storage of the exact C ABI layout required by the API.
        let result = unsafe {
            get_file_information_by_handle(file.as_raw_handle(), information.as_mut_ptr())
        };
        if result == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: Windows initialized the complete structure when the call
        // returned nonzero.
        let information = unsafe { information.assume_init() };
        let file_index =
            (u64::from(information.file_index_high) << 32) | u64::from(information.file_index_low);
        Ok((
            information.volume_serial_number,
            file_index,
            information.number_of_links,
            information.file_attributes,
        ))
    }
}

const SCHEMA_VERSION: u32 = 11;
const THREAD_POLICY_SCHEMA_VERSION: u32 = 9;
const INITIAL_SCHEMA_MIGRATION: &str = "0001_initial_session_store";
// 保留历史 migration id；当前代码表达 approval/trace event history，不表达密码学 ledger。
const DURABLE_EVENT_HISTORY_SCHEMA_MIGRATION: &str = "0002_durable_ledger";
const PENDING_TOOL_CALL_SCHEMA_MIGRATION: &str = "0004_pending_tool_calls";
const STORE_HARDENING_SCHEMA_MIGRATION: &str = "0005_store_hardening";
const CONVERSATION_HISTORY_SCHEMA_MIGRATION: &str = "0006_conversation_history";
const PENDING_EXECUTION_STATE_SCHEMA_MIGRATION: &str = "0007_pending_execution_state";
const APPROVAL_EXECUTION_RECOVERY_SCHEMA_MIGRATION: &str = "0008_approval_execution_recovery";
const THREAD_POLICY_SNAPSHOT_SCHEMA_MIGRATION: &str = "0009_thread_policy_snapshot";
const STABLE_ENUM_TEXT_SCHEMA_MIGRATION: &str = "0010_stable_enum_text";
const TYPED_PERMISSION_RESOURCE_SCHEMA_MIGRATION: &str = "0011_typed_permission_resources";
// This migration existed only while the removed sidecar-run runtime was live.
// It is accepted while reading old databases and deliberately not retained in the current schema.
const RETIRED_ACTIVE_SIDECAR_RUN_SCHEMA_MIGRATION: &str = "0003_active_sidecar_runs";
const SQLITE_BUSY_TIMEOUT_MS: u64 = 5_000;
const STORE_INITIALIZATION_LOCK_RETRY_MS: u64 = 10;
const HISTORY_SCAN_BATCH_TURNS: usize = 64;
const SQLITE_FOREIGN_KEYS_PRAGMA: &str = "foreign_keys";
const SQLITE_JOURNAL_MODE_PRAGMA: &str = "journal_mode";
const SQLITE_JOURNAL_MODE_WAL: &str = "WAL";
const SQLITE_SECURE_DELETE_PRAGMA: &str = "secure_delete";
const REDACTED_ARTIFACT_VALUE: &str = "[redacted]";
const REDACTED_USER_INPUT: &str = "[redacted sensitive user input]";
const REDACTED_ASSISTANT_OUTPUT: &str = "[redacted sensitive assistant output]";
const TRACE_HASH_PREFIX: &str = "sha256:";
const ARTIFACT_URI_PREFIX: &str = "artifact://";
const ARTIFACT_KIND_MAX_BYTES: usize = 64;
const ARTIFACT_TEXT_MAX_BYTES: usize = 4_096;
const ARTIFACT_METADATA_MAX_BYTES: usize = 16 * 1024;
const ARTIFACT_METADATA_MAX_DEPTH: usize = 8;
const SHA256_HEX_LENGTH: usize = 64;
const SENSITIVE_ARTIFACT_MARKERS: [&str; 5] =
    ["api_key", "authorization", "password", "secret", "token"];
const APPROVAL_BINDING_REQUIRED: &str =
    "approval request must include explicit thread_id and turn_id";
const APPROVAL_TURN_THREAD_MISMATCH: &str = "approval request thread_id must match bound turn";
const PENDING_TOOL_CALL_ID_MISMATCH: &str =
    "pending tool call tool_call_id must match approval request";
const PENDING_TOOL_CALL_TURN_MISMATCH: &str =
    "pending tool call turn_id must match approval request";
const PENDING_TOOL_CALL_THREAD_MISMATCH: &str =
    "pending tool call thread_id must match approval request";
const PENDING_APPROVAL_ALLOW_REQUIRES_ACTIVE_THREAD: &str =
    "pending approval allow requires an active thread";

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
trait DbEnum: Clone + Sized {
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
impl_db_enum!(ApprovalOutcome, "approval outcome");
impl_db_enum!(PermissionProfileName, "sandbox mode");
impl_db_enum!(ApprovalPolicy, "approval policy");

fn unknown_db_enum(label: &str, value: &str) -> StoreError {
    StoreError::InvalidState(format!("unknown {label} database value {value:?}"))
}

fn decode_db_enum<T: DbEnum>(value: String, column: usize) -> rusqlite::Result<T> {
    T::from_db_text(&value).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            column,
            rusqlite::types::Type::Text,
            Box::new(unknown_db_enum(T::LABEL, &value)),
        )
    })
}

// Schema detection, legacy preflight/conversion, and current DDL validation live
// behind one typed initialization entry; runtime transaction paths stay here.
mod migration {
    use super::*;

    const EXPECTED_MIGRATIONS: [&str; 10] = [
        INITIAL_SCHEMA_MIGRATION,
        DURABLE_EVENT_HISTORY_SCHEMA_MIGRATION,
        PENDING_TOOL_CALL_SCHEMA_MIGRATION,
        STORE_HARDENING_SCHEMA_MIGRATION,
        CONVERSATION_HISTORY_SCHEMA_MIGRATION,
        PENDING_EXECUTION_STATE_SCHEMA_MIGRATION,
        APPROVAL_EXECUTION_RECOVERY_SCHEMA_MIGRATION,
        THREAD_POLICY_SNAPSHOT_SCHEMA_MIGRATION,
        STABLE_ENUM_TEXT_SCHEMA_MIGRATION,
        TYPED_PERMISSION_RESOURCE_SCHEMA_MIGRATION,
    ];

    const KNOWN_LEGACY_MIGRATIONS: [&str; 10] = [
        INITIAL_SCHEMA_MIGRATION,
        DURABLE_EVENT_HISTORY_SCHEMA_MIGRATION,
        RETIRED_ACTIVE_SIDECAR_RUN_SCHEMA_MIGRATION,
        PENDING_TOOL_CALL_SCHEMA_MIGRATION,
        STORE_HARDENING_SCHEMA_MIGRATION,
        CONVERSATION_HISTORY_SCHEMA_MIGRATION,
        PENDING_EXECUTION_STATE_SCHEMA_MIGRATION,
        APPROVAL_EXECUTION_RECOVERY_SCHEMA_MIGRATION,
        THREAD_POLICY_SNAPSHOT_SCHEMA_MIGRATION,
        STABLE_ENUM_TEXT_SCHEMA_MIGRATION,
    ];

    #[derive(Debug, Clone)]
    struct LegacyThreadRow {
        thread_id: String,
        model: Option<String>,
        cwd: Option<String>,
        status: ThreadStatus,
        sandbox_mode: PermissionProfileName,
        approval_policy: ApprovalPolicy,
    }

    #[derive(Debug, Clone)]
    struct LegacyTurnRow {
        turn_id: String,
        thread_id: String,
        turn_sequence: i64,
        status: TurnStatus,
        agent_loop_status: String,
    }

    #[derive(Debug, Clone)]
    struct LegacyItemRow {
        item_id: String,
        turn_id: String,
        item_sequence: i64,
        kind: ItemKind,
        payload: Value,
        status: ItemStatus,
        redacted: bool,
    }

    #[derive(Debug, Clone)]
    struct LegacyTraceRow {
        event: TraceEvent,
    }

    #[derive(Debug, Clone)]
    struct LegacyApprovalRow {
        request: ApprovalRequest,
        outcome: Option<ApprovalOutcome>,
        reason: Option<String>,
    }

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct LegacyApprovalRequestV1 {
        request_id: String,
        session_id: String,
        task_id: String,
        action: String,
        reason: String,
    }

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct LegacyApprovalRequestV4 {
        request_id: String,
        session_id: String,
        task_id: String,
        action: String,
        #[serde(default)]
        resources: Vec<String>,
        reason: String,
    }

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct LegacyApprovalRequestV5 {
        request_id: String,
        session_id: String,
        task_id: String,
        thread_id: String,
        turn_id: String,
        tool_call_id: Option<String>,
        action: String,
        #[serde(default)]
        resources: Vec<String>,
        reason: String,
    }

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct LegacyApprovalRequestCurrent {
        request_id: String,
        thread_id: String,
        turn_id: String,
        tool_call_id: Option<String>,
        action: String,
        #[serde(default)]
        resources: Vec<String>,
        reason: String,
    }

    #[derive(Debug, Clone)]
    struct LegacyDecisionRow {
        decision: ApprovalDecision,
    }

    #[derive(Debug, Clone)]
    struct LegacyPendingRow {
        request_id: String,
        thread_id: String,
        turn_id: String,
        tool_call_id: String,
        payload: String,
        execution_state: String,
    }

    #[derive(Debug, Clone)]
    struct LegacyArtifactRow {
        artifact: ArtifactRef,
    }

    #[derive(Debug, Clone)]
    struct LegacyData {
        threads: Vec<LegacyThreadRow>,
        turns: Vec<LegacyTurnRow>,
        items: Vec<LegacyItemRow>,
        traces: Vec<LegacyTraceRow>,
        approvals: Vec<LegacyApprovalRow>,
        decisions: Vec<LegacyDecisionRow>,
        pending_tool_calls: Vec<LegacyPendingRow>,
        artifacts: Vec<LegacyArtifactRow>,
    }

    pub(super) fn initialize_or_validate_schema(connection: &Connection) -> StoreResult<()> {
        let tables = user_tables(connection)?;
        if tables.is_empty() {
            create_v11_schema(connection)?;
            return Ok(());
        }

        let version = detect_schema_version(connection)?;
        if version > SCHEMA_VERSION {
            return Err(StoreError::UnsupportedSchema {
                found: version,
                supported: SCHEMA_VERSION,
            });
        }
        if version == SCHEMA_VERSION {
            return validate_v11_schema(connection);
        }

        migrate_legacy_schema(connection, version)?;
        // The migration transaction already performed the complete v11 data
        // validation before commit.  Recheck only the committed structure here;
        // callers opening an already-v11 store use the full validator above.
        validate_v11_structure(connection)
    }

    fn create_v11_schema(connection: &Connection) -> StoreResult<()> {
        let transaction = Transaction::new_unchecked(connection, TransactionBehavior::Immediate)?;
        transaction.execute_batch(V11_SCHEMA_SQL)?;
        transaction.execute_batch(V11_INDEX_SQL)?;
        for migration in EXPECTED_MIGRATIONS {
            transaction.execute(
                "insert into schema_migrations(migration_id) values(?1)",
                params![migration],
            )?;
        }
        transaction.execute(
            "insert into schema_meta(schema_version) values(?1)",
            params![SCHEMA_VERSION],
        )?;
        validate_v11_schema(&transaction)?;
        transaction.commit()?;
        Ok(())
    }

    const V11_SCHEMA_SQL: &str = r#"
create table schema_meta(
    schema_version integer not null check(schema_version = 11)
);
create table schema_migrations(
    migration_id text primary key,
    applied_at text not null default current_timestamp
);
create table threads(
    thread_id text primary key,
    model text,
    cwd text,
    status text not null default 'active'
        check(status in ('active', 'archived')),
    sandbox_mode text not null default 'workspace-write'
        check(sandbox_mode in ('read-only', 'workspace-write')),
    approval_policy text not null default 'on-request'
        check(approval_policy in ('on-request', 'never'))
);
create table turns(
    turn_id text primary key,
    thread_id text not null,
    turn_sequence integer not null check(turn_sequence > 0),
    status text not null
        check(status in ('running', 'completed', 'blocked', 'failed', 'interrupted')),
    agent_loop_status text not null,
    foreign key(thread_id) references threads(thread_id)
);
create table items(
    item_id text primary key,
    turn_id text not null,
    item_sequence integer not null check(item_sequence > 0),
    kind text not null
        check(kind in ('userMessage', 'agentMessage', 'reasoning', 'plan', 'commandExecution', 'fileChange')),
    payload text not null,
    status text not null check(status in ('started', 'completed')),
    redacted integer not null check(redacted in (0, 1)),
    foreign key(turn_id) references turns(turn_id)
);
create table trace_events(
    event_id text primary key,
    run_id text not null,
    session_id text not null default '',
    payload text not null
);
create table approvals(
    request_id text primary key,
    thread_id text not null,
    turn_id text not null,
    payload text not null,
    decision_outcome text
        check(decision_outcome in ('allow', 'deny') or decision_outcome is null),
    decision_reason text,
    foreign key(thread_id) references threads(thread_id),
    foreign key(turn_id) references turns(turn_id)
);
create table approval_decisions(
    decision_id text primary key,
    request_id text not null,
    outcome text not null check(outcome in ('allow', 'deny')),
    reason text not null,
    payload text not null,
    foreign key(request_id) references approvals(request_id)
);
create table artifact_refs(
    artifact_id text primary key,
    run_id text not null,
    item_id text,
    kind text not null,
    uri text not null,
    content_digest text not null,
    summary text not null,
    metadata text not null,
    redacted integer not null check(redacted in (0, 1))
);
create table pending_tool_calls(
    request_id text primary key,
    thread_id text not null,
    turn_id text not null,
    tool_call_id text not null,
    payload text not null,
    execution_state text not null default 'pending'
        check(execution_state in ('pending', 'executing')),
    foreign key(request_id) references approvals(request_id),
    foreign key(thread_id) references threads(thread_id),
    foreign key(turn_id) references turns(turn_id)
);
"#;

    const V11_INDEX_SQL: &str = r#"
create unique index turns_thread_sequence_unique on turns(thread_id, turn_sequence);
create unique index items_turn_sequence_unique on items(turn_id, item_sequence);
create index turns_history_lookup on turns(thread_id, status, turn_sequence);
create index items_history_lookup on items(turn_id, status, kind, item_sequence);
create unique index approval_decisions_request_unique on approval_decisions(request_id);
create index trace_run_lookup on trace_events(run_id, event_id);
create index approvals_pending_lookup on approvals(decision_outcome, request_id);
create index approvals_thread_lookup on approvals(thread_id, decision_outcome, request_id);
create index approvals_turn_lookup on approvals(turn_id, decision_outcome, request_id);
create index pending_tool_calls_turn_state on pending_tool_calls(turn_id, execution_state, request_id);
"#;

    #[derive(Debug, Clone, Copy)]
    enum LegacyTraceLayout {
        SessionBeforePayload,
        SessionAfterPayload,
    }

    #[derive(Debug, Clone, Copy)]
    enum LegacyHistoryIndexes {
        UniqueOnly,
        Full,
    }

    #[derive(Debug, Clone, Copy)]
    enum LegacyV7PendingConstraint {
        FreshFourStateCheck,
        UpgradedWithoutCheck,
    }

    // Reconstruct one exact schema shape emitted by a released v1-v9 store.
    // Product upgrades left three intentional variants behind: the v2 trace
    // column could be appended to a v1 table, v6 initially shipped only its
    // uniqueness indexes, and v7 appended execution_state without a CHECK when
    // upgrading an existing pending table.  No other structural drift is valid.
    fn legacy_reference_schema_sql(
        version: u32,
        include_retired_sidecar: bool,
        trace_layout: LegacyTraceLayout,
        history_indexes: LegacyHistoryIndexes,
        v7_pending_constraint: LegacyV7PendingConstraint,
    ) -> String {
        if version == 10 {
            let mut sql = V11_SCHEMA_SQL.replace("schema_version = 11", "schema_version = 10");
            sql.push_str(V11_INDEX_SQL);
            return sql;
        }
        let mut sql = String::new();
        if version >= 5 {
            sql.push_str("create table schema_meta(schema_version integer not null);");
        }
        if version >= 2 {
            sql.push_str(
                "create table schema_migrations(
                    migration_id text primary key,
                    applied_at text not null default current_timestamp
                );",
            );
        }
        if version >= 9 {
            sql.push_str(
                "create table threads(
                    thread_id text primary key,
                    model text,
                    cwd text,
                    status text not null,
                    sandbox_mode text not null default '\"workspace-write\"',
                    approval_policy text not null default '\"on-request\"'
                );",
            );
        } else {
            sql.push_str(
                "create table threads(
                    thread_id text primary key,
                    model text,
                    cwd text,
                    status text not null
                );",
            );
        }
        if version >= 6 {
            sql.push_str(
                "create table turns(
                    turn_id text primary key,
                    thread_id text not null,
                    turn_sequence integer not null check(turn_sequence > 0),
                    status text not null,
                    agent_loop_status text not null,
                    foreign key(thread_id) references threads(thread_id)
                );
                create table items(
                    item_id text primary key,
                    turn_id text not null,
                    item_sequence integer not null check(item_sequence > 0),
                    kind text not null,
                    payload text not null,
                    status text not null,
                    redacted integer not null check(redacted in (0, 1)),
                    foreign key(turn_id) references turns(turn_id)
                );",
            );
        } else if version >= 5 {
            sql.push_str(
                "create table turns(
                    turn_id text primary key,
                    thread_id text not null,
                    status text not null,
                    agent_loop_status text not null,
                    foreign key(thread_id) references threads(thread_id)
                );
                create table items(
                    item_id text primary key,
                    turn_id text not null,
                    kind text not null,
                    payload text not null,
                    status text not null,
                    foreign key(turn_id) references turns(turn_id)
                );",
            );
        } else {
            sql.push_str(
                "create table turns(
                    turn_id text primary key,
                    thread_id text not null,
                    status text not null,
                    agent_loop_status text not null
                );
                create table items(
                    item_id text primary key,
                    turn_id text not null,
                    kind text not null,
                    payload text not null,
                    status text not null
                );",
            );
        }
        if version == 1 {
            sql.push_str(
                "create table trace_events(
                    event_id text primary key,
                    run_id text not null,
                    payload text not null
                );",
            );
        } else {
            match trace_layout {
                LegacyTraceLayout::SessionBeforePayload => sql.push_str(
                    "create table trace_events(
                        event_id text primary key,
                        run_id text not null,
                        session_id text not null default '',
                        payload text not null
                    );",
                ),
                LegacyTraceLayout::SessionAfterPayload => sql.push_str(
                    "create table trace_events(
                        event_id text primary key,
                        run_id text not null,
                        payload text not null,
                        session_id text not null default ''
                    );",
                ),
            }
        }
        sql.push_str(
            "create table approvals(
                request_id text primary key,
                payload text not null,
                decision_outcome text,
                decision_reason text
            );",
        );
        if version >= 2 {
            if version >= 5 {
                sql.push_str(
                    "create table approval_decisions(
                        decision_id text primary key,
                        request_id text not null,
                        outcome text not null,
                        reason text not null,
                        payload text not null,
                        foreign key(request_id) references approvals(request_id)
                    );",
                );
            } else {
                sql.push_str(
                    "create table approval_decisions(
                        decision_id text primary key,
                        request_id text not null,
                        outcome text not null,
                        reason text not null,
                        payload text not null
                    );",
                );
            }
            sql.push_str(
                "create table artifact_refs(
                    artifact_id text primary key,
                    run_id text not null,
                    item_id text,
                    kind text not null,
                    uri text not null,
                    content_digest text not null,
                    summary text not null,
                    metadata text not null,
                    redacted integer not null
                );",
            );
        }
        if include_retired_sidecar {
            sql.push_str(
                "create table active_sidecar_runs(
                    turn_id text primary key,
                    thread_id text not null,
                    run_id text not null,
                    session_id text not null,
                    task_id text not null,
                    status text not null,
                    created_at text not null default current_timestamp,
                    updated_at text not null default current_timestamp
                );",
            );
        }
        match version {
            4 => sql.push_str(
                "create table pending_tool_calls(
                    request_id text primary key,
                    turn_id text not null,
                    payload text not null
                );",
            ),
            5 | 6 => sql.push_str(
                "create table pending_tool_calls(
                    request_id text primary key,
                    thread_id text not null,
                    turn_id text not null,
                    tool_call_id text not null,
                    payload text not null,
                    foreign key(request_id) references approvals(request_id),
                    foreign key(thread_id) references threads(thread_id),
                    foreign key(turn_id) references turns(turn_id)
                );",
            ),
            7 => {
                let state = match v7_pending_constraint {
                    LegacyV7PendingConstraint::FreshFourStateCheck => {
                        "execution_state text not null default 'pending' check(execution_state in ('pending', 'approved', 'executing', 'outcome_recorded'))"
                    }
                    LegacyV7PendingConstraint::UpgradedWithoutCheck => {
                        "execution_state text not null default 'pending'"
                    }
                };
                sql.push_str(&format!(
                    "create table pending_tool_calls(
                        request_id text primary key,
                        thread_id text not null,
                        turn_id text not null,
                        tool_call_id text not null,
                        payload text not null,
                        {state},
                        foreign key(request_id) references approvals(request_id),
                        foreign key(thread_id) references threads(thread_id),
                        foreign key(turn_id) references turns(turn_id)
                    );"
                ));
            }
            8 | 9 => sql.push_str(
                "create table pending_tool_calls(
                    request_id text primary key,
                    thread_id text not null,
                    turn_id text not null,
                    tool_call_id text not null,
                    payload text not null,
                    execution_state text not null default 'pending'
                        check(execution_state in ('pending', 'executing')),
                    foreign key(request_id) references approvals(request_id),
                    foreign key(thread_id) references threads(thread_id),
                    foreign key(turn_id) references turns(turn_id)
                );",
            ),
            _ => {}
        }
        if version >= 6 {
            sql.push_str(
                "create unique index turns_thread_sequence_unique
                    on turns(thread_id, turn_sequence);
                 create unique index items_turn_sequence_unique
                    on items(turn_id, item_sequence);",
            );
            if version >= 7 || matches!(history_indexes, LegacyHistoryIndexes::Full) {
                sql.push_str(
                    "create index turns_history_lookup
                        on turns(thread_id, status, turn_sequence);
                     create index items_history_lookup
                        on items(turn_id, status, kind, item_sequence);",
                );
            }
        }
        sql
    }

    fn canonical_v11_schema_sql(suffix: &str) -> String {
        if suffix.is_empty() {
            return V11_SCHEMA_SQL.to_string();
        }
        let mut sql = V11_SCHEMA_SQL.to_string();
        for table in [
            "schema_meta",
            "schema_migrations",
            "approval_decisions",
            "pending_tool_calls",
            "trace_events",
            "artifact_refs",
            "threads",
            "turns",
            "items",
            "approvals",
        ] {
            sql = sql.replace(table, &format!("{table}{suffix}"));
        }
        sql
    }

    fn user_tables(connection: &Connection) -> StoreResult<BTreeSet<String>> {
        let mut statement = connection.prepare(
            "select name from sqlite_master
         where type = 'table' and name not like 'sqlite_%' order by name",
        )?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        rows.collect::<Result<BTreeSet<_>, _>>()
            .map_err(StoreError::Sqlite)
    }

    fn table_exists(connection: &Connection, table: &str) -> StoreResult<bool> {
        connection
            .query_row(
                "select exists(select 1 from sqlite_master where type = 'table' and name = ?1)",
                params![table],
                |row| row.get::<_, bool>(0),
            )
            .map_err(StoreError::Sqlite)
    }

    fn table_columns(connection: &Connection, table: &str) -> StoreResult<BTreeSet<String>> {
        let query = format!("pragma table_info({table})");
        let mut statement = connection.prepare(&query)?;
        let rows = statement.query_map([], |row| row.get::<_, String>(1))?;
        rows.collect::<Result<BTreeSet<_>, _>>()
            .map_err(StoreError::Sqlite)
    }

    fn schema_meta_version(connection: &Connection) -> StoreResult<Option<u32>> {
        if !table_exists(connection, "schema_meta")? {
            return Ok(None);
        }
        let versions = connection
            .prepare("select schema_version from schema_meta order by rowid")?
            .query_map([], |row| row.get::<_, u64>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        match versions.as_slice() {
            [version] => u32::try_from(*version).map(Some).map_err(|_| {
                StoreError::InvalidState("schema version is out of range".to_string())
            }),
            [] => Err(StoreError::InvalidState(
                "schema_meta must contain exactly one schema version".to_string(),
            )),
            _ => Err(StoreError::InvalidState(
                "schema_meta contains multiple schema versions".to_string(),
            )),
        }
    }

    fn migration_number(migration: &str) -> Option<u32> {
        migration
            .get(0..4)
            .and_then(|prefix| prefix.parse::<u32>().ok())
    }

    fn read_migration_markers(connection: &Connection) -> StoreResult<BTreeSet<String>> {
        if !table_exists(connection, "schema_migrations")? {
            return Ok(BTreeSet::new());
        }
        let mut statement = connection.prepare("select migration_id from schema_migrations")?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        let mut markers = BTreeSet::new();
        for row in rows {
            let marker = row?;
            if marker.trim().is_empty() {
                return Err(StoreError::InvalidState(
                    "schema migration marker must not be empty".to_string(),
                ));
            }
            if !markers.insert(marker.clone()) {
                return Err(StoreError::InvalidState(format!(
                    "duplicate schema migration marker {marker}"
                )));
            }
        }
        Ok(markers)
    }

    fn detect_schema_version(connection: &Connection) -> StoreResult<u32> {
        if let Some(version) = schema_meta_version(connection)? {
            return Ok(version);
        }
        let markers = read_migration_markers(connection)?;
        if markers
            .iter()
            .any(|marker| !KNOWN_LEGACY_MIGRATIONS.contains(&marker.as_str()))
        {
            return Err(StoreError::InvalidState(
                "schema contains an unknown migration marker".to_string(),
            ));
        }
        if markers.contains(STABLE_ENUM_TEXT_SCHEMA_MIGRATION) {
            return Err(StoreError::InvalidState(
                "v10 migration marker requires schema_meta version 10".to_string(),
            ));
        }
        if let Some(version) = markers
            .iter()
            .filter_map(|marker| migration_number(marker))
            .max()
        {
            return Ok(version.min(THREAD_POLICY_SCHEMA_VERSION));
        }
        if table_has_column(connection, "threads", "approval_policy")?
            || table_has_column(connection, "threads", "sandbox_mode")?
        {
            return Ok(THREAD_POLICY_SCHEMA_VERSION);
        }
        if table_has_column(connection, "items", "item_sequence")?
            || table_has_column(connection, "turns", "turn_sequence")?
        {
            return Ok(6);
        }
        if table_has_column(connection, "pending_tool_calls", "execution_state")? {
            return Ok(7);
        }
        if table_has_column(connection, "trace_events", "session_id")? {
            return Ok(2);
        }
        Ok(1)
    }

    fn table_has_column(connection: &Connection, table: &str, column: &str) -> StoreResult<bool> {
        Ok(table_columns(connection, table)?.contains(column))
    }

    fn validate_legacy_markers(connection: &Connection, version: u32) -> StoreResult<()> {
        if version == 0 || version > SCHEMA_VERSION {
            return Err(StoreError::InvalidState(
                "legacy schema version is outside the supported range".to_string(),
            ));
        }
        let markers = read_migration_markers(connection)?;
        if version == SCHEMA_VERSION {
            let expected = EXPECTED_MIGRATIONS
                .iter()
                .map(|migration| (*migration).to_string())
                .collect::<BTreeSet<_>>();
            if markers != expected {
                return Err(StoreError::InvalidState(
                    "v11 migration markers are incomplete or unknown".to_string(),
                ));
            }
            return Ok(());
        }
        for marker in &markers {
            if !KNOWN_LEGACY_MIGRATIONS.contains(&marker.as_str()) {
                return Err(StoreError::InvalidState(format!(
                    "unknown migration marker {marker}"
                )));
            }
            if version < 10 && marker == STABLE_ENUM_TEXT_SCHEMA_MIGRATION {
                return Err(StoreError::InvalidState(
                    "v10 migration marker is present on a legacy schema".to_string(),
                ));
            }
            if migration_number(marker).is_some_and(|number| number > version) {
                return Err(StoreError::InvalidState(format!(
                    "migration marker {marker} is ahead of schema version {version}"
                )));
            }
        }
        let has_sidecar_table = table_exists(connection, "active_sidecar_runs")?;
        let has_sidecar_marker = markers.contains(RETIRED_ACTIVE_SIDECAR_RUN_SCHEMA_MIGRATION);
        if has_sidecar_table != has_sidecar_marker {
            return Err(StoreError::InvalidState(
                "retired active sidecar schema is incomplete".to_string(),
            ));
        }
        let has_migrations_table = table_exists(connection, "schema_migrations")?;
        if version == 1 {
            if has_migrations_table || !markers.is_empty() {
                return Err(StoreError::InvalidState(
                    "v1 schema must not contain migration markers".to_string(),
                ));
            }
        } else {
            if !has_migrations_table {
                return Err(StoreError::InvalidState(format!(
                    "schema version {version} is missing schema_migrations"
                )));
            }
            let mut expected = BTreeSet::new();
            for (number, marker) in [
                (1, INITIAL_SCHEMA_MIGRATION),
                (2, DURABLE_EVENT_HISTORY_SCHEMA_MIGRATION),
                (4, PENDING_TOOL_CALL_SCHEMA_MIGRATION),
                (5, STORE_HARDENING_SCHEMA_MIGRATION),
                (6, CONVERSATION_HISTORY_SCHEMA_MIGRATION),
                (7, PENDING_EXECUTION_STATE_SCHEMA_MIGRATION),
                (8, APPROVAL_EXECUTION_RECOVERY_SCHEMA_MIGRATION),
                (9, THREAD_POLICY_SNAPSHOT_SCHEMA_MIGRATION),
                (10, STABLE_ENUM_TEXT_SCHEMA_MIGRATION),
            ] {
                if number <= version {
                    expected.insert(marker.to_string());
                }
            }
            if has_sidecar_marker {
                expected.insert(RETIRED_ACTIVE_SIDECAR_RUN_SCHEMA_MIGRATION.to_string());
            }
            if markers != expected {
                return Err(StoreError::InvalidState(format!(
                    "schema version {version} migration markers are incomplete or unknown"
                )));
            }
        }
        let has_turn_sequence = table_has_column(connection, "turns", "turn_sequence")?;
        let has_item_sequence = table_has_column(connection, "items", "item_sequence")?;
        let has_item_redacted = table_has_column(connection, "items", "redacted")?;
        if version < 6 && (has_turn_sequence || has_item_sequence || has_item_redacted) {
            return Err(StoreError::InvalidState(
                "conversation history columns exist before their migration marker".to_string(),
            ));
        }
        if version >= 6 && !(has_turn_sequence && has_item_sequence && has_item_redacted) {
            return Err(StoreError::InvalidState(
                "conversation history schema is incomplete".to_string(),
            ));
        }
        let has_sandbox = table_has_column(connection, "threads", "sandbox_mode")?;
        let has_policy = table_has_column(connection, "threads", "approval_policy")?;
        if version < 9 && (has_sandbox || has_policy) {
            return Err(StoreError::InvalidState(
                "thread policy columns exist before their migration marker".to_string(),
            ));
        }
        if version >= 9 && !(has_sandbox && has_policy) {
            return Err(StoreError::InvalidState(
                "thread policy schema is incomplete".to_string(),
            ));
        }
        Ok(())
    }

    fn require_legacy_tables(connection: &Connection) -> StoreResult<()> {
        for table in ["threads", "turns", "items"] {
            if !table_exists(connection, table)? {
                return Err(StoreError::InvalidState(format!(
                    "legacy schema is missing required table {table}"
                )));
            }
        }
        Ok(())
    }

    fn decode_legacy_enum<T: DbEnum>(value: &str, legacy: bool) -> StoreResult<T> {
        if legacy {
            decode_legacy_db_enum(value)
        } else {
            T::from_db_text(value).ok_or_else(|| unknown_db_enum(T::LABEL, value))
        }
    }

    fn read_legacy_threads(
        connection: &Connection,
        legacy: bool,
    ) -> StoreResult<Vec<LegacyThreadRow>> {
        let columns = table_columns(connection, "threads")?;
        let has_sandbox = columns.contains("sandbox_mode");
        let has_policy = columns.contains("approval_policy");
        if has_sandbox != has_policy {
            return Err(StoreError::InvalidState(
                "thread policy schema is partially migrated".to_string(),
            ));
        }
        let query = if has_sandbox {
            "select thread_id, model, cwd, status, sandbox_mode, approval_policy from threads order by rowid"
        } else {
            "select thread_id, model, cwd, status, null, null from threads order by rowid"
        };
        let mut statement = connection.prepare(query)?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
            ))
        })?;
        let mut threads = Vec::new();
        for row in rows {
            let (thread_id, model, cwd, status, sandbox_mode, approval_policy) = row?;
            let sandbox_mode = sandbox_mode
                .as_deref()
                .map(|value| decode_legacy_enum::<PermissionProfileName>(value, legacy))
                .transpose()?
                .unwrap_or(PermissionProfileName::WorkspaceWrite);
            let approval_policy = approval_policy
                .as_deref()
                .map(|value| decode_legacy_enum::<ApprovalPolicy>(value, legacy))
                .transpose()?
                .unwrap_or(ApprovalPolicy::OnRequest);
            threads.push(LegacyThreadRow {
                thread_id,
                model,
                cwd,
                status: decode_legacy_enum::<ThreadStatus>(&status, legacy)?,
                sandbox_mode,
                approval_policy,
            });
        }
        Ok(threads)
    }

    fn read_legacy_turns(connection: &Connection, legacy: bool) -> StoreResult<Vec<LegacyTurnRow>> {
        let has_sequence = table_has_column(connection, "turns", "turn_sequence")?;
        let query = if has_sequence {
            "select turn_id, thread_id, turn_sequence, status, agent_loop_status from turns order by rowid"
        } else {
            "select turn_id, thread_id, null, status, agent_loop_status from turns order by rowid"
        };
        let mut statement = connection.prepare(query)?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<i64>>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
            ))
        })?;
        let mut next_by_thread = BTreeMap::<String, i64>::new();
        let mut turns = Vec::new();
        for row in rows {
            let (turn_id, thread_id, sequence, status, agent_loop_status) = row?;
            let turn_sequence = match sequence {
                Some(sequence) => sequence,
                None => {
                    let next = next_by_thread.entry(thread_id.clone()).or_insert(0);
                    *next = next.checked_add(1).ok_or_else(|| {
                        StoreError::InvalidState("turn sequence overflow".to_string())
                    })?;
                    *next
                }
            };
            turns.push(LegacyTurnRow {
                turn_id,
                thread_id,
                turn_sequence,
                status: decode_legacy_enum::<TurnStatus>(&status, legacy)?,
                agent_loop_status,
            });
        }
        Ok(turns)
    }

    fn read_legacy_items(connection: &Connection, legacy: bool) -> StoreResult<Vec<LegacyItemRow>> {
        let has_sequence = table_has_column(connection, "items", "item_sequence")?;
        let has_redacted = table_has_column(connection, "items", "redacted")?;
        let query = match (has_sequence, has_redacted) {
            (true, true) => {
                "select item_id, turn_id, item_sequence, kind, payload, status, redacted from items order by rowid"
            }
            (false, false) => {
                "select item_id, turn_id, null, kind, payload, status, null from items order by rowid"
            }
            _ => {
                return Err(StoreError::InvalidState(
                    "conversation item schema is partially migrated".to_string(),
                ));
            }
        };
        let mut statement = connection.prepare(query)?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<i64>>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, Option<i64>>(6)?,
            ))
        })?;
        let mut next_by_turn = BTreeMap::<String, i64>::new();
        let mut items = Vec::new();
        for row in rows {
            let (item_id, turn_id, sequence, kind, payload, status, redacted) = row?;
            let item_sequence = match sequence {
                Some(sequence) => sequence,
                None => {
                    let next = next_by_turn.entry(turn_id.clone()).or_insert(0);
                    *next = next.checked_add(1).ok_or_else(|| {
                        StoreError::InvalidState("item sequence overflow".to_string())
                    })?;
                    *next
                }
            };
            let kind = decode_legacy_enum::<ItemKind>(&kind, legacy)?;
            let status = decode_legacy_enum::<ItemStatus>(&status, legacy)?;
            let payload: Value = serde_json::from_str(&payload)?;
            let (payload, detected_redaction) = sanitize_item_payload(&kind, payload)?;
            let redacted = match redacted {
                Some(value) if value == 0 || value == 1 => value != 0,
                Some(_) => {
                    return Err(StoreError::InvalidState(
                        "item redaction flag is invalid".to_string(),
                    ));
                }
                None => false,
            } || detected_redaction;
            items.push(LegacyItemRow {
                item_id,
                turn_id,
                item_sequence,
                kind,
                payload,
                status,
                redacted,
            });
        }
        Ok(items)
    }

    fn read_legacy_traces(
        connection: &Connection,
        threads: &[LegacyThreadRow],
        turns: &[LegacyTurnRow],
        allow_repair: bool,
    ) -> StoreResult<Vec<LegacyTraceRow>> {
        if !table_exists(connection, "trace_events")? {
            return Ok(Vec::new());
        }
        let has_session_id = table_has_column(connection, "trace_events", "session_id")?;
        let query = if has_session_id {
            "select event_id, run_id, session_id, payload from trace_events order by rowid"
        } else {
            "select event_id, run_id, null, payload from trace_events order by rowid"
        };
        let mut statement = connection.prepare(query)?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?;
        let thread_ids = threads
            .iter()
            .map(|thread| thread.thread_id.as_str())
            .collect::<BTreeSet<_>>();
        let mut turns_by_id = BTreeMap::new();
        for turn in turns {
            if turns_by_id.insert(turn.turn_id.as_str(), turn).is_some() {
                return Err(StoreError::InvalidState(format!(
                    "duplicate turn {} while resolving traces",
                    turn.turn_id
                )));
            }
        }
        let mut traces = Vec::new();
        for row in rows {
            let (event_id, run_id, stored_session_id, payload) = row?;
            let mut event: TraceEvent = serde_json::from_str(&payload).map_err(|error| {
                StoreError::InvalidState(format!("trace {event_id} payload is invalid: {error}"))
            })?;
            if event.event_id != event_id || event.run_id != run_id {
                return Err(StoreError::InvalidState(format!(
                    "trace {event_id} columns do not match payload"
                )));
            }
            if let Some(stored_session_id) = stored_session_id.as_deref()
                && stored_session_id != event.session_id
                && !(allow_repair && stored_session_id.is_empty())
            {
                return Err(StoreError::InvalidState(format!(
                    "trace {event_id} session_id column does not match payload"
                )));
            }
            let mut session_id = event.session_id.clone();
            let mut task_id = event.task_id.clone();
            if let Some(task_id_value) = task_id.as_deref() {
                let turn = turns_by_id.get(task_id_value).ok_or_else(|| {
                    StoreError::InvalidState(format!(
                        "trace {event_id} task_id does not identify an existing turn"
                    ))
                })?;
                if turn.thread_id != event.run_id {
                    return Err(StoreError::InvalidState(format!(
                        "trace {event_id} task_id is bound to another thread"
                    )));
                }
                if session_id == turn.thread_id || session_id.is_empty() {
                    if !allow_repair {
                        return Err(StoreError::InvalidState(format!(
                            "trace {event_id} has an unnormalized turn binding"
                        )));
                    }
                    session_id = task_id_value.to_string();
                } else if session_id != task_id_value {
                    return Err(StoreError::InvalidState(format!(
                        "trace {event_id} has an ambiguous turn binding"
                    )));
                }
            } else if let Some(thread_id) = thread_ids.get(event.run_id.as_str()) {
                if session_id == *thread_id {
                    // Thread-level events use the thread as their session identity.
                } else if let Some(turn) = turns_by_id.get(session_id.as_str()) {
                    if turn.thread_id != event.run_id {
                        return Err(StoreError::InvalidState(format!(
                            "trace {event_id} session_id is bound to another thread"
                        )));
                    }
                    if !allow_repair {
                        return Err(StoreError::InvalidState(format!(
                            "trace {event_id} is missing task_id for a turn binding"
                        )));
                    }
                    task_id = Some(turn.turn_id.clone());
                } else {
                    return Err(StoreError::InvalidState(format!(
                        "trace {event_id} has an unknown turn session binding"
                    )));
                }
            } else if let Some(turn) = turns_by_id.get(session_id.as_str()) {
                if turn.thread_id != event.run_id {
                    return Err(StoreError::InvalidState(format!(
                        "trace {event_id} session_id is bound to another thread"
                    )));
                }
                if !allow_repair {
                    return Err(StoreError::InvalidState(format!(
                        "trace {event_id} is missing task_id for a turn binding"
                    )));
                }
                task_id = Some(turn.turn_id.clone());
            }
            event.task_id = task_id;
            event.session_id = session_id;
            if let Some(turn_id) = event.task_id.as_deref() {
                let turn = turns_by_id.get(turn_id).ok_or_else(|| {
                    StoreError::InvalidState(format!(
                        "trace {event_id} task_id does not identify an existing turn"
                    ))
                })?;
                event
                    .validate_turn_binding(&turn.thread_id, &turn.turn_id)
                    .map_err(|error| {
                        StoreError::InvalidState(format!(
                            "trace {event_id} binding invalid: {error}"
                        ))
                    })?;
            }
            if event.redaction_applied {
                if event.payload_hash != trace_payload_hash(&event.payload) {
                    return Err(StoreError::TraceIntegrity(format!(
                        "payload hash mismatch for {event_id}"
                    )));
                }
            } else if allow_repair {
                event = sanitize_trace_event(&event);
            } else {
                return Err(StoreError::TraceIntegrity(format!(
                    "stored trace {event_id} was not sanitized"
                )));
            }
            traces.push(LegacyTraceRow { event });
        }
        Ok(traces)
    }

    fn legacy_tool_id(action: String, context: &str) -> StoreResult<ToolId> {
        ToolId::new(action).map_err(|error| {
            StoreError::InvalidState(format!("{context} has an invalid tool id: {error}"))
        })
    }

    fn legacy_permission_resources(
        action: &ToolId,
        resources: Vec<String>,
        context: &str,
    ) -> StoreResult<Vec<PermissionResource>> {
        resources
            .into_iter()
            .map(|resource| match action.as_str() {
                "read" | "list" | "grep" | "edit" | "patch" => {
                    WorkspaceRelativePath::from_canonical(resource)
                        .map(PermissionResource::WorkspacePath)
                        .map_err(|error| {
                            StoreError::InvalidState(format!(
                                "{context} has an invalid workspace resource: {error}"
                            ))
                        })
                }
                "command" => resource
                    .strip_prefix("command_script;scope_digest:")
                    .ok_or_else(|| {
                        StoreError::InvalidState(format!(
                            "{context} command resource is not an exact historical scope"
                        ))
                    })
                    .and_then(|digest| {
                        CommandScopeDigest::new(digest.to_string())
                            .map(PermissionResource::CommandScope)
                            .map_err(|error| {
                                StoreError::InvalidState(format!(
                                    "{context} has an invalid command resource: {error}"
                                ))
                            })
                    }),
                "update_plan" if resource == action.as_str() => {
                    Ok(PermissionResource::Tool(action.clone()))
                }
                _ => Err(StoreError::InvalidState(format!(
                    "{context} resource type cannot be uniquely recovered"
                ))),
            })
            .collect()
    }

    fn current_approval_request(
        value: LegacyApprovalRequestCurrent,
        context: &str,
    ) -> StoreResult<ApprovalRequest> {
        let action = legacy_tool_id(value.action, context)?;
        let resources = legacy_permission_resources(&action, value.resources, context)?;
        Ok(ApprovalRequest {
            request_id: value.request_id,
            thread_id: value.thread_id,
            turn_id: value.turn_id,
            tool_call_id: value.tool_call_id,
            action,
            resources,
            reason: value.reason,
        })
    }

    fn decode_legacy_approval_request(
        version: u32,
        request_id: &str,
        payload: &str,
    ) -> StoreResult<ApprovalRequest> {
        let invalid = |error: serde_json::Error| {
            StoreError::InvalidState(format!(
                "approval {request_id} payload is invalid for v{version}: {error}"
            ))
        };
        let context = format!("approval {request_id}");
        match version {
            1..=3 => {
                let value: LegacyApprovalRequestV1 =
                    serde_json::from_str(payload).map_err(invalid)?;
                Ok(ApprovalRequest {
                    request_id: value.request_id,
                    thread_id: value.session_id,
                    turn_id: value.task_id,
                    tool_call_id: None,
                    action: legacy_tool_id(value.action, &context)?,
                    resources: Vec::new(),
                    reason: value.reason,
                })
            }
            4 => {
                let value: LegacyApprovalRequestV4 =
                    serde_json::from_str(payload).map_err(invalid)?;
                let action = legacy_tool_id(value.action, &context)?;
                let resources = legacy_permission_resources(&action, value.resources, &context)?;
                Ok(ApprovalRequest {
                    request_id: value.request_id,
                    thread_id: value.session_id,
                    turn_id: value.task_id,
                    tool_call_id: None,
                    action,
                    resources,
                    reason: value.reason,
                })
            }
            5 => {
                let value: LegacyApprovalRequestV5 =
                    serde_json::from_str(payload).map_err(invalid)?;
                if value.session_id != value.thread_id || value.task_id != value.turn_id {
                    return Err(StoreError::InvalidState(format!(
                        "approval {request_id} legacy and durable bindings disagree"
                    )));
                }
                let action = legacy_tool_id(value.action, &context)?;
                let resources = legacy_permission_resources(&action, value.resources, &context)?;
                Ok(ApprovalRequest {
                    request_id: value.request_id,
                    thread_id: value.thread_id,
                    turn_id: value.turn_id,
                    tool_call_id: value.tool_call_id,
                    action,
                    resources,
                    reason: value.reason,
                })
            }
            6 => {
                if let Ok(value) = serde_json::from_str::<LegacyApprovalRequestCurrent>(payload) {
                    return current_approval_request(value, &context);
                }
                let value: LegacyApprovalRequestV5 =
                    serde_json::from_str(payload).map_err(invalid)?;
                if value.session_id != value.thread_id || value.task_id != value.turn_id {
                    return Err(StoreError::InvalidState(format!(
                        "approval {request_id} legacy and durable bindings disagree"
                    )));
                }
                let action = legacy_tool_id(value.action, &context)?;
                let resources = legacy_permission_resources(&action, value.resources, &context)?;
                Ok(ApprovalRequest {
                    request_id: value.request_id,
                    thread_id: value.thread_id,
                    turn_id: value.turn_id,
                    tool_call_id: value.tool_call_id,
                    action,
                    resources,
                    reason: value.reason,
                })
            }
            7..=10 => {
                let value = serde_json::from_str::<LegacyApprovalRequestCurrent>(payload)
                    .map_err(invalid)?;
                current_approval_request(value, &context)
            }
            11 => serde_json::from_str::<ApprovalRequest>(payload).map_err(invalid),
            _ => Err(StoreError::InvalidState(format!(
                "approval {request_id} uses unsupported schema version {version}"
            ))),
        }
    }

    fn read_legacy_approvals(
        connection: &Connection,
        version: u32,
    ) -> StoreResult<Vec<LegacyApprovalRow>> {
        if !table_exists(connection, "approvals")? {
            return Ok(Vec::new());
        }
        let columns = table_columns(connection, "approvals")?;
        let has_thread_id = columns.contains("thread_id");
        let has_turn_id = columns.contains("turn_id");
        if has_thread_id != has_turn_id {
            return Err(StoreError::InvalidState(
                "approval binding projection is incomplete".to_string(),
            ));
        }
        let query = if has_thread_id {
            "select request_id, thread_id, turn_id, payload, decision_outcome, decision_reason
             from approvals order by rowid"
        } else {
            "select request_id, null, null, payload, decision_outcome, decision_reason
             from approvals order by rowid"
        };
        let mut statement = connection.prepare(query)?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
            ))
        })?;
        let mut approvals = Vec::new();
        for row in rows {
            let (request_id, stored_thread_id, stored_turn_id, payload, outcome, reason) = row?;
            let request = decode_legacy_approval_request(version, &request_id, &payload)?;
            if request.request_id != request_id
                || request.thread_id.trim().is_empty()
                || request.turn_id.trim().is_empty()
            {
                return Err(StoreError::InvalidState(format!(
                    "approval {request_id} payload binding is invalid"
                )));
            }
            if has_thread_id && (stored_thread_id.is_none() || stored_turn_id.is_none()) {
                return Err(StoreError::InvalidState(format!(
                    "approval {request_id} binding projection is null"
                )));
            }
            if stored_thread_id
                .as_deref()
                .is_some_and(|value| value != request.thread_id)
                || stored_turn_id
                    .as_deref()
                    .is_some_and(|value| value != request.turn_id)
            {
                return Err(StoreError::InvalidState(format!(
                    "approval {request_id} binding columns do not match payload"
                )));
            }
            let outcome = outcome
                .as_deref()
                .map(decode_final_approval_outcome)
                .transpose()?;
            if outcome.is_none() && reason.is_some() {
                return Err(StoreError::InvalidState(format!(
                    "approval {request_id} has a decision reason without a decision"
                )));
            }
            approvals.push(LegacyApprovalRow {
                request,
                outcome,
                reason,
            });
        }
        Ok(approvals)
    }

    fn read_legacy_decisions(connection: &Connection) -> StoreResult<Vec<LegacyDecisionRow>> {
        if !table_exists(connection, "approval_decisions")? {
            return Ok(Vec::new());
        }
        let mut statement = connection.prepare(
            "select decision_id, request_id, outcome, reason, payload
         from approval_decisions order by rowid",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
            ))
        })?;
        let mut decisions = Vec::new();
        for row in rows {
            let (decision_id, request_id, outcome, reason, payload) = row?;
            let decision: ApprovalDecision = serde_json::from_str(&payload).map_err(|error| {
                StoreError::InvalidState(format!(
                    "approval decision {decision_id} payload is invalid: {error}"
                ))
            })?;
            let expected_outcome = decode_final_approval_outcome(&outcome)?;
            if decision.decision_id != decision_id
                || decision.request_id != request_id
                || decision.outcome != expected_outcome
                || decision.reason != reason
            {
                return Err(StoreError::InvalidState(format!(
                    "approval decision {decision_id} columns do not match payload"
                )));
            }
            decisions.push(LegacyDecisionRow { decision });
        }
        Ok(decisions)
    }

    fn read_legacy_pending_tool_calls(
        connection: &Connection,
        version: u32,
        approvals: &[LegacyApprovalRow],
    ) -> StoreResult<Vec<LegacyPendingRow>> {
        if !table_exists(connection, "pending_tool_calls")? {
            return Ok(Vec::new());
        }
        let columns = table_columns(connection, "pending_tool_calls")?;
        let has_thread = columns.contains("thread_id");
        let has_tool_call = columns.contains("tool_call_id");
        let has_state = columns.contains("execution_state");
        let query = format!(
            "select request_id, {thread}, turn_id, {tool_call}, payload, {state}
         from pending_tool_calls order by rowid",
            thread = if has_thread { "thread_id" } else { "null" },
            tool_call = if has_tool_call {
                "tool_call_id"
            } else {
                "null"
            },
            state = if has_state { "execution_state" } else { "null" },
        );
        let mut statement = connection.prepare(&query)?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, Option<String>>(5)?,
            ))
        })?;
        let approval_by_id = approvals
            .iter()
            .map(|approval| (approval.request.request_id.as_str(), approval))
            .collect::<BTreeMap<_, _>>();
        let mut pending = Vec::new();
        for row in rows {
            let (request_id, thread_id, turn_id, tool_call_id, payload, state) = row?;
            let approval = approval_by_id.get(request_id.as_str()).ok_or_else(|| {
                StoreError::InvalidState(format!(
                    "pending tool call {request_id} has no approval request"
                ))
            })?;
            if version < SCHEMA_VERSION {
                return Err(StoreError::InvalidState(format!(
                    "v{version} pending AgentLoop checkpoint {request_id} cannot be migrated into the current checkpoint contract"
                )));
            }
            // Current checkpoint payloads stay opaque here: syntax validation is the only payload
            // check; Agent owns the versioned codec and all business-field validation.
            if payload.trim().is_empty() {
                return Err(StoreError::InvalidState(format!(
                    "pending tool call {request_id} payload is empty"
                )));
            }
            serde_json::from_str::<Value>(&payload).map_err(|error| {
                StoreError::InvalidState(format!(
                    "pending tool call {request_id} payload is invalid JSON: {error}"
                ))
            })?;
            let thread_id = thread_id
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| approval.request.thread_id.clone());
            let tool_call_id = tool_call_id
                .filter(|value| !value.is_empty())
                .ok_or_else(|| {
                    StoreError::InvalidState(format!(
                        "pending tool call {request_id} has no tool_call_id"
                    ))
                })?;
            let execution_state = match state.as_deref().unwrap_or("pending") {
                "pending" => "pending".to_string(),
                "executing" => "executing".to_string(),
                _ => {
                    return Err(StoreError::InvalidState(format!(
                        "pending tool call {request_id} has unknown execution state"
                    )));
                }
            };
            if execution_state != "pending" && execution_state != "executing" {
                return Err(StoreError::InvalidState(format!(
                    "pending tool call {request_id} has unknown execution state"
                )));
            }
            if thread_id != approval.request.thread_id || turn_id != approval.request.turn_id {
                return Err(StoreError::InvalidState(format!(
                    "pending tool call {request_id} binding does not match approval request"
                )));
            }
            if approval.request.tool_call_id.as_deref() != Some(tool_call_id.as_str()) {
                return Err(StoreError::InvalidState(format!(
                    "pending tool call {request_id} tool_call_id does not match approval request"
                )));
            }
            pending.push(LegacyPendingRow {
                request_id,
                thread_id,
                turn_id,
                tool_call_id,
                payload,
                execution_state,
            });
        }
        Ok(pending)
    }

    fn read_legacy_artifacts(connection: &Connection) -> StoreResult<Vec<LegacyArtifactRow>> {
        if !table_exists(connection, "artifact_refs")? {
            return Ok(Vec::new());
        }
        let mut statement = connection.prepare(
            "select artifact_id, run_id, item_id, kind, uri, content_digest,
                summary, metadata, redacted
         from artifact_refs order by rowid",
        )?;
        let rows = statement.query_map([], artifact_from_row)?;
        rows.map(|row| Ok(LegacyArtifactRow { artifact: row? }))
            .collect()
    }

    fn validate_legacy_sequences(data: &LegacyData, version: u32) -> StoreResult<()> {
        let mut thread_ids = BTreeSet::new();
        for thread in &data.threads {
            if thread.thread_id.trim().is_empty() || !thread_ids.insert(thread.thread_id.as_str()) {
                return Err(StoreError::InvalidState(format!(
                    "duplicate or empty thread {}",
                    thread.thread_id
                )));
            }
        }
        let mut turn_ids = BTreeSet::new();
        let mut turn_sequences = BTreeSet::new();
        for turn in &data.turns {
            if turn.turn_id.trim().is_empty() || turn.thread_id.trim().is_empty() {
                return Err(StoreError::InvalidState(
                    "turn id and thread binding must not be empty".to_string(),
                ));
            }
            if !thread_ids.contains(turn.thread_id.as_str()) {
                return Err(StoreError::InvalidState(format!(
                    "turn {} references a missing thread",
                    turn.turn_id
                )));
            }
            if turn.turn_sequence <= 0
                || !turn_sequences.insert((turn.thread_id.as_str(), turn.turn_sequence))
            {
                return Err(StoreError::InvalidState(format!(
                    "turn {} has an invalid or duplicate sequence",
                    turn.turn_id
                )));
            }
            if !turn_ids.insert(turn.turn_id.as_str()) {
                return Err(StoreError::InvalidState(format!(
                    "duplicate turn {}",
                    turn.turn_id
                )));
            }
        }
        let mut item_ids = BTreeSet::new();
        let mut item_sequences = BTreeSet::new();
        for item in &data.items {
            if item.item_id.trim().is_empty() || item.turn_id.trim().is_empty() {
                return Err(StoreError::InvalidState(
                    "item id and turn binding must not be empty".to_string(),
                ));
            }
            if !turn_ids.contains(item.turn_id.as_str()) {
                return Err(StoreError::InvalidState(format!(
                    "item {} references a missing turn",
                    item.item_id
                )));
            }
            if item.item_sequence <= 0
                || !item_sequences.insert((item.turn_id.as_str(), item.item_sequence))
            {
                return Err(StoreError::InvalidState(format!(
                    "item {} has an invalid or duplicate sequence",
                    item.item_id
                )));
            }
            if !item_ids.insert(item.item_id.as_str()) {
                return Err(StoreError::InvalidState(format!(
                    "duplicate item {}",
                    item.item_id
                )));
            }
        }
        let mut trace_ids = BTreeSet::new();
        for trace in &data.traces {
            if trace.event.event_id.trim().is_empty()
                || !trace_ids.insert(trace.event.event_id.as_str())
            {
                return Err(StoreError::InvalidState(format!(
                    "duplicate or empty trace event {}",
                    trace.event.event_id
                )));
            }
        }
        let mut pending_ids = BTreeSet::new();
        for pending in &data.pending_tool_calls {
            if pending.request_id.trim().is_empty()
                || !pending_ids.insert(pending.request_id.as_str())
            {
                return Err(StoreError::InvalidState(format!(
                    "duplicate or empty pending request {}",
                    pending.request_id
                )));
            }
        }
        let mut artifact_ids = BTreeSet::new();
        for artifact in &data.artifacts {
            if artifact.artifact.artifact_id.trim().is_empty()
                || !artifact_ids.insert(artifact.artifact.artifact_id.as_str())
            {
                return Err(StoreError::InvalidState(format!(
                    "duplicate or empty artifact {}",
                    artifact.artifact.artifact_id
                )));
            }
        }
        if version >= 6 {
            let has_sequence_columns = table_has_column_from_data_marker(version);
            if !has_sequence_columns {
                return Err(StoreError::InvalidState(
                    "conversation history sequence marker is inconsistent".to_string(),
                ));
            }
        }
        Ok(())
    }

    // Version six and later always carry explicit history sequences; kept as a
    // named predicate so the migration contract is visible at the validation seam.
    const fn table_has_column_from_data_marker(version: u32) -> bool {
        version >= 6
    }

    fn validate_legacy_approvals(data: &LegacyData) -> StoreResult<()> {
        let thread_ids = data
            .threads
            .iter()
            .map(|thread| thread.thread_id.as_str())
            .collect::<BTreeSet<_>>();
        let mut turns = BTreeMap::new();
        for turn in &data.turns {
            if turns.insert(turn.turn_id.as_str(), turn).is_some() {
                return Err(StoreError::InvalidState(format!(
                    "duplicate turn {}",
                    turn.turn_id
                )));
            }
        }
        let mut approvals = BTreeMap::new();
        for approval in &data.approvals {
            if approval.request.request_id.trim().is_empty()
                || approvals
                    .insert(approval.request.request_id.as_str(), approval)
                    .is_some()
            {
                return Err(StoreError::InvalidState(format!(
                    "duplicate or empty approval {}",
                    approval.request.request_id
                )));
            }
        }
        let mut decisions_by_request = BTreeMap::<&str, Vec<&LegacyDecisionRow>>::new();
        let mut decision_ids = BTreeSet::new();
        for decision in &data.decisions {
            if decision.decision.decision_id.trim().is_empty()
                || !decision_ids.insert(decision.decision.decision_id.as_str())
            {
                return Err(StoreError::InvalidState(format!(
                    "duplicate or empty approval decision {}",
                    decision.decision.decision_id
                )));
            }
            if !approvals.contains_key(decision.decision.request_id.as_str()) {
                return Err(StoreError::InvalidState(format!(
                    "approval decision {} has no request",
                    decision.decision.decision_id
                )));
            }
            decisions_by_request
                .entry(decision.decision.request_id.as_str())
                .or_default()
                .push(decision);
        }
        for approval in &data.approvals {
            let request = &approval.request;
            if !thread_ids.contains(request.thread_id.as_str()) {
                return Err(StoreError::InvalidState(format!(
                    "approval {} references a missing thread",
                    request.request_id
                )));
            }
            let turn = turns.get(request.turn_id.as_str()).ok_or_else(|| {
                StoreError::InvalidState(format!(
                    "approval {} references a missing turn",
                    request.request_id
                ))
            })?;
            if turn.thread_id != request.thread_id {
                return Err(StoreError::InvalidState(
                    APPROVAL_TURN_THREAD_MISMATCH.to_string(),
                ));
            }
            let history = decisions_by_request
                .get(request.request_id.as_str())
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            match (approval.outcome, history) {
                (None, []) => {}
                (None, _) => {
                    return Err(StoreError::InvalidState(format!(
                        "approval {} has decision history without final columns",
                        request.request_id
                    )));
                }
                (Some(expected), [decision]) => {
                    if decision.decision.outcome != expected
                        || approval.reason.as_deref() != Some(decision.decision.reason.as_str())
                    {
                        return Err(StoreError::InvalidState(format!(
                            "approval {} columns do not match decision history",
                            request.request_id
                        )));
                    }
                }
                (Some(_), _) => {
                    return Err(StoreError::InvalidState(format!(
                        "approval {} has ambiguous decision history",
                        request.request_id
                    )));
                }
            }
        }
        for pending in &data.pending_tool_calls {
            let approval = approvals.get(pending.request_id.as_str()).ok_or_else(|| {
                StoreError::InvalidState(format!(
                    "pending tool call {} has no approval request",
                    pending.request_id
                ))
            })?;
            let history = decisions_by_request
                .get(pending.request_id.as_str())
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            if approval.outcome.is_some() && approval.outcome != Some(ApprovalOutcome::Allow) {
                return Err(StoreError::InvalidState(format!(
                    "approval {} retains a checkpoint after denial",
                    pending.request_id
                )));
            }
            match (approval.outcome, pending.execution_state.as_str(), history) {
                (None, "pending", []) => {}
                (Some(ApprovalOutcome::Allow), "executing", [_]) => {}
                _ => {
                    return Err(StoreError::InvalidState(format!(
                        "approval {} has inconsistent checkpoint state",
                        pending.request_id
                    )));
                }
            }
            let turn = turns.get(pending.turn_id.as_str()).ok_or_else(|| {
                StoreError::InvalidState(format!(
                    "pending tool call {} references a missing turn",
                    pending.request_id
                ))
            })?;
            if pending.execution_state == "pending"
                && (turn.status != TurnStatus::Blocked || turn.agent_loop_status != "blocked")
            {
                return Err(StoreError::InvalidState(
                    "pending approval is not bound to a blocked turn".to_string(),
                ));
            }
        }
        Ok(())
    }

    fn read_legacy_data(
        connection: &Connection,
        version: u32,
        allow_trace_repair: bool,
    ) -> StoreResult<LegacyData> {
        require_legacy_tables(connection)?;
        if version < SCHEMA_VERSION {
            validate_legacy_schema_fingerprint(connection, version)?;
        }
        validate_legacy_markers(connection, version)?;
        fail_closed_on_foreign_key_violations(connection, "legacy preflight")?;
        let legacy = version < SCHEMA_VERSION;
        let threads = read_legacy_threads(connection, legacy)?;
        let turns = read_legacy_turns(connection, legacy)?;
        let items = read_legacy_items(connection, legacy)?;
        let approvals = read_legacy_approvals(connection, version)?;
        let decisions = read_legacy_decisions(connection)?;
        let pending_tool_calls = read_legacy_pending_tool_calls(connection, version, &approvals)?;
        let traces = read_legacy_traces(connection, &threads, &turns, allow_trace_repair)?;
        let artifacts = read_legacy_artifacts(connection)?;
        let data = LegacyData {
            threads,
            turns,
            items,
            traces,
            approvals,
            decisions,
            pending_tool_calls,
            artifacts,
        };
        validate_legacy_sequences(&data, version)?;
        validate_legacy_approvals(&data)?;
        Ok(data)
    }

    fn migrate_legacy_schema(connection: &Connection, version: u32) -> StoreResult<()> {
        let transaction = Transaction::new_unchecked(connection, TransactionBehavior::Immediate)?;
        // This is deliberately the first schema/data write boundary. Enum,
        // approval, opaque-checkpoint syntax, trace, and schema inputs are
        // validated first.
        // Foreign keys remain enabled: replacement rows are inserted parent-first
        // and legacy tables are removed child-first within this transaction.
        let data = read_legacy_data(&transaction, version, true)?;
        write_v11_tables(&transaction, &data)?;
        // Validate the fully rebuilt schema while the old database is still
        // recoverable by the transaction. A post-commit validation cannot
        // protect source tables from a malformed final schema or row.
        validate_v11_schema(&transaction)?;
        transaction.commit()?;
        Ok(())
    }

    fn write_v11_tables(connection: &Connection, data: &LegacyData) -> StoreResult<()> {
        connection.execute_batch(&canonical_v11_schema_sql("_v11"))?;
        for thread in &data.threads {
            connection.execute(
            "insert into threads_v11(thread_id, model, cwd, status, sandbox_mode, approval_policy)
             values(?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                thread.thread_id,
                thread.model,
                thread.cwd,
                thread.status.to_db_text(),
                thread.sandbox_mode.to_db_text(),
                thread.approval_policy.to_db_text(),
            ],
        )?;
        }
        for turn in &data.turns {
            connection.execute(
            "insert into turns_v11(turn_id, thread_id, turn_sequence, status, agent_loop_status)
             values(?1, ?2, ?3, ?4, ?5)",
            params![
                turn.turn_id,
                turn.thread_id,
                turn.turn_sequence,
                turn.status.to_db_text(),
                turn.agent_loop_status,
            ],
        )?;
        }
        for item in &data.items {
            connection.execute(
            "insert into items_v11(item_id, turn_id, item_sequence, kind, payload, status, redacted)
             values(?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                item.item_id,
                item.turn_id,
                item.item_sequence,
                item.kind.to_db_text(),
                serde_json::to_string(&item.payload)?,
                item.status.to_db_text(),
                item.redacted,
            ],
        )?;
        }
        for trace in &data.traces {
            let event = &trace.event;
            connection.execute(
                "insert into trace_events_v11(event_id, run_id, session_id, payload)
             values(?1, ?2, ?3, ?4)",
                params![
                    event.event_id,
                    event.run_id,
                    event.session_id,
                    serde_json::to_string(event)?,
                ],
            )?;
        }
        for approval in &data.approvals {
            let outcome = approval
                .outcome
                .map(final_approval_outcome_to_db_text)
                .transpose()?;
            connection.execute(
                "insert into approvals_v11(
                 request_id, thread_id, turn_id, payload, decision_outcome, decision_reason
             ) values(?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    approval.request.request_id,
                    approval.request.thread_id,
                    approval.request.turn_id,
                    serde_json::to_string(&approval.request)?,
                    outcome,
                    approval.reason,
                ],
            )?;
        }
        for decision in &data.decisions {
            let outcome = final_approval_outcome_to_db_text(decision.decision.outcome)?;
            connection.execute(
            "insert into approval_decisions_v11(decision_id, request_id, outcome, reason, payload)
             values(?1, ?2, ?3, ?4, ?5)",
            params![
                decision.decision.decision_id,
                decision.decision.request_id,
                outcome,
                decision.decision.reason,
                serde_json::to_string(&decision.decision)?,
            ],
        )?;
        }
        for pending in &data.pending_tool_calls {
            connection.execute(
                "insert into pending_tool_calls_v11(
                 request_id, thread_id, turn_id, tool_call_id, payload, execution_state
             ) values(?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    pending.request_id,
                    pending.thread_id,
                    pending.turn_id,
                    pending.tool_call_id,
                    &pending.payload,
                    pending.execution_state,
                ],
            )?;
        }
        for artifact in &data.artifacts {
            let artifact = &artifact.artifact;
            connection.execute(
                "insert into artifact_refs_v11(
                 artifact_id, run_id, item_id, kind, uri, content_digest,
                 summary, metadata, redacted
             ) values(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    artifact.artifact_id,
                    artifact.run_id,
                    artifact.item_id,
                    artifact.kind,
                    artifact.uri,
                    artifact.content_digest,
                    artifact.summary,
                    serde_json::to_string(&artifact.metadata)?,
                    artifact.redacted,
                ],
            )?;
        }
        for migration in EXPECTED_MIGRATIONS {
            connection.execute(
                "insert into schema_migrations_v11(migration_id) values(?1)",
                params![migration],
            )?;
        }
        connection.execute(
            "insert into schema_meta_v11(schema_version) values(?1)",
            params![SCHEMA_VERSION],
        )?;

        // Existing foreign-key tables are replaced only after all transformed rows
        // have been accepted by the new schema.
        connection.execute_batch(
            r#"
        drop table if exists active_sidecar_runs;
        drop table if exists pending_tool_calls;
        drop table if exists approval_decisions;
        drop table if exists approvals;
        drop table if exists trace_events;
        drop table if exists items;
        drop table if exists turns;
        drop table if exists threads;
        drop table if exists artifact_refs;
        drop table if exists schema_migrations;
        drop table if exists schema_meta;
        alter table schema_meta_v11 rename to schema_meta;
        alter table schema_migrations_v11 rename to schema_migrations;
        alter table threads_v11 rename to threads;
        alter table turns_v11 rename to turns;
        alter table items_v11 rename to items;
        alter table trace_events_v11 rename to trace_events;
        alter table approvals_v11 rename to approvals;
        alter table approval_decisions_v11 rename to approval_decisions;
        alter table artifact_refs_v11 rename to artifact_refs;
        alter table pending_tool_calls_v11 rename to pending_tool_calls;
        "#,
        )?;
        connection.execute_batch(V11_INDEX_SQL)?;
        Ok(())
    }

    fn validate_v11_schema(connection: &Connection) -> StoreResult<()> {
        validate_v11_structure(connection)?;
        read_legacy_data(connection, SCHEMA_VERSION, false)?;
        fail_closed_on_foreign_key_violations(connection, "v11 validation")?;
        Ok(())
    }

    // Validate the immutable v11 interface without scanning or decoding every
    // stored row.  Trusted reopen uses this after the owning process initialized
    // the database; row payloads remain validated at each read or transaction.
    pub(super) fn validate_v11_structure(connection: &Connection) -> StoreResult<()> {
        if schema_meta_version(connection)? != Some(SCHEMA_VERSION) {
            return Err(StoreError::InvalidState(
                "v11 schema_meta version is missing or inconsistent".to_string(),
            ));
        }
        validate_legacy_markers(connection, SCHEMA_VERSION)?;
        validate_canonical_v11_fingerprint(connection)
    }

    #[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
    struct SchemaFingerprint {
        objects: Vec<SchemaObjectFingerprint>,
        tables: Vec<TableFingerprint>,
    }

    #[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
    struct SchemaObjectFingerprint {
        kind: String,
        name: String,
        table_name: String,
        sql: Option<String>,
    }

    #[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
    struct TableFingerprint {
        name: String,
        columns: Vec<ColumnFingerprint>,
        indexes: Vec<IndexFingerprint>,
        foreign_keys: Vec<ForeignKeyFingerprint>,
    }

    #[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
    struct ColumnFingerprint {
        cid: i64,
        name: String,
        type_name: String,
        not_null: bool,
        default: Option<String>,
        primary_key: i64,
        hidden: i64,
    }

    #[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
    struct IndexFingerprint {
        // SQLite assigns implementation-detail names to PRIMARY KEY and UNIQUE
        // autoindexes. Their origin and complete xinfo remain part of the contract.
        explicit_name: Option<String>,
        unique: bool,
        origin: String,
        partial: bool,
        sql: Option<String>,
        columns: Vec<IndexColumnFingerprint>,
    }

    #[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
    struct IndexColumnFingerprint {
        sequence: i64,
        column_id: i64,
        name: Option<String>,
        descending: bool,
        collation: Option<String>,
        key: bool,
    }

    #[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
    struct ForeignKeyFingerprint {
        id: i64,
        sequence: i64,
        parent_table: String,
        from_column: String,
        to_column: Option<String>,
        on_update: String,
        on_delete: String,
        match_name: String,
    }

    fn validate_canonical_v11_fingerprint(connection: &Connection) -> StoreResult<()> {
        let reference = Connection::open_in_memory()?;
        reference.execute_batch(V11_SCHEMA_SQL)?;
        reference.execute_batch(V11_INDEX_SQL)?;
        let expected = schema_fingerprint(&reference)?;
        let actual = schema_fingerprint(connection)?;
        if actual != expected {
            return Err(StoreError::InvalidState(
                "v11 schema fingerprint is not canonical".to_string(),
            ));
        }
        Ok(())
    }

    fn validate_legacy_schema_fingerprint(
        connection: &Connection,
        version: u32,
    ) -> StoreResult<()> {
        let actual = schema_fingerprint(connection)?;
        let sidecar_options: &[bool] = match version {
            3 | 4 => &[true],
            5..=9 => &[false, true],
            _ => &[false],
        };
        let trace_options: &[LegacyTraceLayout] = if version >= 2 {
            &[
                LegacyTraceLayout::SessionBeforePayload,
                LegacyTraceLayout::SessionAfterPayload,
            ]
        } else {
            &[LegacyTraceLayout::SessionBeforePayload]
        };
        let history_options: &[LegacyHistoryIndexes] = if version == 6 {
            &[LegacyHistoryIndexes::UniqueOnly, LegacyHistoryIndexes::Full]
        } else {
            &[LegacyHistoryIndexes::Full]
        };
        let pending_options: &[LegacyV7PendingConstraint] = if version == 7 {
            &[
                LegacyV7PendingConstraint::FreshFourStateCheck,
                LegacyV7PendingConstraint::UpgradedWithoutCheck,
            ]
        } else {
            &[LegacyV7PendingConstraint::FreshFourStateCheck]
        };

        for &include_sidecar in sidecar_options {
            for &trace_layout in trace_options {
                for &history_indexes in history_options {
                    for &pending_constraint in pending_options {
                        let reference = Connection::open_in_memory()?;
                        reference.execute_batch(&legacy_reference_schema_sql(
                            version,
                            include_sidecar,
                            trace_layout,
                            history_indexes,
                            pending_constraint,
                        ))?;
                        if actual == schema_fingerprint(&reference)? {
                            return Ok(());
                        }
                    }
                }
            }
        }
        Err(StoreError::InvalidState(format!(
            "v{version} schema fingerprint is not a released legacy contract"
        )))
    }

    fn schema_fingerprint(connection: &Connection) -> StoreResult<SchemaFingerprint> {
        let mut object_statement = connection.prepare(
            "select type, name, tbl_name, sql from sqlite_schema
             where name not like 'sqlite_%' order by type, name",
        )?;
        let object_rows = object_statement.query_map([], |row| {
            Ok(SchemaObjectFingerprint {
                kind: normalized_identifier(row.get::<_, String>(0)?),
                name: normalized_identifier(row.get::<_, String>(1)?),
                table_name: normalized_identifier(row.get::<_, String>(2)?),
                sql: row
                    .get::<_, Option<String>>(3)?
                    .map(|sql| normalize_sql(&sql)),
            })
        })?;
        let mut objects = object_rows.collect::<Result<Vec<_>, _>>()?;
        objects.sort();

        let table_names = objects
            .iter()
            .filter(|object| object.kind == "table")
            .map(|object| object.name.clone())
            .collect::<Vec<_>>();
        let mut tables = Vec::with_capacity(table_names.len());
        for table_name in table_names {
            tables.push(TableFingerprint {
                columns: table_fingerprint_columns(connection, &table_name)?,
                indexes: table_fingerprint_indexes(connection, &table_name)?,
                foreign_keys: table_fingerprint_foreign_keys(connection, &table_name)?,
                name: table_name,
            });
        }
        tables.sort();
        Ok(SchemaFingerprint { objects, tables })
    }

    fn table_fingerprint_columns(
        connection: &Connection,
        table_name: &str,
    ) -> StoreResult<Vec<ColumnFingerprint>> {
        let mut statement = connection.prepare(
            "select cid, name, type, \"notnull\", dflt_value, pk, hidden
             from pragma_table_xinfo(?1) order by cid",
        )?;
        let rows = statement.query_map(params![table_name], |row| {
            Ok(ColumnFingerprint {
                cid: row.get(0)?,
                name: normalized_identifier(row.get::<_, String>(1)?),
                type_name: normalize_sql(&row.get::<_, String>(2)?),
                not_null: row.get::<_, i64>(3)? != 0,
                default: row
                    .get::<_, Option<String>>(4)?
                    .map(|value| normalize_sql(&value)),
                primary_key: row.get(5)?,
                hidden: row.get(6)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::Sqlite)
    }

    fn table_fingerprint_indexes(
        connection: &Connection,
        table_name: &str,
    ) -> StoreResult<Vec<IndexFingerprint>> {
        let mut statement = connection
            .prepare("select name, \"unique\", origin, partial from pragma_index_list(?1)")?;
        let rows = statement.query_map(params![table_name], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)? != 0,
                normalized_identifier(row.get::<_, String>(2)?),
                row.get::<_, i64>(3)? != 0,
            ))
        })?;
        let metadata = rows.collect::<Result<Vec<_>, _>>()?;
        let mut indexes = Vec::with_capacity(metadata.len());
        for (name, unique, origin, partial) in metadata {
            let mut xinfo_statement = connection.prepare(
                "select seqno, cid, name, \"desc\", coll, \"key\"
                 from pragma_index_xinfo(?1) order by seqno",
            )?;
            let xinfo_rows = xinfo_statement.query_map(params![&name], |row| {
                Ok(IndexColumnFingerprint {
                    sequence: row.get(0)?,
                    column_id: row.get(1)?,
                    name: row.get::<_, Option<String>>(2)?.map(normalized_identifier),
                    descending: row.get::<_, i64>(3)? != 0,
                    collation: row.get::<_, Option<String>>(4)?.map(normalized_identifier),
                    key: row.get::<_, i64>(5)? != 0,
                })
            })?;
            let columns = xinfo_rows.collect::<Result<Vec<_>, _>>()?;
            let sql = connection
                .query_row(
                    "select sql from sqlite_schema where type = 'index' and name = ?1",
                    params![&name],
                    |row| row.get::<_, Option<String>>(0),
                )
                .optional()?
                .flatten()
                .map(|value| normalize_sql(&value));
            indexes.push(IndexFingerprint {
                explicit_name: (origin == "c").then(|| normalized_identifier(&name)),
                unique,
                origin,
                partial,
                sql,
                columns,
            });
        }
        indexes.sort();
        Ok(indexes)
    }

    fn table_fingerprint_foreign_keys(
        connection: &Connection,
        table_name: &str,
    ) -> StoreResult<Vec<ForeignKeyFingerprint>> {
        let mut statement = connection.prepare(
            "select id, seq, \"table\", \"from\", \"to\", on_update, on_delete, \"match\"
             from pragma_foreign_key_list(?1) order by id, seq",
        )?;
        let rows = statement.query_map(params![table_name], |row| {
            Ok(ForeignKeyFingerprint {
                id: row.get(0)?,
                sequence: row.get(1)?,
                parent_table: normalized_identifier(row.get::<_, String>(2)?),
                from_column: normalized_identifier(row.get::<_, String>(3)?),
                to_column: row.get::<_, Option<String>>(4)?.map(normalized_identifier),
                on_update: normalized_identifier(row.get::<_, String>(5)?),
                on_delete: normalized_identifier(row.get::<_, String>(6)?),
                match_name: normalized_identifier(row.get::<_, String>(7)?),
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::Sqlite)
    }

    fn normalized_identifier(value: impl AsRef<str>) -> String {
        value.as_ref().to_ascii_lowercase()
    }

    // Normalize SQL syntax and identifier quoting only. Single-quoted literal
    // contents remain byte-for-byte significant, including case and whitespace.
    fn normalize_sql(value: &str) -> String {
        let mut normalized = String::with_capacity(value.len());
        let mut characters = value.chars().peekable();
        let mut in_literal = false;
        while let Some(character) = characters.next() {
            if in_literal {
                normalized.push(character);
                if character == '\'' {
                    if characters.peek() == Some(&'\'') {
                        normalized.push(characters.next().expect("peeked escaped quote"));
                    } else {
                        in_literal = false;
                    }
                }
                continue;
            }
            match character {
                '\'' => {
                    in_literal = true;
                    normalized.push(character);
                }
                '"' | '`' | '[' | ']' => {}
                _ if character.is_whitespace() => {}
                _ => normalized.extend(character.to_lowercase()),
            }
        }
        normalized
    }
    // Migration-only decoder: accept current plain text or the historical JSON string scalar.
    pub(super) fn decode_legacy_db_enum<T: DbEnum>(value: &str) -> StoreResult<T> {
        if let Some(decoded) = T::from_db_text(value) {
            return Ok(decoded);
        }
        let scalar = serde_json::from_str::<Value>(value)
            .ok()
            .and_then(|value| value.as_str().map(str::to_string));
        scalar
            .as_deref()
            .and_then(T::from_db_text)
            .ok_or_else(|| unknown_db_enum(T::LABEL, value))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        // Force SQLITE_FULL after preflight on the same connection. The write
        // transaction must restore every released-v9 object and row.
        #[test]
        fn migration_write_failure_rolls_back_legacy_schema_and_rows() {
            let dir = tempfile::tempdir().expect("temp dir");
            let path = dir.path().join("legacy-v9.sqlite3");
            let connection = Connection::open(path).expect("open legacy db");
            connection
                .execute_batch(&legacy_reference_schema_sql(
                    9,
                    false,
                    LegacyTraceLayout::SessionBeforePayload,
                    LegacyHistoryIndexes::Full,
                    LegacyV7PendingConstraint::FreshFourStateCheck,
                ))
                .expect("create v9 schema");
            connection
                .execute("insert into schema_meta(schema_version) values(9)", [])
                .expect("insert schema version");
            for migration in EXPECTED_MIGRATIONS.iter().copied().filter(|migration| {
                !matches!(
                    *migration,
                    STABLE_ENUM_TEXT_SCHEMA_MIGRATION | TYPED_PERMISSION_RESOURCE_SCHEMA_MIGRATION
                )
            }) {
                connection
                    .execute(
                        "insert into schema_migrations(migration_id) values(?1)",
                        [migration],
                    )
                    .expect("insert migration marker");
            }
            connection
                .execute(
                    "insert into threads(
                         thread_id, model, cwd, status, sandbox_mode, approval_policy
                     ) values('thread_fault', null, null, ?1, ?2, ?3)",
                    params![
                        serde_json::to_string(&ThreadStatus::Active).expect("thread status"),
                        serde_json::to_string(&PermissionProfileName::WorkspaceWrite)
                            .expect("sandbox mode"),
                        serde_json::to_string(&ApprovalPolicy::OnRequest).expect("approval policy"),
                    ],
                )
                .expect("insert thread");
            connection
                .execute(
                    "insert into turns(
                         turn_id, thread_id, turn_sequence, status, agent_loop_status
                     ) values('turn_fault', 'thread_fault', 1, ?1, 'completed')",
                    [serde_json::to_string(&TurnStatus::Completed).expect("turn status")],
                )
                .expect("insert turn");
            let payload = serde_json::to_string(&serde_json::json!([{
                "type": "text",
                "text": "x".repeat(4096),
            }]))
            .expect("large payload");
            for sequence in 1..=256_i64 {
                connection
                    .execute(
                        "insert into items(
                             item_id, turn_id, item_sequence, kind, payload, status, redacted
                         ) values(?1, 'turn_fault', ?2, ?3, ?4, ?5, 0)",
                        params![
                            format!("item_fault_{sequence}"),
                            sequence,
                            serde_json::to_string(&ItemKind::UserMessage).expect("item kind"),
                            payload,
                            serde_json::to_string(&ItemStatus::Completed).expect("item status"),
                        ],
                    )
                    .expect("insert item");
            }
            connection.execute_batch("vacuum;").expect("compact db");
            let page_count: i64 = connection
                .query_row("pragma page_count", [], |row| row.get(0))
                .expect("page count");
            let max_page_count = format!("pragma max_page_count = {page_count}");
            assert_eq!(
                connection
                    .query_row(&max_page_count, [], |row| row.get::<_, i64>(0))
                    .expect("set page limit"),
                page_count
            );

            let error = migrate_legacy_schema(&connection, 9)
                .expect_err("page limit must fail the schema write");
            assert!(matches!(error, StoreError::Sqlite(_)), "{error:?}");
            assert_eq!(
                connection
                    .query_row("select schema_version from schema_meta", [], |row| {
                        row.get::<_, u32>(0)
                    })
                    .expect("legacy schema version"),
                9
            );
            assert_eq!(
                connection
                    .query_row("select count(*) from items", [], |row| row.get::<_, u32>(0))
                    .expect("legacy item count"),
                256
            );
            assert_eq!(
                connection
                    .query_row(
                        "select count(*) from sqlite_schema
                         where type = 'table' and name like '%_v11'",
                        [],
                        |row| row.get::<_, u32>(0),
                    )
                    .expect("temporary table count"),
                0
            );
        }
    }
} // mod migration

fn decode_final_approval_outcome(value: &str) -> StoreResult<ApprovalOutcome> {
    match migration::decode_legacy_db_enum::<ApprovalOutcome>(value)? {
        ApprovalOutcome::Allow => Ok(ApprovalOutcome::Allow),
        ApprovalOutcome::Deny => Ok(ApprovalOutcome::Deny),
        ApprovalOutcome::Defer => Err(StoreError::InvalidState(
            "defer approval outcome must remain pending".to_string(),
        )),
    }
}

fn final_approval_outcome_to_db_text(outcome: ApprovalOutcome) -> StoreResult<&'static str> {
    match outcome {
        ApprovalOutcome::Allow => Ok(ApprovalOutcome::Allow.to_db_text()),
        ApprovalOutcome::Deny => Ok(ApprovalOutcome::Deny.to_db_text()),
        ApprovalOutcome::Defer => Err(StoreError::InvalidState(
            "defer approval outcome must remain pending".to_string(),
        )),
    }
}

// 读取 approval 行时同时验证列、payload 和 request 的 turn 绑定。
fn decode_stored_approval_request_row(
    connection: &Connection,
    request_id: &str,
    stored_thread_id: &str,
    stored_turn_id: &str,
    payload: &str,
    outcome: Option<&str>,
    reason: Option<&str>,
) -> StoreResult<ApprovalRequest> {
    let request = decode_stored_approval_request_columns(
        request_id,
        stored_thread_id,
        stored_turn_id,
        payload,
        outcome,
        reason,
    )?;
    ensure_request_turn_binding(connection, &request)?;
    Ok(request)
}

fn decode_stored_approval_request_columns(
    request_id: &str,
    stored_thread_id: &str,
    stored_turn_id: &str,
    payload: &str,
    outcome: Option<&str>,
    reason: Option<&str>,
) -> StoreResult<ApprovalRequest> {
    let request: ApprovalRequest = serde_json::from_str(payload).map_err(|error| {
        StoreError::InvalidState(format!("approval {request_id} payload is invalid: {error}"))
    })?;
    if request.request_id != request_id
        || request.thread_id != stored_thread_id
        || request.turn_id != stored_turn_id
        || request.thread_id.trim().is_empty()
        || request.turn_id.trim().is_empty()
    {
        return Err(StoreError::InvalidState(format!(
            "approval {request_id} payload binding is invalid"
        )));
    }
    let outcome = outcome.map(decode_final_approval_outcome).transpose()?;
    if outcome.is_none() && reason.is_some() {
        return Err(StoreError::InvalidState(format!(
            "approval {request_id} has a decision reason without a decision"
        )));
    }
    Ok(request)
}

// 读取 decision 行时同时验证 decision 表、approval 最终列和 payload。
fn decode_stored_approval_decision_row(
    connection: &Connection,
    decision_id: &str,
    request_id: &str,
    outcome: &str,
    reason: &str,
    payload: &str,
) -> StoreResult<ApprovalDecision> {
    let decision =
        decode_stored_approval_decision_columns(decision_id, request_id, outcome, reason, payload)?;
    let (
        approval_request_id,
        approval_thread_id,
        approval_turn_id,
        approval_payload,
        approval_outcome,
        approval_reason,
    ): (
        String,
        String,
        String,
        String,
        Option<String>,
        Option<String>,
    ) = connection
        .query_row(
            "select request_id, thread_id, turn_id, payload, decision_outcome, decision_reason
             from approvals where request_id = ?1",
            params![request_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            },
        )
        .map_err(|error| match error {
            rusqlite::Error::QueryReturnedNoRows => StoreError::InvalidState(format!(
                "approval decision {decision_id} has no approval request"
            )),
            other => StoreError::Sqlite(other),
        })?;
    let _request = decode_stored_approval_request_row(
        connection,
        &approval_request_id,
        &approval_thread_id,
        &approval_turn_id,
        &approval_payload,
        approval_outcome.as_deref(),
        approval_reason.as_deref(),
    )?;
    if approval_request_id != request_id
        || approval_outcome.as_deref() != Some(outcome)
        || approval_reason.as_deref() != Some(reason)
    {
        return Err(StoreError::InvalidState(format!(
            "approval decision {decision_id} does not match approval final columns"
        )));
    }
    Ok(decision)
}

fn decode_stored_approval_decision_columns(
    decision_id: &str,
    request_id: &str,
    outcome: &str,
    reason: &str,
    payload: &str,
) -> StoreResult<ApprovalDecision> {
    let decision: ApprovalDecision = serde_json::from_str(payload).map_err(|error| {
        StoreError::InvalidState(format!(
            "approval decision {decision_id} payload is invalid: {error}"
        ))
    })?;
    let expected_outcome = decode_final_approval_outcome(outcome)?;
    if decision.decision_id != decision_id
        || decision.request_id != request_id
        || decision.outcome != expected_outcome
        || decision.reason != reason
    {
        return Err(StoreError::InvalidState(format!(
            "approval decision {decision_id} columns do not match payload"
        )));
    }
    Ok(decision)
}

fn validate_turn_trace_binding(
    event: &TraceEvent,
    thread_id: &str,
    turn_id: &str,
) -> StoreResult<()> {
    event.validate_turn_binding(thread_id, turn_id)?;
    Ok(())
}

// Public generic trace append may store external runs, but it cannot weaken a
// trace that identifies an existing thread or turn.
fn validate_public_trace_binding(connection: &Connection, event: &TraceEvent) -> StoreResult<()> {
    validate_public_trace_bindings(connection, std::slice::from_ref(event))
}

// Batch-prefetch the small set of thread/turn rows needed by a trace page.
// This keeps payload decoding row-local without issuing one binding query per event.
fn validate_public_trace_bindings(
    connection: &Connection,
    events: &[TraceEvent],
) -> StoreResult<()> {
    let thread_ids = events
        .iter()
        .map(|event| event.run_id.clone())
        .collect::<BTreeSet<_>>();
    let turn_ids = events
        .iter()
        .flat_map(|event| {
            event
                .task_id
                .iter()
                .chain(std::iter::once(&event.session_id))
                .cloned()
        })
        .collect::<BTreeSet<_>>();

    let existing_threads = select_trace_thread_ids(connection, &thread_ids)?;
    let turns = select_trace_turn_bindings(connection, &turn_ids)?;
    for event in events {
        let thread_exists = existing_threads.contains(&event.run_id);
        let turn_id = event.task_id.as_deref().unwrap_or(&event.session_id);
        match (thread_exists, turns.get(turn_id)) {
            (false, None) if event.task_id.is_none() => {}
            (false, None) => {
                return Err(StoreError::InvalidState(
                    "trace task_id must identify an existing turn".to_string(),
                ));
            }
            (true, None) if event.task_id.is_none() && event.session_id == event.run_id => {}
            (true, Some(thread_id)) | (false, Some(thread_id)) => {
                validate_turn_trace_binding(event, thread_id, turn_id)?;
            }
            (true, None) => {
                return Err(StoreError::InvalidState(
                    "trace for an existing thread must bind to the thread or an existing turn"
                        .to_string(),
                ));
            }
        }
    }
    Ok(())
}

fn select_trace_thread_ids(
    connection: &Connection,
    thread_ids: &BTreeSet<String>,
) -> StoreResult<BTreeSet<String>> {
    if thread_ids.is_empty() {
        return Ok(BTreeSet::new());
    }
    let placeholders = std::iter::repeat_n("?", thread_ids.len())
        .collect::<Vec<_>>()
        .join(", ");
    let query = format!("select thread_id from threads where thread_id in ({placeholders})");
    let mut statement = connection.prepare(&query)?;
    let rows = statement.query_map(params_from_iter(thread_ids.iter()), |row| row.get(0))?;
    rows.collect::<Result<BTreeSet<_>, _>>()
        .map_err(StoreError::Sqlite)
}

fn select_trace_turn_bindings(
    connection: &Connection,
    turn_ids: &BTreeSet<String>,
) -> StoreResult<BTreeMap<String, String>> {
    if turn_ids.is_empty() {
        return Ok(BTreeMap::new());
    }
    let placeholders = std::iter::repeat_n("?", turn_ids.len())
        .collect::<Vec<_>>()
        .join(", ");
    let query = format!("select turn_id, thread_id from turns where turn_id in ({placeholders})");
    let mut statement = connection.prepare(&query)?;
    let rows = statement.query_map(params_from_iter(turn_ids.iter()), |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut bindings = BTreeMap::new();
    for row in rows {
        let (turn_id, thread_id) = row?;
        if bindings.insert(turn_id.clone(), thread_id).is_some() {
            return Err(StoreError::InvalidState(format!(
                "duplicate turn binding {turn_id}"
            )));
        }
    }
    Ok(bindings)
}

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

/// 按模型重建所需顺序排列的一页已完成对话历史。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ThreadHistoryPage {
    /// 按 turn 顺序排列的 conversation message。
    pub messages: Vec<ConversationMessage>,
    /// 下一页查询使用的 exclusive turn sequence。
    pub next_before_turn_sequence: Option<u64>,
}

/// 创建 turn、用户条目、追踪和初始历史页后得到的原子结果。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct StartedTurn {
    /// 新建的 turn。
    pub turn: Turn,
    /// 清理后的 user input item。
    pub item: Item,
    /// 与创建操作绑定的 trace event。
    pub trace: TraceEvent,
    /// 创建边界之前可供模型重建的历史页。
    pub history: ThreadHistoryPage,
}

/// 尚未持久化、仅用于在 turn 初始化前绑定进程内资源的唯一标识。
#[derive(Debug)]
pub struct AllocatedTurnId(String);

impl AllocatedTurnId {
    /// 返回将由 Store 原子持久化的 turn id。
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// 使用预分配 id 创建 started turn 事务所需的字段。
pub struct CreateStartedTurnParams<'a> {
    /// 所属 thread。
    pub thread_id: &'a str,
    /// 初始 AgentLoop 状态。
    pub agent_loop_status: &'a str,
    /// 未清理的用户输入。
    pub input: Value,
    /// started trace 的组件名。
    pub component: &'a str,
    /// started trace 的摘要。
    pub summary: &'a str,
    /// 创建边界需要读取的历史 turn 上限。
    pub history_turn_limit: usize,
}

/// turn 的原子结果，以及相关的持久化计划、助手条目和追踪（如有）。
#[derive(Debug, Clone, PartialEq)]
pub struct CommittedTurnOutcome {
    /// 提交后的 turn 状态。
    pub turn: Turn,
    /// 可选的持久化 plan item。
    pub plan_item: Option<Item>,
    /// 可选的持久化 assistant item。
    pub assistant_item: Option<Item>,
    /// 与结果提交绑定的 trace event。
    pub trace: TraceEvent,
}

/// 终态提交时拥有该状态归约权的 typed 原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnOutcomeAuthority {
    /// 普通 AgentLoop/用户路径，必须遵守 cancel-requested 的取消合同。
    AgentLoop,
    /// 监视基础设施故障，唯一允许的归约是 Failed，不伪装成 Interrupted。
    InfrastructureFailure,
}

/// 负责 turn 生命周期、approval、追踪、产物和恢复的持久化 SQLite 存储。
pub struct SessionStore {
    connection: Connection,
    descriptor: SessionStoreDescriptor,
    runtime_path: Option<PathBuf>,
    identity_guard: Option<StoreIdentityGuard>,
}

#[derive(Debug)]
struct StoreIdentityGuard {
    path: PathBuf,
    identity: StoreFileIdentity,
    _file: File,
    parent: CapabilityDir,
    file_name: OsString,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StoreFileIdentity {
    #[cfg(unix)]
    Unix { device: u64, inode: u64 },
    #[cfg(windows)]
    Windows {
        volume_serial_number: u32,
        file_index: u64,
    },
    #[cfg(not(any(unix, windows)))]
    Unsupported,
}

/// 由进程持有、用于串行化线程或工作区执行的所有权保护。
pub struct WorkspaceExecutionGuard {
    execution_scope: WorkspaceExecutionScope,
    store_path: PathBuf,
    _lock_file: File,
}

// 执行所有权锁的粒度：优先 workspace，缺少 cwd 时使用 thread。
enum WorkspaceExecutionScope {
    // 以 canonical workspace 路径作为跨 thread 的执行锁范围。
    Workspace(String),
    // 无 workspace 时退化为 thread 级执行锁范围。
    Thread(String),
}

/// approval 决定，以及 `AppServer` 所需的检查点和追踪数据。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RecordedApprovalDecision {
    /// 被决定的 approval 请求。
    pub request: ApprovalRequest,
    /// 已记录的 approval decision。
    pub decision: ApprovalDecision,
    /// claim 事务提交时绑定 turn 的一致性快照，供后续补偿路径复用。
    pub turn: Turn,
    /// 允许继续执行时保留的 pending tool call checkpoint。
    pub pending_tool_call: Option<Value>,
    /// 记录决定的 trace event。
    pub trace: TraceEvent,
}

/// 注册脱敏、内容寻址产物引用的输入。
pub struct RegisterArtifactRefParams<'a> {
    /// artifact 所属 run。
    pub run_id: &'a str,
    /// 可选的来源 item。
    pub item_id: Option<&'a str>,
    /// artifact 类型。
    pub kind: &'a str,
    /// artifact URI。
    pub uri: &'a str,
    /// 内容摘要。
    pub content_digest: &'a str,
    /// 面向用户的摘要。
    pub summary: &'a str,
    /// 需要持久化并按规则脱敏的 metadata。
    pub metadata: Value,
}

/// 为一个终止、已中断或 approval 阻塞的 turn 结果提交的持久化字段。
pub struct CommitTurnOutcomeParams<'a> {
    /// turn 的目标终态。
    pub status: TurnStatus,
    /// AgentLoop 的目标状态。
    pub agent_loop_status: &'a str,
    /// 可选的 assistant 增量。
    pub assistant_delta: Option<&'a str>,
    /// 可选的 plan payload。
    pub plan: Option<&'a Value>,
    /// 与提交绑定的 trace event。
    pub trace: &'a TraceEvent,
}

// SessionStore 的公开生命周期、恢复、历史、trace、approval 与 artifact 边界。
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
        migration::validate_v11_structure(&connection)?;
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

    /// 创建 thread 并持久化其初始状态。
    pub fn create_thread(&self, model: Option<&str>, cwd: Option<&str>) -> StoreResult<Thread> {
        self.create_thread_with_policy(
            model,
            cwd,
            PermissionProfileName::WorkspaceWrite,
            ApprovalPolicy::OnRequest,
        )
    }

    /// 创建带有不可变 sandbox/approval 快照的 thread。
    pub fn create_thread_with_policy(
        &self,
        model: Option<&str>,
        cwd: Option<&str>,
        sandbox_mode: PermissionProfileName,
        approval_policy: ApprovalPolicy,
    ) -> StoreResult<Thread> {
        let thread = Self::new_thread(model, cwd, sandbox_mode, approval_policy);
        Self::insert_thread(&self.connection, &thread)?;
        Ok(thread)
    }

    /// 按持久化顺序列出所有 thread。
    pub fn list_threads(&self) -> StoreResult<Vec<Thread>> {
        let mut statement = self.connection.prepare(
            "select thread_id, model, cwd, status, sandbox_mode, approval_policy
                 from threads order by rowid",
        )?;
        let rows = statement.query_map([], |row| self.thread_from_row(row))?;
        let mut threads = Vec::new();
        for row in rows {
            threads.push(row?);
        }
        Ok(threads)
    }

    /// 读取指定 thread，不存在时返回 NotFound。
    pub fn get_thread(&self, thread_id: &str) -> StoreResult<Thread> {
        self.connection
            .query_row(
                "select thread_id, model, cwd, status, sandbox_mode, approval_policy
                 from threads where thread_id = ?1",
                params![thread_id],
                |row| self.thread_from_row(row),
            )
            .map_err(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => {
                    StoreError::NotFound(format!("thread {thread_id}"))
                }
                other => StoreError::Sqlite(other),
            })
    }

    /// 原子更新 thread 状态，并在归档前检查非终态 turn。
    pub fn update_thread_status(
        &self,
        thread_id: &str,
        status: ThreadStatus,
    ) -> StoreResult<Thread> {
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        if status == ThreadStatus::Archived {
            Self::ensure_thread_has_no_nonterminal_turn(&transaction, thread_id)?;
        }
        let changed = transaction.execute(
            "update threads set status = ?1 where thread_id = ?2",
            params![status.to_db_text(), thread_id],
        )?;
        if changed == 0 {
            return Err(StoreError::NotFound(format!("thread {thread_id}")));
        }
        transaction.commit()?;
        self.get_thread(thread_id)
    }

    /// 删除 thread 及其绑定的 turn、item、trace、approval 和 artifact。
    pub fn delete_thread(&self, thread_id: &str) -> StoreResult<()> {
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        Self::ensure_thread_has_no_nonterminal_turn(&transaction, thread_id)?;
        // 先收集所有显式或 checkpoint 绑定的 approval request。
        let mut approval_request_ids = BTreeSet::new();
        {
            let mut statement = transaction.prepare(
                "select request_id, thread_id, turn_id, payload, decision_outcome, decision_reason
                 from approvals where thread_id = ?1",
            )?;
            let rows = statement.query_map(params![thread_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                ))
            })?;
            for row in rows {
                let (request_id, stored_thread_id, stored_turn_id, payload, outcome, reason) = row?;
                let request = decode_stored_approval_request_row(
                    &transaction,
                    &request_id,
                    &stored_thread_id,
                    &stored_turn_id,
                    &payload,
                    outcome.as_deref(),
                    reason.as_deref(),
                )?;
                if request.thread_id != thread_id {
                    return Err(StoreError::InvalidState(
                        "approval thread projection is inconsistent during deletion".to_string(),
                    ));
                }
                approval_request_ids.insert(request_id);
            }
        }
        {
            let mut statement = transaction.prepare(
                "select request_id from pending_tool_calls where thread_id = ?1 or turn_id in (select turn_id from turns where thread_id = ?1)",
            )?;
            let rows = statement.query_map(params![thread_id], |row| row.get::<_, String>(0))?;
            for row in rows {
                approval_request_ids.insert(row?);
            }
        }
        // 再按依赖顺序删除 decision history、checkpoint 和 approval。
        for request_id in approval_request_ids {
            transaction.execute(
                "delete from approval_decisions where request_id = ?1",
                params![request_id],
            )?;
            transaction.execute(
                "delete from pending_tool_calls where request_id = ?1",
                params![request_id],
            )?;
            transaction.execute(
                "delete from approvals where request_id = ?1",
                params![request_id],
            )?;
        }
        // 最后清理 thread 的 turn、item、trace、artifact 和自身记录。
        transaction.execute(
            "delete from pending_tool_calls where turn_id in (select turn_id from turns where thread_id = ?1)",
            params![thread_id],
        )?;
        transaction.execute(
            "delete from items where turn_id in (select turn_id from turns where thread_id = ?1)",
            params![thread_id],
        )?;
        transaction.execute("delete from turns where thread_id = ?1", params![thread_id])?;
        transaction.execute(
            "delete from trace_events where run_id = ?1 or session_id = ?1",
            params![thread_id],
        )?;
        transaction.execute(
            "delete from artifact_refs where run_id = ?1",
            params![thread_id],
        )?;
        let changed = transaction.execute(
            "delete from threads where thread_id = ?1",
            params![thread_id],
        )?;
        if changed == 0 {
            return Err(StoreError::NotFound(format!("thread {thread_id}")));
        }
        transaction.commit()?;
        Ok(())
    }

    /// 原子创建 thread 及其初始 trace。
    pub fn create_thread_with_trace(
        &self,
        model: Option<&str>,
        cwd: Option<&str>,
        component: &str,
        summary: &str,
    ) -> StoreResult<(Thread, TraceEvent)> {
        self.create_thread_with_trace_and_policy(
            model,
            cwd,
            PermissionProfileName::WorkspaceWrite,
            ApprovalPolicy::OnRequest,
            component,
            summary,
        )
    }

    /// 原子创建带有 policy 快照的 thread 及其初始 trace。
    pub fn create_thread_with_trace_and_policy(
        &self,
        model: Option<&str>,
        cwd: Option<&str>,
        sandbox_mode: PermissionProfileName,
        approval_policy: ApprovalPolicy,
        component: &str,
        summary: &str,
    ) -> StoreResult<(Thread, TraceEvent)> {
        let transaction = self.connection.unchecked_transaction()?;
        let thread = Self::new_thread(model, cwd, sandbox_mode, approval_policy);
        Self::insert_thread(&transaction, &thread)?;
        let trace = TraceEvent::new(
            format!("trace_{}", thread.thread_id),
            thread.thread_id.clone(),
            thread.thread_id.clone(),
            component,
            summary,
        );
        let trace = Self::insert_trace(&transaction, &trace)?;
        transaction.commit()?;
        Ok((thread, trace))
    }

    /// 创建一个受 thread 与 workspace 并发约束的 turn。
    pub fn create_turn(&self, thread_id: &str, agent_loop_status: &str) -> StoreResult<Turn> {
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        Self::ensure_active_thread(&transaction, thread_id)?;
        Self::ensure_workspace_has_no_nonterminal_turn(&transaction, thread_id, None)?;
        let turn_sequence = Self::next_turn_sequence(&transaction, thread_id)?;
        let turn = Self::new_turn(thread_id, agent_loop_status);
        Self::insert_turn(&transaction, &turn, turn_sequence)?;
        transaction.commit()?;
        Ok(turn)
    }

    /// 创建 turn、user input 和 trace，不返回历史页。
    pub fn create_turn_with_input_and_trace(
        &self,
        thread_id: &str,
        agent_loop_status: &str,
        input: Value,
        component: &str,
        summary: &str,
    ) -> StoreResult<(Turn, Item, TraceEvent)> {
        let started = self.create_turn_with_input_trace_and_history(
            thread_id,
            agent_loop_status,
            input,
            component,
            summary,
            0,
        )?;
        Ok((started.turn, started.item, started.trace))
    }

    /// 原子地创建 turn、清理其输入、记录追踪，并读取此前的历史。
    pub fn create_turn_with_input_trace_and_history(
        &self,
        thread_id: &str,
        agent_loop_status: &str,
        input: Value,
        component: &str,
        summary: &str,
        history_turn_limit: usize,
    ) -> StoreResult<StartedTurn> {
        self.create_allocated_turn_with_input_trace_and_history(
            Self::allocate_turn_id(),
            CreateStartedTurnParams {
                thread_id,
                agent_loop_status,
                input,
                component,
                summary,
                history_turn_limit,
            },
        )
    }

    /// 分配一个尚未持久化的 turn id，供调用方先建立所有易失败的运行资源。
    pub fn allocate_turn_id() -> AllocatedTurnId {
        AllocatedTurnId(format!("turn_{}", short_id()))
    }

    /// 使用预分配 id 原子创建 turn、输入、trace 和此前历史。
    pub fn create_allocated_turn_with_input_trace_and_history(
        &self,
        allocated_turn_id: AllocatedTurnId,
        params: CreateStartedTurnParams<'_>,
    ) -> StoreResult<StartedTurn> {
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        Self::ensure_active_thread(&transaction, params.thread_id)?;
        Self::ensure_workspace_has_no_nonterminal_turn(&transaction, params.thread_id, None)?;
        let history = Self::read_thread_history_from(
            &transaction,
            params.thread_id,
            None,
            params.history_turn_limit,
        )?;
        let turn_sequence = Self::next_turn_sequence(&transaction, params.thread_id)?;
        let turn = Self::new_turn_with_id(
            allocated_turn_id.0,
            params.thread_id,
            params.agent_loop_status,
        );
        Self::insert_turn(&transaction, &turn, turn_sequence)?;
        let item_sequence = Self::next_item_sequence(&transaction, &turn.turn_id)?;
        let (input, redacted) = sanitize_item_payload(&ItemKind::UserMessage, params.input)?;
        let item = Self::new_item(&turn.turn_id, ItemKind::UserMessage, input);
        Self::insert_item(&transaction, &item, item_sequence, redacted)?;
        let trace = TraceEvent::for_turn(
            format!("trace_{}", turn.turn_id),
            params.thread_id,
            turn.turn_id.clone(),
            params.component,
            params.summary,
        );
        let trace = Self::insert_turn_trace(&transaction, &trace, params.thread_id, &turn.turn_id)?;
        transaction.commit()?;
        Ok(StartedTurn {
            turn,
            item,
            trace,
            history,
        })
    }

    /// 读取指定 thread 的 completed conversation 历史页。
    pub fn read_thread_history(
        &self,
        thread_id: &str,
        before_turn_sequence: Option<u64>,
        turn_limit: usize,
    ) -> StoreResult<ThreadHistoryPage> {
        let transaction = self.connection.unchecked_transaction()?;
        if !Self::exists_in_transaction(
            &transaction,
            "select 1 from threads where thread_id = ?1",
            thread_id,
        )? {
            return Err(StoreError::NotFound(format!("thread {thread_id}")));
        }
        let history = Self::read_thread_history_from(
            &transaction,
            thread_id,
            before_turn_sequence,
            turn_limit,
        )?;
        transaction.commit()?;
        Ok(history)
    }

    /// 在指定 turn sequence 之前读取一页历史。
    pub fn read_thread_history_before_turn(
        &self,
        thread_id: &str,
        turn_id: &str,
        turn_limit: usize,
    ) -> StoreResult<ThreadHistoryPage> {
        let transaction = self.connection.unchecked_transaction()?;
        let turn_sequence = transaction
            .query_row(
                "select turn_sequence from turns where turn_id = ?1 and thread_id = ?2",
                params![turn_id, thread_id],
                |row| row.get::<_, i64>(0),
            )
            .map_err(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => {
                    StoreError::NotFound(format!("turn {turn_id} in thread {thread_id}"))
                }
                other => StoreError::Sqlite(other),
            })?;
        let history = Self::read_thread_history_from(
            &transaction,
            thread_id,
            Some(sequence_from_sql(turn_sequence, "turn sequence")?),
            turn_limit,
        )?;
        transaction.commit()?;
        Ok(history)
    }

    /// 读取指定 turn，不存在时返回 NotFound。
    pub fn get_turn(&self, turn_id: &str) -> StoreResult<Turn> {
        self.connection
            .query_row(
                "select turn_id, thread_id, status, agent_loop_status from turns where turn_id = ?1",
                params![turn_id],
                |row| self.turn_from_row(row),
            )
            .map_err(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => StoreError::NotFound(format!("turn {turn_id}")),
                other => StoreError::Sqlite(other),
            })
    }

    /// 更新 turn status，并拒绝覆盖已终态的 turn。
    pub fn update_turn_status(&self, turn_id: &str, status: TurnStatus) -> StoreResult<Turn> {
        self.ensure_turn_status_update_allowed(turn_id, &status, None)?;
        let changed = self.connection.execute(
            "update turns set status = ?1 where turn_id = ?2",
            params![status.to_db_text(), turn_id],
        )?;
        if changed == 0 {
            return Err(StoreError::NotFound(format!("turn {turn_id}")));
        }
        self.get_turn(turn_id)
    }

    /// 在不产生附加 item/trace 的情况下更新 turn 与 AgentLoop 状态。
    pub fn update_turn_state(
        &self,
        turn_id: &str,
        status: TurnStatus,
        agent_loop_status: &str,
    ) -> StoreResult<Turn> {
        self.ensure_turn_status_update_allowed(turn_id, &status, Some(agent_loop_status))?;
        let changed = self.connection.execute(
            "update turns set status = ?1, agent_loop_status = ?2 where turn_id = ?3",
            params![status.to_db_text(), agent_loop_status, turn_id],
        )?;
        if changed == 0 {
            return Err(StoreError::NotFound(format!("turn {turn_id}")));
        }
        self.get_turn(turn_id)
    }

    /// 在一个事务中提交 turn 状态及其持久化条目和追踪。
    pub fn commit_turn_outcome(
        &self,
        turn_id: &str,
        params: CommitTurnOutcomeParams<'_>,
    ) -> StoreResult<CommittedTurnOutcome> {
        self.commit_turn_outcome_with_authority(turn_id, params, TurnOutcomeAuthority::AgentLoop)
    }

    /// 在 typed 基础设施故障权限下提交 turn 终态。
    pub fn commit_turn_outcome_with_authority(
        &self,
        turn_id: &str,
        params: CommitTurnOutcomeParams<'_>,
        authority: TurnOutcomeAuthority,
    ) -> StoreResult<CommittedTurnOutcome> {
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        let committed =
            self.commit_turn_outcome_in_transaction(&transaction, turn_id, params, authority)?;
        transaction.commit()?;
        Ok(committed)
    }

    // 在既有事务中写入 turn 终态、附加 items 和 trace。
    fn commit_turn_outcome_in_transaction(
        &self,
        transaction: &Transaction<'_>,
        turn_id: &str,
        params: CommitTurnOutcomeParams<'_>,
        authority: TurnOutcomeAuthority,
    ) -> StoreResult<CommittedTurnOutcome> {
        let CommitTurnOutcomeParams {
            status,
            agent_loop_status,
            assistant_delta,
            plan,
            trace,
        } = params;
        let current = transaction
            .query_row(
                "select turn_id, thread_id, status, agent_loop_status from turns where turn_id = ?1",
                params![turn_id],
                |row| self.turn_from_row(row),
            )
            .map_err(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => {
                    StoreError::NotFound(format!("turn {turn_id}"))
                }
                other => StoreError::Sqlite(other),
            })?;
        validate_turn_status_update(&current, &status, Some(agent_loop_status), authority)?;
        validate_turn_trace_binding(trace, &current.thread_id, &current.turn_id)?;
        match (&status, assistant_delta) {
            (TurnStatus::Completed, Some(delta)) if !delta.trim().is_empty() => {}
            (TurnStatus::Completed, _) => {
                return Err(StoreError::InvalidState(
                    "completed turn outcome requires a non-empty assistant message".to_string(),
                ));
            }
            (_, Some(_)) => {
                return Err(StoreError::InvalidState(
                    "only a completed turn outcome may include an assistant message".to_string(),
                ));
            }
            (_, None) => {}
        }
        if status == TurnStatus::Interrupted {
            Self::delete_unresolved_pending_approvals(transaction, turn_id)?;
        }

        transaction.execute(
            "update turns set status = ?1, agent_loop_status = ?2 where turn_id = ?3",
            params![status.to_db_text(), agent_loop_status, turn_id],
        )?;
        let trace_thread_id = current.thread_id.clone();
        let turn = Turn {
            status,
            agent_loop_status: agent_loop_status.to_string(),
            ..current
        };
        let plan_item = plan
            .map(|plan| -> StoreResult<Item> {
                let kind = ItemKind::Plan;
                let (payload, redacted) = sanitize_item_payload(&kind, plan.clone())?;
                let item = Self::new_item(turn_id, kind, payload);
                let item_sequence = Self::next_item_sequence(transaction, turn_id)?;
                Self::insert_item(transaction, &item, item_sequence, redacted)?;
                Ok(item)
            })
            .transpose()?;
        let assistant_item = assistant_delta
            .map(|delta| -> StoreResult<Item> {
                let kind = ItemKind::AgentMessage;
                let (payload, redacted) =
                    sanitize_item_payload(&kind, serde_json::json!({"delta": delta}))?;
                let item = Self::new_item(turn_id, kind, payload);
                let item_sequence = Self::next_item_sequence(transaction, turn_id)?;
                Self::insert_item(transaction, &item, item_sequence, redacted)?;
                Ok(item)
            })
            .transpose()?;
        let trace = Self::insert_turn_trace(transaction, trace, &trace_thread_id, turn_id)?;
        Ok(CommittedTurnOutcome {
            turn,
            plan_item,
            assistant_item,
            trace,
        })
    }

    /// 记录取消，同时保留待处理 approval 与执行中工作之间的区别。
    pub fn request_turn_cancellation(
        &self,
        turn_id: &str,
        trace: &TraceEvent,
    ) -> StoreResult<Turn> {
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        let mut turn = transaction
            .query_row(
                "select turn_id, thread_id, status, agent_loop_status from turns where turn_id = ?1",
                params![turn_id],
                |row| self.turn_from_row(row),
            )
            .map_err(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => {
                    StoreError::NotFound(format!("turn {turn_id}"))
                }
                other => StoreError::Sqlite(other),
            })?;
        if is_terminal_turn_status(&turn.status) || turn.agent_loop_status == "cancel_requested" {
            transaction.commit()?;
            return Ok(turn);
        }
        validate_turn_trace_binding(trace, &turn.thread_id, &turn.turn_id)?;
        let pending_count: i64 = transaction.query_row(
            "select count(*) from pending_tool_calls
             where turn_id = ?1 and execution_state = 'pending'",
            params![turn_id],
            |row| row.get(0),
        )?;
        let has_executing = Self::exists_in_transaction(
            &transaction,
            "select 1 from pending_tool_calls where turn_id = ?1 and execution_state = 'executing'",
            turn_id,
        )?;
        if pending_count > 0 && !has_executing {
            Self::delete_unresolved_pending_approvals(&transaction, turn_id)?;
            transaction.execute(
                "update turns set status = ?1, agent_loop_status = 'cancelled' where turn_id = ?2",
                params![TurnStatus::Interrupted.to_db_text(), turn_id],
            )?;
            let mut terminal_trace = trace.clone();
            terminal_trace.summary = "turn interrupted while approval pending".to_string();
            terminal_trace.payload = serde_json::json!({
                "turn_id": turn_id,
                "agent_loop_status": "cancelled",
                "pending_approval_cancelled": true,
            });
            Self::insert_turn_trace(
                &transaction,
                &terminal_trace,
                &turn.thread_id,
                &turn.turn_id,
            )?;
            turn.status = TurnStatus::Interrupted;
            turn.agent_loop_status = "cancelled".to_string();
            transaction.commit()?;
            return Ok(turn);
        }
        transaction.execute(
            "update turns set agent_loop_status = 'cancel_requested' where turn_id = ?1",
            params![turn_id],
        )?;
        Self::insert_turn_trace(&transaction, trace, &turn.thread_id, &turn.turn_id)?;
        turn.agent_loop_status = "cancel_requested".to_string();
        transaction.commit()?;
        Ok(turn)
    }

    // 删除尚未开始外部执行的 pending approval 及其 checkpoint。
    fn delete_unresolved_pending_approvals(
        transaction: &Transaction<'_>,
        turn_id: &str,
    ) -> StoreResult<usize> {
        let mut pending_statement = transaction.prepare(
            "select request_id from pending_tool_calls
             where turn_id = ?1 and execution_state = 'pending' order by rowid",
        )?;
        let pending_request_ids = pending_statement
            .query_map(params![turn_id], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        drop(pending_statement);
        transaction.execute(
            "delete from pending_tool_calls where turn_id = ?1 and execution_state = 'pending'",
            params![turn_id],
        )?;
        for request_id in &pending_request_ids {
            let deleted = transaction.execute(
                "delete from approvals where request_id = ?1 and decision_outcome is null",
                params![request_id],
            )?;
            if deleted != 1 {
                return Err(StoreError::InvalidState(
                    "pending turn cancellation requires an unresolved approval".to_string(),
                ));
            }
        }
        Ok(pending_request_ids.len())
    }

    /// 清理 payload 后向 turn 追加一个 item。
    pub fn append_item(&self, turn_id: &str, kind: ItemKind, payload: Value) -> StoreResult<Item> {
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        if !Self::exists_in_transaction(
            &transaction,
            "select 1 from turns where turn_id = ?1",
            turn_id,
        )? {
            return Err(StoreError::NotFound(format!("turn {turn_id}")));
        }
        let item_sequence = Self::next_item_sequence(&transaction, turn_id)?;
        let (payload, redacted) = sanitize_item_payload(&kind, payload)?;
        let item = Self::new_item(turn_id, kind, payload);
        Self::insert_item(&transaction, &item, item_sequence, redacted)?;
        transaction.commit()?;
        Ok(item)
    }

    /// 读取 turn 的 user input payload，供 approval resume 重建上下文。
    pub fn get_turn_user_input(&self, turn_id: &str) -> StoreResult<Value> {
        let payload: String = self
            .connection
            .query_row(
                "select payload from items where turn_id = ?1 and kind = ?2 order by item_sequence limit 1",
                params![turn_id, ItemKind::UserMessage.to_db_text()],
                |row| row.get(0),
            )
            .map_err(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => {
                    StoreError::NotFound(format!("turn user input {turn_id}"))
                }
                other => StoreError::Sqlite(other),
            })?;
        Ok(serde_json::from_str(&payload)?)
    }
    /// 脱敏并追加一条带完整性校验的 trace event。
    pub fn append_trace(&self, event: &TraceEvent) -> StoreResult<()> {
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        validate_public_trace_binding(&transaction, event)?;
        let _ = Self::insert_trace(&transaction, event)?;
        transaction.commit()?;
        Ok(())
    }

    /// 读取 run 的全部 trace，并校验每条事件完整性。
    pub fn list_trace(&self, run_id: &str) -> StoreResult<Vec<TraceEvent>> {
        self.list_trace_page(run_id, None, None)
    }

    /// 按 rowid 游标分页读取 run trace。
    pub fn list_trace_page(
        &self,
        run_id: &str,
        limit: Option<usize>,
        offset: Option<usize>,
    ) -> StoreResult<Vec<TraceEvent>> {
        let limit = limit.unwrap_or(usize::MAX);
        let offset = offset.unwrap_or(0);
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let offset = i64::try_from(offset).unwrap_or(i64::MAX);
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Deferred)?;
        let mut statement = transaction.prepare(
            "select event_id, run_id, session_id, payload
             from trace_events where run_id = ?1 order by rowid limit ?2 offset ?3",
        )?;
        let rows = statement.query_map(params![run_id, limit, offset], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?;
        let raw_events = rows.collect::<Result<Vec<_>, _>>()?;
        drop(statement);
        let mut events = Vec::new();
        for row in raw_events {
            let (event_id, stored_run_id, stored_session_id, payload) = row;
            let event =
                decode_stored_trace_row(&event_id, &stored_run_id, &stored_session_id, &payload)?;
            events.push(event);
        }
        validate_public_trace_bindings(&transaction, &events)?;
        if events.is_empty() {
            if Self::exists_in_transaction(
                &transaction,
                "select 1 from trace_events where run_id = ?1",
                run_id,
            )? {
                transaction.commit()?;
                return Ok(events);
            }
            return Err(StoreError::NotFound(format!("trace run {run_id}")));
        }
        transaction.commit()?;
        Ok(events)
    }

    /// 读取 run trace 的有界最新窗口并恢复时间顺序。
    pub fn tail_trace(
        &self,
        run_id: &str,
        limit: usize,
        offset: Option<usize>,
    ) -> StoreResult<Vec<TraceEvent>> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let offset = i64::try_from(offset.unwrap_or(0)).unwrap_or(i64::MAX);
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Deferred)?;
        let mut statement = transaction.prepare(
            "select event_id, run_id, session_id, payload
             from trace_events where run_id = ?1 order by rowid desc limit ?2 offset ?3",
        )?;
        let rows = statement.query_map(params![run_id, limit, offset], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?;
        let raw_events = rows.collect::<Result<Vec<_>, _>>()?;
        drop(statement);
        let mut events = Vec::new();
        for row in raw_events {
            let (event_id, stored_run_id, stored_session_id, payload) = row;
            let event =
                decode_stored_trace_row(&event_id, &stored_run_id, &stored_session_id, &payload)?;
            events.push(event);
        }
        validate_public_trace_bindings(&transaction, &events)?;
        if events.is_empty()
            && !Self::exists_in_transaction(
                &transaction,
                "select 1 from trace_events where run_id = ?1",
                run_id,
            )?
        {
            return Err(StoreError::NotFound(format!("trace run {run_id}")));
        }
        events.reverse();
        transaction.commit()?;
        Ok(events)
    }

    /// 读取单条 trace event 并校验完整性。
    pub fn show_trace(&self, event_id: &str) -> StoreResult<TraceEvent> {
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Deferred)?;
        let (stored_event_id, stored_run_id, stored_session_id, payload): (
            String,
            String,
            String,
            String,
        ) = transaction
            .query_row(
                "select event_id, run_id, session_id, payload
                 from trace_events where event_id = ?1",
                params![event_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .map_err(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => {
                    StoreError::NotFound(format!("trace event {event_id}"))
                }
                other => StoreError::Sqlite(other),
            })?;
        let event = decode_stored_trace_row(
            &stored_event_id,
            &stored_run_id,
            &stored_session_id,
            &payload,
        )?;
        validate_public_trace_binding(&transaction, &event)?;
        transaction.commit()?;
        Ok(event)
    }

    /// 校验绑定后保存一个不带 checkpoint 的 approval 请求。
    pub fn create_approval(&self, request: &ApprovalRequest) -> StoreResult<()> {
        insert_approval(&self.connection, request)?;
        Ok(())
    }

    /// 原子保存 approval 请求及其 trace。
    pub fn create_approval_with_trace(
        &self,
        request: &ApprovalRequest,
        component: &str,
        summary: &str,
    ) -> StoreResult<TraceEvent> {
        self.create_approval_with_pending_tool_call_and_trace(request, None, component, summary)
    }

    /// 原子保存一批 approval 请求、检查点和对应 trace。
    ///
    /// 所有请求会在同一个 `BEGIN IMMEDIATE` 事务中先完成绑定、checkpoint、
    /// resource、当前 turn 状态和唯一性校验；已有完全相同的 pending 请求按
    /// 幂等重试处理，新的请求则要么全部提交，要么不留下任何副作用。
    pub fn create_approval_batch_with_pending_tool_calls_and_trace(
        &self,
        approvals: &[(ApprovalRequest, Value)],
        component: &str,
        summary: &str,
    ) -> StoreResult<Vec<TraceEvent>> {
        if approvals.is_empty() {
            return Ok(Vec::new());
        }

        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        let mut request_ids = BTreeSet::new();
        let mut tool_call_keys = BTreeSet::new();
        let mut existing_request_ids = BTreeSet::new();
        let mut new_turn_ids = BTreeSet::new();

        // 先验证整个批次，不能让第一个合法项先改变 turn 或写入 approval。
        for (request, checkpoint) in approvals {
            if request.request_id.trim().is_empty()
                || !request_ids.insert(request.request_id.clone())
            {
                return Err(StoreError::InvalidState(
                    "approval batch request ids must be non-empty and unique".to_string(),
                ));
            }
            let tool_call_id = request.tool_call_id.clone().ok_or_else(|| {
                StoreError::InvalidState(
                    "pending approval checkpoint requires an explicit tool_call_id".to_string(),
                )
            })?;
            if !tool_call_keys.insert((request.turn_id.clone(), tool_call_id.clone())) {
                return Err(StoreError::InvalidState(
                    "approval batch tool call bindings must be unique".to_string(),
                ));
            }

            let (turn_thread_id, status_text, agent_loop_status): (String, String, String) =
                transaction
                    .query_row(
                        "select thread_id, status, agent_loop_status from turns where turn_id = ?1",
                        params![request.turn_id],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                    )
                    .map_err(|error| match error {
                        rusqlite::Error::QueryReturnedNoRows => {
                            StoreError::NotFound(format!("turn {}", request.turn_id))
                        }
                        other => StoreError::Sqlite(other),
                    })?;
            if turn_thread_id != request.thread_id {
                return Err(StoreError::InvalidState(
                    APPROVAL_TURN_THREAD_MISMATCH.to_string(),
                ));
            }
            let turn_status = TurnStatus::from_storage_text(&status_text)
                .ok_or_else(|| unknown_db_enum("turn status", &status_text))?;
            let valid_turn_state = matches!(turn_status, TurnStatus::Running | TurnStatus::Blocked)
                && matches!(agent_loop_status.as_str(), "running" | "blocked");
            if !valid_turn_state {
                return Err(StoreError::InvalidState(
                    "pending approval requires a running or blocked turn".to_string(),
                ));
            }

            let existing = transaction
                .query_row(
                    "select request_id, thread_id, turn_id, payload, decision_outcome, decision_reason
                     from approvals where request_id = ?1",
                    params![request.request_id],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, String>(3)?,
                            row.get::<_, Option<String>>(4)?,
                            row.get::<_, Option<String>>(5)?,
                        ))
                    },
                )
                .optional()?;
            if let Some((
                stored_request_id,
                stored_thread_id,
                stored_turn_id,
                stored_payload,
                stored_outcome,
                stored_reason,
            )) = existing
            {
                let stored_request = decode_stored_approval_request_row(
                    &transaction,
                    &stored_request_id,
                    &stored_thread_id,
                    &stored_turn_id,
                    &stored_payload,
                    stored_outcome.as_deref(),
                    stored_reason.as_deref(),
                )?;
                if stored_request != *request || stored_outcome.is_some() {
                    return Err(StoreError::InvalidState(
                        "approval batch request already exists with different state".to_string(),
                    ));
                }
                let stored_pending = transaction
                    .query_row(
                        "select thread_id, turn_id, tool_call_id, payload, execution_state
                         from pending_tool_calls where request_id = ?1",
                        params![request.request_id],
                        |row| {
                            Ok((
                                row.get::<_, String>(0)?,
                                row.get::<_, String>(1)?,
                                row.get::<_, String>(2)?,
                                row.get::<_, String>(3)?,
                                row.get::<_, String>(4)?,
                            ))
                        },
                    )
                    .optional()?;
                let Some((
                    stored_pending_thread_id,
                    stored_pending_turn_id,
                    stored_tool_call_id,
                    stored_checkpoint,
                    execution_state,
                )) = stored_pending
                else {
                    return Err(StoreError::InvalidState(
                        "existing approval batch request is missing its checkpoint".to_string(),
                    ));
                };
                if execution_state != "pending"
                    || stored_pending_thread_id != request.thread_id
                    || stored_pending_turn_id != request.turn_id
                    || stored_tool_call_id != tool_call_id
                    || stored_checkpoint != serde_json::to_string(checkpoint)?
                {
                    return Err(StoreError::InvalidState(
                        "existing approval batch checkpoint does not match the request".to_string(),
                    ));
                }
                existing_request_ids.insert(request.request_id.clone());
            } else {
                let conflicting_request = transaction
                    .query_row(
                        "select request_id from pending_tool_calls
                         where turn_id = ?1 and tool_call_id = ?2 limit 1",
                        params![request.turn_id, tool_call_id],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()?;
                if conflicting_request.is_some() {
                    return Err(StoreError::InvalidState(
                        "approval batch tool call is already bound to another request".to_string(),
                    ));
                }
                new_turn_ids.insert(request.turn_id.clone());
            }
        }

        let mut traces = Vec::new();
        for (request, checkpoint) in approvals {
            if existing_request_ids.contains(&request.request_id) {
                continue;
            }
            insert_approval(&transaction, request)?;
            let tool_call_id = request.tool_call_id.as_deref().ok_or_else(|| {
                StoreError::InvalidState(
                    "pending approval checkpoint requires an explicit tool_call_id".to_string(),
                )
            })?;
            transaction.execute(
                "insert into pending_tool_calls(request_id, thread_id, turn_id, tool_call_id, payload, execution_state) values(?1, ?2, ?3, ?4, ?5, 'pending')",
                params![
                    request.request_id,
                    request.thread_id,
                    request.turn_id,
                    tool_call_id,
                    serde_json::to_string(checkpoint)?
                ],
            )?;
        }
        for turn_id in &new_turn_ids {
            let changed = transaction.execute(
                "update turns set status = ?1, agent_loop_status = 'blocked'
                 where turn_id = ?2 and status in (?3, ?1)
                   and agent_loop_status in ('running', 'blocked')",
                params![
                    TurnStatus::Blocked.to_db_text(),
                    turn_id,
                    TurnStatus::Running.to_db_text(),
                ],
            )?;
            if changed != 1 {
                return Err(StoreError::InvalidState(
                    "pending approval requires a running or blocked turn".to_string(),
                ));
            }
        }
        for (request, _) in approvals {
            if existing_request_ids.contains(&request.request_id) {
                continue;
            }
            let trace = TraceEvent {
                task_id: Some(request.turn_id.clone()),
                payload: serde_json::json!({
                    "request_id": &request.request_id,
                    "action": &request.action,
                    "tool_call_id": &request.tool_call_id,
                }),
                ..TraceEvent::for_turn(
                    format!("trace_{}", request.request_id),
                    request.thread_id.clone(),
                    request.turn_id.clone(),
                    component,
                    summary,
                )
            };
            traces.push(Self::insert_turn_trace(
                &transaction,
                &trace,
                &request.thread_id,
                &request.turn_id,
            )?);
        }
        transaction.commit()?;
        Ok(traces)
    }

    /// 保存 approval 请求和可选检查点，并将其绑定到阻塞的 turn。
    pub fn create_approval_with_pending_tool_call_and_trace(
        &self,
        request: &ApprovalRequest,
        pending_tool_call: Option<Value>,
        component: &str,
        summary: &str,
    ) -> StoreResult<TraceEvent> {
        if let Some(pending_tool_call) = pending_tool_call {
            let traces = self.create_approval_batch_with_pending_tool_calls_and_trace(
                &[(request.clone(), pending_tool_call)],
                component,
                summary,
            )?;
            return traces.into_iter().next().ok_or_else(|| {
                StoreError::AlreadyExists(format!("approval {}", request.request_id))
            });
        }
        let transaction = self.connection.unchecked_transaction()?;
        insert_approval(&transaction, request)?;
        let trace = TraceEvent::for_turn(
            format!("trace_{}", request.request_id),
            request.thread_id.clone(),
            request.turn_id.clone(),
            component,
            summary,
        );
        let trace = TraceEvent {
            task_id: Some(request.turn_id.clone()),
            payload: serde_json::json!({
                "request_id": &request.request_id,
                "action": &request.action,
                "tool_call_id": &request.tool_call_id,
            }),
            ..trace
        };
        let trace =
            Self::insert_turn_trace(&transaction, &trace, &request.thread_id, &request.turn_id)?;
        transaction.commit()?;
        Ok(trace)
    }

    /// 列出尚未记录最终决定的 approval 请求。
    pub fn list_pending_approvals(&self) -> StoreResult<Vec<ApprovalRequest>> {
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Deferred)?;
        let mut statement = transaction.prepare(
            "select a.request_id, a.thread_id, a.turn_id, a.payload,
                    a.decision_outcome, a.decision_reason,
                    t.thread_id, d.request_id
             from approvals a
             left join turns t on t.turn_id = a.turn_id
             left join approval_decisions d on d.request_id = a.request_id
             where a.decision_outcome is null
             order by a.rowid",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, Option<String>>(6)?,
                row.get::<_, Option<String>>(7)?,
            ))
        })?;
        let raw_rows = rows.collect::<Result<Vec<_>, _>>()?;
        drop(statement);
        let mut approvals = Vec::with_capacity(raw_rows.len());
        for (
            request_id,
            thread_id,
            turn_id,
            payload,
            outcome,
            reason,
            bound_turn_thread_id,
            decision_request_id,
        ) in raw_rows
        {
            let request = decode_stored_approval_request_columns(
                &request_id,
                &thread_id,
                &turn_id,
                &payload,
                outcome.as_deref(),
                reason.as_deref(),
            )?;
            if bound_turn_thread_id.as_deref() != Some(request.thread_id.as_str()) {
                return Err(StoreError::InvalidState(format!(
                    "approval {request_id} turn binding is missing or inconsistent"
                )));
            }
            if decision_request_id.is_some() {
                return Err(StoreError::InvalidState(format!(
                    "approval {request_id} has decision history without final columns"
                )));
            }
            approvals.push(request);
        }
        transaction.commit()?;
        Ok(approvals)
    }

    /// 读取指定 request_id 的 pending approval。
    pub fn get_pending_approval(&self, request_id: &str) -> StoreResult<ApprovalRequest> {
        let (stored_request_id, stored_thread_id, stored_turn_id, payload, outcome, reason): (
            String,
            String,
            String,
            String,
            Option<String>,
            Option<String>,
        ) = self
            .connection
            .query_row(
                "select request_id, thread_id, turn_id, payload, decision_outcome, decision_reason
                 from approvals where request_id = ?1 and decision_outcome is null",
                params![request_id],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                },
            )
            .map_err(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => {
                    StoreError::NotFound(format!("approval {request_id}"))
                }
                other => StoreError::Sqlite(other),
            })?;
        let request = decode_stored_approval_request_row(
            &self.connection,
            &stored_request_id,
            &stored_thread_id,
            &stored_turn_id,
            &payload,
            outcome.as_deref(),
            reason.as_deref(),
        )?;
        if Self::exists_in_transaction(
            &self.connection,
            "select 1 from approval_decisions where request_id = ?1",
            request_id,
        )? {
            return Err(StoreError::InvalidState(format!(
                "approval {request_id} has decision history without final columns"
            )));
        }
        Ok(request)
    }

    /// 判断 approval 是否仍绑定 pending tool call。
    pub fn has_pending_tool_call(&self, request_id: &str) -> StoreResult<bool> {
        Self::exists_in_transaction(
            &self.connection,
            "select 1 from pending_tool_calls where request_id = ?1",
            request_id,
        )
    }

    /// 读取 pending execution 的 opaque checkpoint payload；字段语义由 AppServer/AgentLoop
    /// 在 typed persistence seam 解码，Store 只返回持久化字节和值关系。
    pub fn get_pending_tool_call(&self, request_id: &str) -> StoreResult<Option<Value>> {
        let payload = self
            .connection
            .query_row(
                "select payload from pending_tool_calls where request_id = ?1",
                params![request_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        payload
            .map(|payload| serde_json::from_str::<Value>(&payload).map_err(StoreError::from))
            .transpose()
    }

    /// 按持久化顺序列出已记录的 approval decisions。
    pub fn list_approval_decisions(&self) -> StoreResult<Vec<ApprovalDecision>> {
        type RawDecisionRow = (
            String,
            String,
            String,
            String,
            String,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
        );

        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Deferred)?;
        let mut statement = transaction.prepare(
            "select d.decision_id, d.request_id, d.outcome, d.reason, d.payload,
                    a.request_id, a.thread_id, a.turn_id, a.payload,
                    a.decision_outcome, a.decision_reason, t.thread_id
             from approval_decisions d
             left join approvals a on a.request_id = d.request_id
             left join turns t on t.turn_id = a.turn_id
             order by d.rowid",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
                row.get(7)?,
                row.get(8)?,
                row.get(9)?,
                row.get(10)?,
                row.get(11)?,
            ))
        })?;
        let raw_rows = rows.collect::<Result<Vec<RawDecisionRow>, _>>()?;
        drop(statement);
        let mut decisions = Vec::with_capacity(raw_rows.len());
        for (
            decision_id,
            request_id,
            outcome,
            reason,
            decision_payload,
            approval_request_id,
            approval_thread_id,
            approval_turn_id,
            approval_payload,
            approval_outcome,
            approval_reason,
            bound_turn_thread_id,
        ) in raw_rows
        {
            let (
                Some(approval_request_id),
                Some(approval_thread_id),
                Some(approval_turn_id),
                Some(approval_payload),
            ) = (
                approval_request_id,
                approval_thread_id,
                approval_turn_id,
                approval_payload,
            )
            else {
                return Err(StoreError::InvalidState(format!(
                    "approval decision {decision_id} has no approval request"
                )));
            };
            let request = decode_stored_approval_request_columns(
                &approval_request_id,
                &approval_thread_id,
                &approval_turn_id,
                &approval_payload,
                approval_outcome.as_deref(),
                approval_reason.as_deref(),
            )?;
            if bound_turn_thread_id.as_deref() != Some(request.thread_id.as_str()) {
                return Err(StoreError::InvalidState(format!(
                    "approval {approval_request_id} turn binding is missing or inconsistent"
                )));
            }
            let decision = decode_stored_approval_decision_columns(
                &decision_id,
                &request_id,
                &outcome,
                &reason,
                &decision_payload,
            )?;
            if approval_request_id != request_id
                || approval_outcome.as_deref() != Some(outcome.as_str())
                || approval_reason.as_deref() != Some(reason.as_str())
            {
                return Err(StoreError::InvalidState(format!(
                    "approval decision {decision_id} does not match approval final columns"
                )));
            }
            decisions.push(decision);
        }
        transaction.commit()?;
        Ok(decisions)
    }

    /// 校验并记录 approval 结果，延后或认领其待执行操作。
    pub fn record_approval_decision(
        &self,
        decision: &ApprovalDecision,
        component: &str,
        summary: &str,
    ) -> StoreResult<RecordedApprovalDecision> {
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        // 读取尚未决定的 approval，并恢复其持久化绑定。
        let (
            stored_request_id,
            stored_thread_id,
            stored_turn_id,
            request_payload,
            request_outcome,
            request_reason,
        ): (
            String,
            String,
            String,
            String,
            Option<String>,
            Option<String>,
        ) = transaction
            .query_row(
                "select request_id, thread_id, turn_id, payload, decision_outcome, decision_reason
                 from approvals where request_id = ?1 and decision_outcome is null",
                params![decision.request_id],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                },
            )
            .map_err(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => {
                    StoreError::NotFound(format!("approval {}", decision.request_id))
                }
                other => StoreError::Sqlite(other),
            })?;
        let request = decode_stored_approval_request_row(
            &transaction,
            &stored_request_id,
            &stored_thread_id,
            &stored_turn_id,
            &request_payload,
            request_outcome.as_deref(),
            request_reason.as_deref(),
        )?;
        if Self::exists_in_transaction(
            &transaction,
            "select 1 from approval_decisions where request_id = ?1",
            &decision.request_id,
        )? {
            return Err(StoreError::InvalidState(format!(
                "approval {} has decision history without final columns",
                decision.request_id
            )));
        }
        // 校验 pending tool call 与 approval 的 thread、turn 和 call id 一致。
        let pending_tool_call = match transaction
            .query_row(
                "select thread_id, turn_id, tool_call_id, payload from pending_tool_calls where request_id = ?1",
                params![decision.request_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                },
            )
            .optional()?
        {
            Some((thread_id, turn_id, tool_call_id, payload)) => {
                if thread_id != request.thread_id {
                    return Err(StoreError::InvalidState(
                        PENDING_TOOL_CALL_THREAD_MISMATCH.to_string(),
                    ));
                }
                if turn_id != request.turn_id {
                    return Err(StoreError::InvalidState(
                        PENDING_TOOL_CALL_TURN_MISMATCH.to_string(),
                    ));
                }
                if request.tool_call_id.as_deref() != Some(tool_call_id.as_str()) {
                    return Err(StoreError::InvalidState(
                        PENDING_TOOL_CALL_ID_MISMATCH.to_string(),
                    ));
                }
                Some(serde_json::from_str::<Value>(&payload)?)
            }
            None => {
                if request.tool_call_id.is_some() {
                    return Err(StoreError::NotFound(format!(
                        "pending tool call {}",
                        decision.request_id
                    )));
                }
                None
            }
        };
        if pending_tool_call.is_some() {
            let (turn_status, agent_loop_status): (String, String) = transaction.query_row(
                "select status, agent_loop_status from turns where turn_id = ?1",
                params![request.turn_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            if turn_status != TurnStatus::Blocked.to_db_text() || agent_loop_status != "blocked" {
                return Err(StoreError::InvalidState(
                    "pending approval is not bound to a blocked turn".to_string(),
                ));
            }
        }
        if pending_tool_call.is_some() && decision.outcome == ApprovalOutcome::Allow {
            let thread_status: String = transaction.query_row(
                "select status from threads where thread_id = ?1",
                params![request.thread_id],
                |row| row.get(0),
            )?;
            if thread_status != ThreadStatus::Active.to_db_text() {
                return Err(StoreError::InvalidState(
                    PENDING_APPROVAL_ALLOW_REQUIRES_ACTIVE_THREAD.to_string(),
                ));
            }
            Self::ensure_workspace_has_no_nonterminal_turn(
                &transaction,
                &request.thread_id,
                Some(&request.turn_id),
            )?;
        }
        // Defer 只记录 trace，保留 approval 与 checkpoint 以便后续恢复。
        if decision.outcome == ApprovalOutcome::Defer {
            let trace = TraceEvent {
                task_id: Some(request.turn_id.clone()),
                payload: serde_json::json!({
                    "request_id": decision.request_id,
                    "decision_id": decision.decision_id,
                    "outcome": decision.outcome,
                }),
                ..TraceEvent::for_turn(
                    format!("trace_{}_defer_{}", decision.request_id, Uuid::new_v4()),
                    request.thread_id.clone(),
                    request.turn_id.clone(),
                    component,
                    "approval deferred",
                )
            };
            let trace = Self::insert_turn_trace(
                &transaction,
                &trace,
                &request.thread_id,
                &request.turn_id,
            )?;
            let turn = self.turn_in_transaction(&transaction, &request.turn_id)?;
            transaction.commit()?;
            return Ok(RecordedApprovalDecision {
                request,
                decision: decision.clone(),
                turn,
                pending_tool_call,
                trace,
            });
        }
        // 其他 outcome 写入 approval decision history，并推进或清理执行状态。
        let changed = transaction.execute(
            "update approvals set decision_outcome = ?1, decision_reason = ?2 where request_id = ?3 and decision_outcome is null",
            params![
                final_approval_outcome_to_db_text(decision.outcome)?,
                decision.reason,
                decision.request_id
            ],
        )?;
        if changed == 0 {
            return Err(StoreError::NotFound(format!(
                "approval {}",
                decision.request_id
            )));
        }
        transaction.execute(
            "insert into approval_decisions(decision_id, request_id, outcome, reason, payload) values(?1, ?2, ?3, ?4, ?5)",
            params![
                decision.decision_id,
                decision.request_id,
                final_approval_outcome_to_db_text(decision.outcome)?,
                decision.reason,
                serde_json::to_string(decision)?
            ],
        )?;
        if decision.outcome == ApprovalOutcome::Allow {
            let changed = transaction.execute(
                "update pending_tool_calls set execution_state = 'executing' where request_id = ?1 and execution_state = 'pending'",
                params![decision.request_id],
            )?;
            if pending_tool_call.is_some() && changed != 1 {
                return Err(StoreError::InvalidState(format!(
                    "pending execution {} is not in pending state",
                    decision.request_id
                )));
            }
        } else {
            transaction.execute(
                "delete from pending_tool_calls where request_id = ?1",
                params![decision.request_id],
            )?;
            if pending_tool_call.is_some() {
                let terminal_trace = TraceEvent {
                    task_id: Some(request.turn_id.clone()),
                    payload: serde_json::json!({
                        "request_id": decision.request_id,
                        "decision_id": decision.decision_id,
                        "outcome": decision.outcome,
                    }),
                    ..TraceEvent::for_turn(
                        format!("trace_{}_denied", decision.decision_id),
                        request.thread_id.clone(),
                        request.turn_id.clone(),
                        "agent_loop",
                        "approval denied",
                    )
                };
                let changed = transaction.execute(
                    "update turns set status = ?1, agent_loop_status = 'failed' where turn_id = ?2 and status = ?3 and agent_loop_status = 'blocked'",
                    params![
                        TurnStatus::Failed.to_db_text(),
                        request.turn_id,
                        TurnStatus::Blocked.to_db_text(),
                    ],
                )?;
                if changed != 1 {
                    return Err(StoreError::InvalidState(
                        "pending approval is not bound to a blocked turn".to_string(),
                    ));
                }
                Self::insert_turn_trace(
                    &transaction,
                    &terminal_trace,
                    &request.thread_id,
                    &request.turn_id,
                )?;
            }
        }
        // 将最终决定与其绑定的 turn trace 一并提交。
        let trace = TraceEvent::for_turn(
            format!("trace_{}", decision.decision_id),
            request.thread_id.clone(),
            request.turn_id.clone(),
            component,
            summary,
        );
        let trace = TraceEvent {
            task_id: Some(request.turn_id.clone()),
            payload: serde_json::json!({
                "request_id": decision.request_id,
                "decision_id": decision.decision_id,
                "outcome": decision.outcome,
            }),
            ..trace
        };
        let trace =
            Self::insert_turn_trace(&transaction, &trace, &request.thread_id, &request.turn_id)?;
        let turn = self.turn_in_transaction(&transaction, &request.turn_id)?;
        transaction.commit()?;
        Ok(RecordedApprovalDecision {
            request,
            decision: decision.clone(),
            turn,
            pending_tool_call,
            trace,
        })
    }

    /// 原子地用 turn 结果和后续检查点（如有）解决执行中的 approval。
    pub fn commit_turn_outcome_and_resolve_pending_execution(
        &self,
        request_id: &str,
        params: CommitTurnOutcomeParams<'_>,
        next_approvals: &[(ApprovalRequest, Value)],
    ) -> StoreResult<CommittedTurnOutcome> {
        self.commit_turn_outcome_and_resolve_pending_execution_with_authority(
            request_id,
            params,
            next_approvals,
            TurnOutcomeAuthority::AgentLoop,
        )
    }

    /// 在 typed 基础设施故障权限下提交 executing approval 的终态并清理 checkpoint。
    pub fn commit_turn_outcome_and_resolve_pending_execution_with_authority(
        &self,
        request_id: &str,
        params: CommitTurnOutcomeParams<'_>,
        next_approvals: &[(ApprovalRequest, Value)],
        authority: TurnOutcomeAuthority,
    ) -> StoreResult<CommittedTurnOutcome> {
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        if !next_approvals.is_empty()
            && (params.status != TurnStatus::Blocked || params.agent_loop_status != "blocked")
        {
            return Err(StoreError::InvalidState(
                "next approval handoff requires a blocked turn outcome".to_string(),
            ));
        }
        let bound_turn_id: String = transaction
            .query_row(
                "select turn_id from pending_tool_calls where request_id = ?1 and execution_state = 'executing'",
                params![request_id],
                |row| row.get(0),
            )
            .map_err(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => StoreError::InvalidState(format!(
                    "pending execution {request_id} is not in executing state"
                )),
                other => StoreError::Sqlite(other),
            })?;
        // 先提交当前 executing approval 的 turn 结果。
        let committed = self.commit_turn_outcome_in_transaction(
            &transaction,
            &bound_turn_id,
            params,
            authority,
        )?;
        // 原子移除已解决 checkpoint，再写入 successor approvals。
        let deleted = transaction.execute(
            "delete from pending_tool_calls where request_id = ?1 and execution_state = 'executing'",
            params![request_id],
        )?;
        if deleted != 1 {
            return Err(StoreError::InvalidState(format!(
                "pending execution {request_id} was not resolved"
            )));
        }
        for (request, checkpoint) in next_approvals {
            if request.turn_id != bound_turn_id {
                return Err(StoreError::InvalidState(
                    "next approval turn binding mismatch".to_string(),
                ));
            }
            insert_approval(&transaction, request)?;
            let tool_call_id = request.tool_call_id.as_deref().ok_or_else(|| {
                StoreError::InvalidState(
                    "pending approval checkpoint requires an explicit tool_call_id".to_string(),
                )
            })?;
            ensure_request_turn_binding(&transaction, request)?;
            transaction.execute(
                "insert into pending_tool_calls(request_id, thread_id, turn_id, tool_call_id, payload, execution_state) values(?1, ?2, ?3, ?4, ?5, 'pending')",
                params![
                    request.request_id,
                    request.thread_id,
                    request.turn_id,
                    tool_call_id,
                    serde_json::to_string(checkpoint)?
                ],
            )?;
            let approval_trace = TraceEvent {
                task_id: Some(request.turn_id.clone()),
                payload: serde_json::json!({
                    "request_id": &request.request_id,
                    "action": &request.action,
                    "tool_call_id": &request.tool_call_id,
                }),
                ..TraceEvent::for_turn(
                    format!("trace_{}", request.request_id),
                    request.thread_id.clone(),
                    request.turn_id.clone(),
                    "approval",
                    "approval requested",
                )
            };
            Self::insert_turn_trace(
                &transaction,
                &approval_trace,
                &request.thread_id,
                &request.turn_id,
            )?;
        }
        transaction.commit()?;
        Ok(committed)
    }

    /// 对执行中的 approval 进行协调，而不重放其未知的外部副作用。
    fn recover_incomplete_approval_executions_for_thread(
        transaction: &Connection,
        thread_id: &str,
    ) -> StoreResult<Vec<String>> {
        // approval 与 pending execution 的联合恢复行。
        struct RecoveryRow {
            approval_rowid: i64,
            request: ApprovalRequest,
            decision: Option<ApprovalOutcome>,
            pending_rowid: Option<i64>,
            pending_state: Option<String>,
            turn_status: TurnStatus,
            agent_loop_status: String,
        }

        let mut statement = transaction.prepare(
            "select a.rowid, a.request_id, a.thread_id, a.turn_id, a.payload,
                    a.decision_outcome, a.decision_reason,
                    p.rowid, p.thread_id, p.turn_id, p.tool_call_id, p.payload,
                    p.execution_state
             from approvals a
             left join pending_tool_calls p on p.request_id = a.request_id
             where a.thread_id = ?1
             order by a.rowid",
        )?;
        // 先读取 approval、decision 与 pending execution 的联合快照。
        let persisted_rows = statement
            .query_map(params![thread_id], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, Option<i64>>(7)?,
                    row.get::<_, Option<String>>(8)?,
                    row.get::<_, Option<String>>(9)?,
                    row.get::<_, Option<String>>(10)?,
                    row.get::<_, Option<String>>(11)?,
                    row.get::<_, Option<String>>(12)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        drop(statement);

        // 再将快照解码为可逐行校验和修复的恢复记录。
        let mut rows = Vec::with_capacity(persisted_rows.len());
        for (
            approval_rowid,
            request_id,
            stored_thread_id,
            stored_turn_id,
            request_payload,
            decision,
            decision_reason,
            pending_rowid,
            pending_thread_id,
            pending_turn_id,
            pending_tool_call_id_value,
            pending_payload,
            pending_state,
        ) in persisted_rows
        {
            let request = decode_stored_approval_request_row(
                transaction,
                &request_id,
                &stored_thread_id,
                &stored_turn_id,
                &request_payload,
                decision.as_deref(),
                decision_reason.as_deref(),
            )?;
            if request.thread_id != thread_id {
                return Err(StoreError::InvalidState(format!(
                    "approval {request_id} recovery thread filter mismatch"
                )));
            }
            let (turn_status, agent_loop_status) = transaction
                .query_row(
                    "select status, agent_loop_status from turns where turn_id = ?1 and thread_id = ?2",
                    params![request.turn_id, request.thread_id],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                )
                .optional()?
                .ok_or_else(|| {
                    StoreError::InvalidState(format!(
                        "approval {request_id} is not bound to an existing turn"
                    ))
                })?;
            let status = TurnStatus::from_db_text(&turn_status)
                .ok_or_else(|| unknown_db_enum("turn status", &turn_status))?;
            let decision = decision
                .as_deref()
                .map(decode_final_approval_outcome)
                .transpose()?;
            let mut decision_statement = transaction.prepare(
                "select decision_id, request_id, outcome, reason, payload
                 from approval_decisions where request_id = ?1 order by rowid",
            )?;
            let decision_rows = decision_statement
                .query_map(params![request_id], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            drop(decision_statement);
            match decision {
                None if !decision_rows.is_empty() => {
                    return Err(StoreError::InvalidState(format!(
                        "approval {request_id} has inconsistent decision history"
                    )));
                }
                None => {}
                Some(ApprovalOutcome::Defer) => {
                    return Err(StoreError::InvalidState(format!(
                        "approval {request_id} has inconsistent decision history"
                    )));
                }
                Some(expected_outcome) => {
                    let [(decision_id, history_request_id, outcome, reason, payload)] =
                        decision_rows.as_slice()
                    else {
                        return Err(StoreError::InvalidState(format!(
                            "approval {request_id} has inconsistent decision history"
                        )));
                    };
                    let history_decision: ApprovalDecision = serde_json::from_str(payload)?;
                    if history_request_id != &request_id
                        || decode_final_approval_outcome(outcome)? != expected_outcome
                        || decision_reason.as_deref() != Some(reason.as_str())
                        || history_decision.decision_id != *decision_id
                        || history_decision.request_id != request_id
                        || history_decision.outcome != expected_outcome
                        || history_decision.reason != *reason
                    {
                        return Err(StoreError::InvalidState(format!(
                            "approval {request_id} has inconsistent decision history"
                        )));
                    }
                }
            }

            match (
                pending_rowid,
                pending_thread_id.as_deref(),
                pending_turn_id.as_deref(),
                pending_tool_call_id_value.as_deref(),
                pending_payload.as_deref(),
                pending_state.as_deref(),
            ) {
                (None, None, None, None, None, None) => {}
                (
                    Some(_),
                    Some(thread_id),
                    Some(turn_id),
                    Some(tool_call_id),
                    Some(payload),
                    Some(_),
                ) => {
                    if thread_id != request.thread_id {
                        return Err(StoreError::InvalidState(
                            PENDING_TOOL_CALL_THREAD_MISMATCH.to_string(),
                        ));
                    }
                    if turn_id != request.turn_id {
                        return Err(StoreError::InvalidState(
                            PENDING_TOOL_CALL_TURN_MISMATCH.to_string(),
                        ));
                    }
                    if request.tool_call_id.as_deref() != Some(tool_call_id) {
                        return Err(StoreError::InvalidState(
                            PENDING_TOOL_CALL_ID_MISMATCH.to_string(),
                        ));
                    }
                    if payload.trim().is_empty() {
                        return Err(StoreError::InvalidState(format!(
                            "approval {request_id} has an empty pending checkpoint payload"
                        )));
                    }
                }
                _ => {
                    return Err(StoreError::InvalidState(format!(
                        "approval {request_id} has incomplete pending checkpoint metadata"
                    )));
                }
            }

            if request.tool_call_id.is_none() && pending_rowid.is_some() {
                return Err(StoreError::InvalidState(format!(
                    "approval {request_id} has inconsistent checkpoint state"
                )));
            }
            rows.push(RecoveryRow {
                approval_rowid,
                request,
                decision,
                pending_rowid,
                pending_state,
                turn_status: status,
                agent_loop_status,
            });
        }

        let mut rows_by_turn = BTreeMap::<String, Vec<usize>>::new();
        for (index, row) in rows.iter().enumerate() {
            rows_by_turn
                .entry(row.request.turn_id.clone())
                .or_default()
                .push(index);
        }
        let mut superseded_executions = BTreeSet::new();
        for indexes in rows_by_turn.values() {
            let pending = indexes
                .iter()
                .copied()
                .filter(|index| rows[*index].pending_state.as_deref() == Some("pending"))
                .collect::<Vec<_>>();
            let executing = indexes
                .iter()
                .copied()
                .filter(|index| rows[*index].pending_state.as_deref() == Some("executing"))
                .collect::<Vec<_>>();
            if pending.len() > 1 || executing.len() > 1 {
                return Err(StoreError::InvalidState(
                    "turn has ambiguous approval execution recovery state".to_string(),
                ));
            }
            if let (Some(executing_index), Some(pending_index)) =
                (executing.first(), pending.first())
            {
                if rows[*executing_index].approval_rowid >= rows[*pending_index].approval_rowid {
                    return Err(StoreError::InvalidState(
                        "pending approval does not follow executing approval".to_string(),
                    ));
                }
                superseded_executions.insert(rows[*executing_index].request.request_id.clone());
            }

            for index in indexes {
                let row = &rows[*index];
                if row.request.tool_call_id.is_none() {
                    continue;
                }
                let terminal = is_terminal_turn_status(&row.turn_status);
                let has_later_active_approval = indexes.iter().any(|candidate| {
                    rows[*candidate].approval_rowid > row.approval_rowid
                        && rows[*candidate].pending_rowid.is_some()
                });
                let valid = match (row.decision, row.pending_state.as_deref(), terminal) {
                    (None, Some("pending"), false) => {
                        row.turn_status == TurnStatus::Blocked && row.agent_loop_status == "blocked"
                    }
                    (Some(ApprovalOutcome::Allow), Some("executing"), _) => true,
                    (Some(ApprovalOutcome::Allow), None, true) => true,
                    (Some(ApprovalOutcome::Allow), None, false) => has_later_active_approval,
                    (Some(ApprovalOutcome::Deny), None, true) => true,
                    _ => false,
                };
                if !valid {
                    return Err(StoreError::InvalidState(format!(
                        "approval {} has inconsistent checkpoint state",
                        row.request.request_id
                    )));
                }
            }
        }

        let mut recovered = Vec::new();
        for row in rows
            .iter()
            .filter(|row| row.pending_state.as_deref() == Some("executing"))
        {
            let request_id = &row.request.request_id;
            let turn_id = &row.request.turn_id;
            let superseded = superseded_executions.contains(request_id);
            if !superseded && !is_terminal_turn_status(&row.turn_status) {
                transaction.execute(
                    "update turns set status = ?1, agent_loop_status = 'interrupted' where turn_id = ?2",
                    params![TurnStatus::Interrupted.to_db_text(), turn_id],
                )?;
            }
            let recovery_reason = if superseded {
                "approval_execution_superseded_by_pending_handoff"
            } else {
                "approval_execution_outcome_unknown"
            };
            let summary = if superseded {
                "stale approval execution reconciled during process recovery"
            } else {
                "approval execution interrupted during process recovery"
            };
            let trace = TraceEvent {
                task_id: Some(turn_id.clone()),
                payload: serde_json::json!({
                    "request_id": request_id,
                    "recovery_reason": recovery_reason,
                    "tool_replayed": false,
                }),
                ..TraceEvent::for_turn(
                    format!("trace_{request_id}_recovered"),
                    row.request.thread_id.clone(),
                    turn_id.clone(),
                    "app_server",
                    summary,
                )
            };
            Self::insert_turn_trace(transaction, &trace, &row.request.thread_id, turn_id)?;
            transaction.execute(
                "delete from pending_tool_calls where request_id = ?1",
                params![request_id],
            )?;
            recovered.push(request_id.clone());
        }
        Ok(recovered)
    }

    /// 对执行保护覆盖的每个线程应用所有权丢失恢复。
    fn recover_abandoned_workspace_execution(
        &self,
        guard: &WorkspaceExecutionGuard,
    ) -> StoreResult<()> {
        self.validate_workspace_execution_guard(guard)?;
        for thread_id in self.workspace_execution_thread_ids(&guard.execution_scope)? {
            self.recover_abandoned_thread_execution(&thread_id)?;
        }
        Ok(())
    }

    // 查询执行锁覆盖的 thread 集合。
    fn workspace_execution_thread_ids(
        &self,
        execution_scope: &WorkspaceExecutionScope,
    ) -> StoreResult<Vec<String>> {
        match execution_scope {
            WorkspaceExecutionScope::Workspace(workspace) => {
                let mut statement = self
                    .connection
                    .prepare("select thread_id from threads where cwd = ?1 order by rowid")?;
                let thread_ids = statement
                    .query_map(params![workspace], |row| row.get::<_, String>(0))?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(thread_ids)
            }
            WorkspaceExecutionScope::Thread(thread_id) => Ok(vec![thread_id.clone()]),
        }
    }

    // 恢复单个 thread 的 abandoned execution。
    fn recover_abandoned_thread_execution(&self, thread_id: &str) -> StoreResult<()> {
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        Self::recover_incomplete_approval_executions_for_thread(&transaction, thread_id)?;
        Self::recover_abandoned_turns_for_thread(&transaction, thread_id)?;
        transaction.commit()?;
        Ok(())
    }

    // 将 thread 中遗留的非终态 turn 收敛为可恢复状态。
    fn recover_abandoned_turns_for_thread(
        transaction: &Connection,
        thread_id: &str,
    ) -> StoreResult<Vec<String>> {
        let mut statement = transaction.prepare(
            "select turn_id, status, agent_loop_status from turns
             where thread_id = ?1 and status not in (?2, ?3, ?4)
             order by turn_sequence",
        )?;
        let turns = statement
            .query_map(
                params![
                    thread_id,
                    TurnStatus::Completed.to_db_text(),
                    TurnStatus::Failed.to_db_text(),
                    TurnStatus::Interrupted.to_db_text(),
                ],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )?
            .collect::<Result<Vec<_>, _>>()?;
        drop(statement);

        let mut recovered = Vec::new();
        for (turn_id, serialized_status, agent_loop_status) in turns {
            let status = TurnStatus::from_db_text(&serialized_status)
                .ok_or_else(|| unknown_db_enum("turn status", &serialized_status))?;
            let pending_count: i64 = transaction.query_row(
                "select count(*) from pending_tool_calls
                 where turn_id = ?1 and execution_state = 'pending'",
                params![&turn_id],
                |row| row.get(0),
            )?;
            let executing_count: i64 = transaction.query_row(
                "select count(*) from pending_tool_calls
                 where turn_id = ?1 and execution_state = 'executing'",
                params![&turn_id],
                |row| row.get(0),
            )?;
            if status == TurnStatus::Blocked
                && agent_loop_status == "blocked"
                && pending_count == 1
                && executing_count == 0
            {
                continue;
            }
            if pending_count != 0 || executing_count != 0 {
                return Err(StoreError::InvalidState(format!(
                    "turn {turn_id} has inconsistent pending execution state"
                )));
            }

            transaction.execute(
                "update turns set status = ?1, agent_loop_status = 'interrupted'
                 where turn_id = ?2",
                params![TurnStatus::Interrupted.to_db_text(), &turn_id],
            )?;
            let trace = TraceEvent {
                task_id: Some(turn_id.clone()),
                payload: serde_json::json!({
                    "turn_id": &turn_id,
                    "previous_status": status,
                    "previous_agent_loop_status": agent_loop_status,
                    "recovery_reason": "execution_owner_lost",
                    "tool_replayed": false,
                }),
                ..TraceEvent::for_turn(
                    format!("trace_{turn_id}_owner_lost_{}", Uuid::new_v4()),
                    thread_id,
                    turn_id.clone(),
                    "app_server",
                    "turn interrupted after execution owner was lost",
                )
            };
            Self::insert_turn_trace(transaction, &trace, thread_id, &turn_id)?;
            recovered.push(turn_id);
        }
        Ok(recovered)
    }

    // 确认 guard 仍属于当前 store 和正确的执行范围。
    fn validate_workspace_execution_guard(
        &self,
        guard: &WorkspaceExecutionGuard,
    ) -> StoreResult<()> {
        if self.runtime_path.as_ref() != Some(&guard.store_path) {
            return Err(StoreError::InvalidState(
                "workspace execution guard belongs to another store".to_string(),
            ));
        }
        Ok(())
    }

    /// 读取指定 decision_id 的 approval decision。
    pub fn get_approval_decision(&self, decision_id: &str) -> StoreResult<ApprovalDecision> {
        let (stored_decision_id, request_id, outcome, reason, payload): (
            String,
            String,
            String,
            String,
            String,
        ) = self
            .connection
            .query_row(
                "select decision_id, request_id, outcome, reason, payload
                 from approval_decisions where decision_id = ?1",
                params![decision_id],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .map_err(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => {
                    StoreError::NotFound(format!("approval decision {decision_id}"))
                }
                other => StoreError::Sqlite(other),
            })?;
        decode_stored_approval_decision_row(
            &self.connection,
            &stored_decision_id,
            &request_id,
            &outcome,
            &reason,
            &payload,
        )
    }

    /// 脱敏并持久化一个 content-addressed artifact ref。
    pub fn register_artifact_ref(
        &self,
        params: RegisterArtifactRefParams<'_>,
    ) -> StoreResult<ArtifactRef> {
        let RegisterArtifactRefParams {
            run_id,
            item_id,
            kind,
            uri,
            content_digest,
            summary,
            metadata,
        } = params;
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        validate_artifact_binding(&transaction, run_id, item_id)?;
        let content_digest =
            validate_artifact_fields(kind, uri, content_digest, summary, &metadata)?;
        let duplicate = transaction
            .query_row(
                "select artifact_id from artifact_refs
                 where run_id = ?1 and kind = ?2 and uri = ?3 and content_digest = ?4
                   and ((item_id = ?5) or (item_id is null and ?5 is null))
                 limit 1",
                params![run_id, kind, uri, content_digest, item_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        if let Some(artifact_id) = duplicate {
            return Err(StoreError::AlreadyExists(format!("artifact {artifact_id}")));
        }
        let artifact = ArtifactRef {
            artifact_id: format!("artifact_{}", short_id()),
            run_id: run_id.to_string(),
            item_id: item_id.map(str::to_string),
            kind: kind.to_string(),
            uri: uri.to_string(),
            content_digest,
            summary: redact_secret_like_text(summary),
            redacted: artifact_needs_redaction(uri, summary, &metadata),
            metadata: redact_secret_like_value(metadata),
        };
        transaction.execute(
            "insert into artifact_refs(artifact_id, run_id, item_id, kind, uri, content_digest, summary, metadata, redacted) values(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                artifact.artifact_id,
                artifact.run_id,
                artifact.item_id,
                artifact.kind,
                artifact.uri,
                artifact.content_digest,
                artifact.summary,
                serde_json::to_string(&artifact.metadata)?,
                artifact.redacted,
            ],
        )?;
        transaction.commit()?;
        Ok(artifact)
    }

    /// 读取指定 artifact ref。
    pub fn get_artifact_ref(&self, artifact_id: &str) -> StoreResult<ArtifactRef> {
        let artifact = self
            .connection
            .query_row(
                "select artifact_id, run_id, item_id, kind, uri, content_digest, summary, metadata, redacted from artifact_refs where artifact_id = ?1",
                params![artifact_id],
                artifact_from_row,
            )
            .map_err(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => {
                    StoreError::NotFound(format!("artifact {artifact_id}"))
                }
                other => StoreError::Sqlite(other),
            })?;
        validate_stored_artifact(&self.connection, &artifact)?;
        Ok(artifact)
    }

    /// 列出 run 关联的 artifact refs。
    pub fn list_artifact_refs(&self, run_id: &str) -> StoreResult<Vec<ArtifactRef>> {
        validate_artifact_run(&self.connection, run_id)?;
        let mut statement = self.connection.prepare(
            "select artifact_id, run_id, item_id, kind, uri, content_digest, summary, metadata, redacted from artifact_refs where run_id = ?1 order by rowid",
        )?;
        let rows = statement.query_map(params![run_id], artifact_from_row)?;
        let artifacts = rows.collect::<Result<Vec<_>, _>>()?;
        drop(statement);
        let mut validated = Vec::with_capacity(artifacts.len());
        for artifact in artifacts {
            validate_stored_artifact(&self.connection, &artifact)?;
            validated.push(artifact);
        }
        Ok(validated)
    }

    /// 返回已应用 migration id 的持久化顺序。
    pub fn applied_migrations(&self) -> StoreResult<Vec<String>> {
        let mut statement = self
            .connection
            .prepare("select migration_id from schema_migrations order by migration_id")?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        let mut migrations = Vec::new();
        for row in rows {
            migrations.push(row?);
        }
        Ok(migrations)
    }

    // 在给定事务边界内读取并投影 completed conversation history。
    fn read_thread_history_from(
        connection: &Connection,
        thread_id: &str,
        before_turn_sequence: Option<u64>,
        turn_limit: usize,
    ) -> StoreResult<ThreadHistoryPage> {
        if turn_limit == 0 {
            return Ok(ThreadHistoryPage {
                messages: Vec::new(),
                next_before_turn_sequence: None,
            });
        }

        struct DecodedTurn {
            turn_id: String,
            sequence: u64,
            status: TurnStatus,
            messages: Vec<ConversationMessage>,
        }

        type RawHistoryRow = (
            String,
            i64,
            String,
            Option<String>,
            Option<String>,
            Option<i64>,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<i64>,
        );

        let mut cursor = before_turn_sequence
            .map(|sequence| sequence_to_sql(sequence, "before turn sequence"))
            .transpose()?;
        let batch_size = turn_limit
            .saturating_add(1)
            .clamp(HISTORY_SCAN_BATCH_TURNS, 256);
        let batch_size = i64::try_from(batch_size).expect("bounded history scan size");
        let mut eligible = Vec::with_capacity(turn_limit.saturating_add(1));
        let mut exhausted = false;

        while eligible.len() <= turn_limit && !exhausted {
            let mut statement = connection.prepare(
                "with selected_turns as (
                     select turn_id, turn_sequence, status
                     from turns
                     where thread_id = ?1
                       and (?2 is null or turn_sequence < ?2)
                     order by turn_sequence desc
                     limit ?3
                 )
                 select t.turn_id, t.turn_sequence, t.status,
                        i.item_id, i.turn_id, i.item_sequence, i.kind,
                        i.payload, i.status, i.redacted
                 from selected_turns t
                 left join items i on i.turn_id = t.turn_id
                 order by t.turn_sequence desc, i.item_sequence",
            )?;
            let rows = statement.query_map(params![thread_id, cursor, batch_size], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                    row.get(8)?,
                    row.get(9)?,
                ))
            })?;
            let raw_rows = rows.collect::<Result<Vec<RawHistoryRow>, _>>()?;
            drop(statement);
            if raw_rows.is_empty() {
                break;
            }

            let mut decoded_turns: Vec<DecodedTurn> = Vec::new();
            for (
                turn_id,
                turn_sequence,
                serialized_turn_status,
                item_id,
                item_turn_id,
                item_sequence,
                serialized_kind,
                serialized_payload,
                serialized_item_status,
                redacted,
            ) in raw_rows
            {
                let needs_turn = decoded_turns
                    .last()
                    .is_none_or(|turn| turn.turn_id != turn_id);
                if needs_turn {
                    let status = TurnStatus::from_storage_text(&serialized_turn_status)
                        .ok_or_else(|| unknown_db_enum("turn status", &serialized_turn_status))?;
                    decoded_turns.push(DecodedTurn {
                        turn_id: turn_id.clone(),
                        sequence: sequence_from_sql(turn_sequence, "turn sequence")?,
                        status,
                        messages: Vec::new(),
                    });
                }
                let turn = decoded_turns
                    .last_mut()
                    .expect("history row always creates its turn");

                let (
                    item_id,
                    item_turn_id,
                    item_sequence,
                    serialized_kind,
                    serialized_payload,
                    serialized_item_status,
                    redacted,
                ) = match (
                    item_id,
                    item_turn_id,
                    item_sequence,
                    serialized_kind,
                    serialized_payload,
                    serialized_item_status,
                    redacted,
                ) {
                    (
                        Some(item_id),
                        Some(item_turn_id),
                        Some(item_sequence),
                        Some(serialized_kind),
                        Some(serialized_payload),
                        Some(serialized_item_status),
                        Some(redacted),
                    ) => (
                        item_id,
                        item_turn_id,
                        item_sequence,
                        serialized_kind,
                        serialized_payload,
                        serialized_item_status,
                        redacted,
                    ),
                    (None, None, None, None, None, None, None) => continue,
                    _ => {
                        return Err(StoreError::InvalidState(format!(
                            "turn {turn_id} has a partially null item row"
                        )));
                    }
                };
                if item_turn_id != turn.turn_id {
                    return Err(StoreError::InvalidState(format!(
                        "item {item_id} turn binding does not match selected turn"
                    )));
                }
                let kind = ItemKind::from_storage_text(&serialized_kind)
                    .ok_or_else(|| unknown_db_enum("item kind", &serialized_kind))?;
                let item_status = ItemStatus::from_storage_text(&serialized_item_status)
                    .ok_or_else(|| unknown_db_enum("item status", &serialized_item_status))?;
                let stored_redacted = match redacted {
                    0 => false,
                    1 => true,
                    _ => {
                        return Err(StoreError::InvalidState(format!(
                            "item {item_id} redaction flag is invalid"
                        )));
                    }
                };
                let payload: Value =
                    serde_json::from_str(&serialized_payload).map_err(|error| {
                        StoreError::InvalidState(format!(
                            "item {item_id} payload is invalid: {error}"
                        ))
                    })?;
                let (payload, detected_redaction) = sanitize_item_payload(&kind, payload)?;
                if item_status == ItemStatus::Completed
                    && matches!(kind, ItemKind::UserMessage | ItemKind::AgentMessage)
                {
                    let (role, content) = conversation_projection(&kind, &payload)?;
                    turn.messages.push(ConversationMessage {
                        item_id,
                        turn_id: turn.turn_id.clone(),
                        turn_sequence: turn.sequence,
                        item_sequence: sequence_from_sql(item_sequence, "item sequence")?,
                        role,
                        content,
                        redacted: stored_redacted || detected_redaction,
                    });
                }
            }

            let scanned_turn_count = decoded_turns.len();
            cursor = decoded_turns
                .last()
                .map(|turn| sequence_to_sql(turn.sequence, "history scan cursor"))
                .transpose()?;
            for turn in decoded_turns {
                if turn.status == TurnStatus::Completed
                    && turn.messages.len() == 2
                    && turn.messages[0].role == ConversationRole::User
                    && turn.messages[1].role == ConversationRole::Assistant
                    && turn.messages[0].item_sequence < turn.messages[1].item_sequence
                {
                    eligible.push(turn);
                    if eligible.len() > turn_limit {
                        break;
                    }
                }
            }
            exhausted = scanned_turn_count < usize::try_from(batch_size).expect("positive batch");
        }

        let has_more = eligible.len() > turn_limit;
        if has_more {
            eligible.truncate(turn_limit);
        }
        let next_before_turn_sequence = has_more.then(|| {
            eligible
                .last()
                .expect("history page with more rows is non-empty")
                .sequence
        });
        eligible.reverse();
        let messages = eligible
            .into_iter()
            .flat_map(|turn| turn.messages)
            .collect();
        Ok(ThreadHistoryPage {
            messages,
            next_before_turn_sequence,
        })
    }
    // 拒绝向不存在或已归档 thread 创建可执行 turn。
    fn ensure_active_thread(connection: &Connection, thread_id: &str) -> StoreResult<()> {
        let status = connection
            .query_row(
                "select status from threads where thread_id = ?1",
                params![thread_id],
                |row| row.get::<_, String>(0),
            )
            .map_err(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => {
                    StoreError::NotFound(format!("thread {thread_id}"))
                }
                other => StoreError::Sqlite(other),
            })?;
        let status = ThreadStatus::from_db_text(&status).ok_or_else(|| {
            StoreError::InvalidState(format!("thread {thread_id} has malformed status"))
        })?;
        if status != ThreadStatus::Active {
            return Err(StoreError::InvalidState(format!(
                "thread {thread_id} is not active"
            )));
        }
        Ok(())
    }

    // 检查 thread 是否存在未终态 turn。
    fn ensure_thread_has_no_nonterminal_turn(
        connection: &Connection,
        thread_id: &str,
    ) -> StoreResult<()> {
        let turn_id = connection
            .query_row(
                "select turn_id from turns
                 where thread_id = ?1 and status not in (?2, ?3, ?4)
                 order by turn_sequence limit 1",
                params![
                    thread_id,
                    TurnStatus::Completed.to_db_text(),
                    TurnStatus::Failed.to_db_text(),
                    TurnStatus::Interrupted.to_db_text(),
                ],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        if let Some(turn_id) = turn_id {
            return Err(StoreError::ThreadHasNonterminalTurn {
                thread_id: thread_id.to_string(),
                turn_id,
            });
        }
        Ok(())
    }

    // 检查共享 workspace 是否已被其他 thread 的 turn 占用。
    fn ensure_workspace_has_no_nonterminal_turn(
        connection: &Connection,
        thread_id: &str,
        except_turn_id: Option<&str>,
    ) -> StoreResult<()> {
        let cwd = connection
            .query_row(
                "select cwd from threads where thread_id = ?1",
                params![thread_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .map_err(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => {
                    StoreError::NotFound(format!("thread {thread_id}"))
                }
                other => StoreError::Sqlite(other),
            })?;
        let workspace = cwd.as_deref().filter(|cwd| !cwd.trim().is_empty());
        let conflict = connection
            .query_row(
                "select turns.thread_id, turns.turn_id
                 from turns
                 join threads on threads.thread_id = turns.thread_id
                 where ((?1 is not null and threads.cwd = ?1)
                        or (?1 is null and turns.thread_id = ?2))
                   and turns.status not in (?3, ?4, ?5)
                   and (?6 is null or turns.turn_id != ?6)
                 order by turns.rowid
                 limit 1",
                params![
                    workspace,
                    thread_id,
                    TurnStatus::Completed.to_db_text(),
                    TurnStatus::Failed.to_db_text(),
                    TurnStatus::Interrupted.to_db_text(),
                    except_turn_id,
                ],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?;
        if let Some((thread_id, turn_id)) = conflict {
            return Err(StoreError::WorkspaceHasNonterminalTurn { thread_id, turn_id });
        }
        Ok(())
    }

    // 为 thread 分配下一个稳定且单调的 turn sequence。
    fn next_turn_sequence(connection: &Connection, thread_id: &str) -> StoreResult<u64> {
        let current = connection.query_row(
            "select max(turn_sequence) from turns where thread_id = ?1",
            params![thread_id],
            |row| row.get::<_, Option<i64>>(0),
        )?;
        next_sequence(current, "turn sequence")
    }

    // 为 turn 分配下一个稳定且单调的 item sequence。
    fn next_item_sequence(connection: &Connection, turn_id: &str) -> StoreResult<u64> {
        let current = connection.query_row(
            "select max(item_sequence) from items where turn_id = ?1",
            params![turn_id],
            |row| row.get::<_, Option<i64>>(0),
        )?;
        next_sequence(current, "item sequence")
    }
    // 将包含安全策略快照的 threads 行解码为 protocol Thread。
    fn thread_from_row(&self, row: &rusqlite::Row<'_>) -> rusqlite::Result<Thread> {
        let status: String = row.get(3)?;
        let sandbox_mode: String = row.get(4)?;
        let approval_policy: String = row.get(5)?;
        Ok(Thread {
            thread_id: row.get(0)?,
            model: row.get(1)?,
            cwd: row.get(2)?,
            status: decode_db_enum(status, 3)?,
            sandbox_mode: decode_db_enum(sandbox_mode, 4)?,
            approval_policy: decode_db_enum(approval_policy, 5)?,
        })
    }

    // 将 turns 行解码为 protocol Turn。
    fn turn_from_row(&self, row: &rusqlite::Row<'_>) -> rusqlite::Result<Turn> {
        let status: String = row.get(2)?;
        Ok(Turn {
            turn_id: row.get(0)?,
            thread_id: row.get(1)?,
            status: decode_db_enum(status, 2)?,
            agent_loop_status: row.get(3)?,
        })
    }

    // 在调用方事务中读取绑定 turn，避免 claim 后再开启一个不受补偿控制的读取。
    fn turn_in_transaction(
        &self,
        transaction: &Transaction<'_>,
        turn_id: &str,
    ) -> StoreResult<Turn> {
        transaction
            .query_row(
                "select turn_id, thread_id, status, agent_loop_status from turns where turn_id = ?1",
                params![turn_id],
                |row| self.turn_from_row(row),
            )
            .map_err(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => {
                    StoreError::NotFound(format!("turn {turn_id}"))
                }
                other => StoreError::Sqlite(other),
            })
    }

    // 校验 turn 状态迁移是否允许覆盖当前状态。
    fn ensure_turn_status_update_allowed(
        &self,
        turn_id: &str,
        next_status: &TurnStatus,
        next_agent_loop_status: Option<&str>,
    ) -> StoreResult<()> {
        let current = self.get_turn(turn_id)?;
        validate_turn_status_update(
            &current,
            next_status,
            next_agent_loop_status,
            TurnOutcomeAuthority::AgentLoop,
        )
    }

    // 构造带新 id、初始 active 状态和安全策略快照的 Thread。
    fn new_thread(
        model: Option<&str>,
        cwd: Option<&str>,
        sandbox_mode: PermissionProfileName,
        approval_policy: ApprovalPolicy,
    ) -> Thread {
        Thread {
            thread_id: format!("thread_{}", short_id()),
            model: model.map(str::to_string),
            cwd: cwd.map(str::to_string),
            status: ThreadStatus::Active,
            sandbox_mode,
            approval_policy,
        }
    }

    // 构造绑定 thread 的 running Turn。
    fn new_turn(thread_id: &str, agent_loop_status: &str) -> Turn {
        Self::new_turn_with_id(Self::allocate_turn_id().0, thread_id, agent_loop_status)
    }

    fn new_turn_with_id(turn_id: String, thread_id: &str, agent_loop_status: &str) -> Turn {
        Turn {
            turn_id,
            thread_id: thread_id.to_string(),
            status: TurnStatus::Running,
            agent_loop_status: agent_loop_status.to_string(),
        }
    }

    // 构造绑定 turn 的 pending Item。
    fn new_item(turn_id: &str, kind: ItemKind, payload: Value) -> Item {
        Item {
            item_id: format!("item_{}", short_id()),
            turn_id: turn_id.to_string(),
            kind,
            payload,
            status: ItemStatus::Completed,
        }
    }

    // 将 Thread 编码后写入 threads 表。
    fn insert_thread(connection: &Connection, thread: &Thread) -> StoreResult<()> {
        connection.execute(
            "insert into threads(
                thread_id, model, cwd, status, sandbox_mode, approval_policy
            ) values(?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                thread.thread_id,
                thread.model,
                thread.cwd,
                thread.status.to_db_text(),
                thread.sandbox_mode.to_db_text(),
                thread.approval_policy.to_db_text(),
            ],
        )?;
        Ok(())
    }

    // 将 Turn 与显式 sequence 写入 turns 表。
    fn insert_turn(connection: &Connection, turn: &Turn, turn_sequence: u64) -> StoreResult<()> {
        connection.execute(
            "insert into turns(turn_id, thread_id, turn_sequence, status, agent_loop_status) values(?1, ?2, ?3, ?4, ?5)",
            params![
                turn.turn_id,
                turn.thread_id,
                sequence_to_sql(turn_sequence, "turn sequence")?,
                turn.status.to_db_text(),
                turn.agent_loop_status
            ],
        )?;
        Ok(())
    }

    // 将已脱敏 Item 与显式 sequence 写入 items 表。
    fn insert_item(
        connection: &Connection,
        item: &Item,
        item_sequence: u64,
        redacted: bool,
    ) -> StoreResult<()> {
        connection.execute(
            "insert into items(item_id, turn_id, item_sequence, kind, payload, status, redacted) values(?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                item.item_id,
                item.turn_id,
                sequence_to_sql(item_sequence, "item sequence")?,
                item.kind.to_db_text(),
                serde_json::to_string(&item.payload)?,
                item.status.to_db_text(),
                redacted,
            ],
        )?;
        Ok(())
    }
    // 脱敏、哈希并写入 trace event，返回持久化投影。
    fn insert_trace(connection: &Connection, event: &TraceEvent) -> StoreResult<TraceEvent> {
        let event = sanitize_trace_event(event);
        connection.execute(
            "insert into trace_events(event_id, run_id, session_id, payload) values(?1, ?2, ?3, ?4)",
            params![
                event.event_id,
                event.run_id,
                event.session_id,
                serde_json::to_string(&event)?
            ],
        )?;
        Ok(event)
    }

    // 在所有 turn 相关写入前统一检查 thread/turn 身份绑定。
    fn insert_turn_trace(
        connection: &Connection,
        event: &TraceEvent,
        thread_id: &str,
        turn_id: &str,
    ) -> StoreResult<TraceEvent> {
        validate_turn_trace_binding(event, thread_id, turn_id)?;
        Self::insert_trace(connection, event)
    }

    // 在调用方事务中执行单参数存在性查询。
    fn exists_in_transaction(
        connection: &Connection,
        query: &str,
        value: &str,
    ) -> StoreResult<bool> {
        let result = connection.query_row(query, params![value], |_| Ok(()));
        match result {
            Ok(()) => Ok(true),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(false),
            Err(error) => Err(StoreError::Sqlite(error)),
        }
    }
}

impl StoreIdentityGuard {
    fn open(path: &Path, create: bool) -> StoreResult<Self> {
        if path == Path::new(":memory:") {
            return Err(StoreError::InvalidState(
                "file identity guard cannot protect an in-memory store".to_string(),
            ));
        }
        let initial = open_protected_store_file(path, create)?;
        let identity = checked_store_file_identity(&initial.file)?;
        let canonical_path = std::fs::canonicalize(&initial.absolute_path).map_err(|error| {
            StoreError::InvalidState(format!("cannot canonicalize protected store path: {error}"))
        })?;
        let canonical = open_protected_store_file(&canonical_path, false)?;
        let path_identity = checked_store_file_identity(&canonical.file)?;
        if identity != path_identity {
            return Err(StoreError::InvalidState(
                "store path identity changed while opening".to_string(),
            ));
        }
        Ok(Self {
            path: canonical_path,
            identity,
            _file: initial.file,
            parent: canonical.parent,
            file_name: canonical.file_name,
        })
    }

    fn verify(&self) -> StoreResult<()> {
        let file_identity = checked_store_file_identity(&self._file)?;
        let parent_file = open_store_file_at(&self.parent, &self.file_name, false)?;
        let parent_identity = checked_store_file_identity(&parent_file)?;
        let namespace_file = open_protected_store_file(&self.path, false)?;
        let namespace_identity = checked_store_file_identity(&namespace_file.file)?;
        if file_identity != self.identity
            || parent_identity != self.identity
            || namespace_identity != self.identity
        {
            return Err(StoreError::InvalidState(
                "store file identity changed after initialization".to_string(),
            ));
        }
        Ok(())
    }
}

struct ProtectedStoreFile {
    absolute_path: PathBuf,
    parent: CapabilityDir,
    file_name: OsString,
    file: File,
}

fn open_protected_store_file(path: &Path, create: bool) -> StoreResult<ProtectedStoreFile> {
    let absolute_path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| {
                StoreError::InvalidState(format!("cannot resolve current directory: {error}"))
            })?
            .join(path)
    };
    let mut root = PathBuf::new();
    let mut names = Vec::<OsString>::new();
    for component in absolute_path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => {
                if !names.is_empty() {
                    return Err(StoreError::InvalidState(
                        "store path contains a misplaced root component".to_string(),
                    ));
                }
                root.push(component.as_os_str());
            }
            Component::CurDir => {}
            Component::ParentDir => {
                return Err(StoreError::InvalidState(
                    "store path must not contain parent-directory components".to_string(),
                ));
            }
            Component::Normal(name) => names.push(name.to_os_string()),
        }
    }
    let file_name = names.pop().ok_or_else(|| {
        StoreError::InvalidState("store path must include a file name".to_string())
    })?;
    if root.as_os_str().is_empty() {
        return Err(StoreError::InvalidState(
            "store path must resolve from an absolute filesystem root".to_string(),
        ));
    }
    let mut parent =
        CapabilityDir::open_ambient_dir(&root, ambient_authority()).map_err(|error| {
            StoreError::InvalidState(format!("cannot open store filesystem root: {error}"))
        })?;
    for name in names {
        parent = parent.open_dir_nofollow(&name).map_err(|error| {
            StoreError::InvalidState(format!(
                "cannot open store parent without following links: {error}"
            ))
        })?;
    }
    let file = open_store_file_at(&parent, &file_name, create)?;
    Ok(ProtectedStoreFile {
        absolute_path,
        parent,
        file_name,
        file,
    })
}

fn open_store_file_at(parent: &CapabilityDir, name: &OsString, create: bool) -> StoreResult<File> {
    let mut options = CapabilityOpenOptions::new();
    options
        .read(true)
        .write(true)
        .create(create)
        .truncate(false)
        .follow(FollowSymlinks::No);
    parent
        .open_with(name, &options)
        .map(cap_std::fs::File::into_std)
        .map_err(|error| {
            StoreError::InvalidState(format!(
                "cannot open protected store file without following links: {error}"
            ))
        })
}

#[cfg(test)]
fn open_store_file(path: &Path, create: bool) -> StoreResult<File> {
    open_protected_store_file(path, create).map(|protected| protected.file)
}

fn checked_store_file_identity(file: &File) -> StoreResult<StoreFileIdentity> {
    let metadata = file.metadata().map_err(|error| {
        StoreError::InvalidState(format!("cannot inspect protected store file: {error}"))
    })?;
    if !metadata.is_file() {
        return Err(StoreError::InvalidState(
            "store path must identify a regular file".to_string(),
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() > 1 {
            return Err(StoreError::InvalidState(
                "store file must not have multiple hard links".to_string(),
            ));
        }
        return Ok(StoreFileIdentity::Unix {
            device: metadata.dev(),
            inode: metadata.ino(),
        });
    }
    #[cfg(windows)]
    {
        let (volume_serial_number, file_index, number_of_links, file_attributes) =
            windows_file_identity::read(file).map_err(|error| {
                StoreError::InvalidState(format!(
                    "cannot inspect Windows store file identity: {error}"
                ))
            })?;
        if file_attributes & 0x0000_0400 != 0 {
            return Err(StoreError::InvalidState(
                "store file must not be a reparse point".to_string(),
            ));
        }
        if number_of_links != 1 {
            return Err(StoreError::InvalidState(
                "store file must not have multiple hard links".to_string(),
            ));
        }
        Ok(StoreFileIdentity::Windows {
            volume_serial_number,
            file_index,
        })
    }
    #[cfg(not(any(unix, windows)))]
    {
        Err(StoreError::InvalidState(
            "store file identity is unsupported on this platform".to_string(),
        ))
    }
}

// 配置 SQLite 的并发、外键、WAL 与安全删除 pragma。
fn configure_connection(connection: &Connection) -> StoreResult<()> {
    connection.busy_timeout(Duration::from_millis(SQLITE_BUSY_TIMEOUT_MS))?;
    connection.pragma_update(None, SQLITE_SECURE_DELETE_PRAGMA, "ON")?;
    connection.pragma_update(None, SQLITE_FOREIGN_KEYS_PRAGMA, "ON")?;
    connection.pragma_update(None, SQLITE_JOURNAL_MODE_PRAGMA, SQLITE_JOURNAL_MODE_WAL)?;
    Ok(())
}

// 确认每个 store connection 都处于受支持的 SQLite 运行时配置。
fn validate_connection_pragmas(connection: &Connection, in_memory: bool) -> StoreResult<()> {
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
fn workspace_execution_scope(thread: &Thread) -> WorkspaceExecutionScope {
    match thread.cwd.as_deref().filter(|cwd| !cwd.trim().is_empty()) {
        Some(cwd) => WorkspaceExecutionScope::Workspace(cwd.to_string()),
        None => WorkspaceExecutionScope::Thread(thread.thread_id.clone()),
    }
}

// 生成执行所有权锁文件的稳定路径。
fn workspace_execution_lock_path(
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
fn acquire_store_initialization_lock(path: &Path) -> StoreResult<Option<File>> {
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
fn next_sequence(current: Option<i64>, label: &str) -> StoreResult<u64> {
    let current = current
        .map(|sequence| sequence_from_sql(sequence, label))
        .transpose()?
        .unwrap_or(0);
    current
        .checked_add(1)
        .ok_or_else(|| StoreError::InvalidState(format!("{label} overflow")))
}

// 将 u64 sequence 转换为 SQLite 可存储的 i64。
fn sequence_to_sql(sequence: u64, label: &str) -> StoreResult<i64> {
    i64::try_from(sequence)
        .map_err(|_| StoreError::InvalidState(format!("{label} exceeds sqlite integer range")))
}

// 将 SQLite sequence 解码为非负 u64。
fn sequence_from_sql(sequence: i64, label: &str) -> StoreResult<u64> {
    u64::try_from(sequence)
        .map_err(|_| StoreError::InvalidState(format!("{label} must be non-negative")))
}

// 按 item kind 清理敏感内容并返回是否发生脱敏。
fn sanitize_item_payload(kind: &ItemKind, mut payload: Value) -> StoreResult<(Value, bool)> {
    match kind {
        ItemKind::UserMessage => {
            let items = payload.as_array_mut().ok_or_else(|| {
                StoreError::InvalidState(
                    "user message payload must be an InputItem array".to_string(),
                )
            })?;
            let mut redacted = false;
            for item in items {
                let object = item.as_object_mut().ok_or_else(|| {
                    StoreError::InvalidState(
                        "user message contains malformed InputItem".to_string(),
                    )
                })?;
                let valid = object.len() == 2
                    && object.get("type").and_then(Value::as_str) == Some("text")
                    && object.get("text").is_some_and(Value::is_string);
                if !valid {
                    return Err(StoreError::InvalidState(
                        "user message contains malformed InputItem".to_string(),
                    ));
                }
                let text = object
                    .get("text")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        StoreError::InvalidState(
                            "user message contains malformed InputItem".to_string(),
                        )
                    })?
                    .to_string();
                if contains_sensitive_text(&text) {
                    object.insert(
                        "text".to_string(),
                        Value::String(REDACTED_USER_INPUT.to_string()),
                    );
                    redacted = true;
                }
            }
            Ok((payload, redacted))
        }
        ItemKind::AgentMessage => {
            let object = payload.as_object_mut().ok_or_else(|| {
                StoreError::InvalidState("agent message payload must contain delta".to_string())
            })?;
            let valid = object.len() == 1 && object.get("delta").is_some_and(Value::is_string);
            if !valid {
                return Err(StoreError::InvalidState(
                    "agent message payload must contain only a string delta".to_string(),
                ));
            }
            let delta = object
                .get("delta")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    StoreError::InvalidState(
                        "agent message payload must contain only a string delta".to_string(),
                    )
                })?
                .to_string();
            let redacted = contains_sensitive_text(&delta);
            if redacted {
                object.insert(
                    "delta".to_string(),
                    Value::String(REDACTED_ASSISTANT_OUTPUT.to_string()),
                );
            }
            Ok((payload, redacted))
        }
        _ => {
            let serialized = serde_json::to_string(&payload)?;
            if contains_sensitive_text(&serialized) {
                Ok((serde_json::json!({"redacted": true}), true))
            } else {
                Ok((payload, false))
            }
        }
    }
}

// 将持久化 item 投影为模型可消费的 conversation message。
fn conversation_projection(
    kind: &ItemKind,
    payload: &Value,
) -> StoreResult<(ConversationRole, String)> {
    match kind {
        ItemKind::UserMessage => {
            let items = payload.as_array().ok_or_else(|| {
                StoreError::InvalidState("malformed user message payload".to_string())
            })?;
            let content = items
                .iter()
                .map(|item| {
                    item.get("text").and_then(Value::as_str).ok_or_else(|| {
                        StoreError::InvalidState("malformed user message InputItem".to_string())
                    })
                })
                .collect::<StoreResult<Vec<_>>>()?
                .join("\n");
            Ok((ConversationRole::User, content))
        }
        ItemKind::AgentMessage => {
            let content = payload
                .get("delta")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    StoreError::InvalidState("malformed agent message payload".to_string())
                })?
                .to_string();
            Ok((ConversationRole::Assistant, content))
        }
        _ => Err(StoreError::InvalidState(
            "non-conversation item selected for history".to_string(),
        )),
    }
}

// 执行 SQLite foreign_key_check，并拒绝已有违反项。
fn fail_closed_on_foreign_key_violations(connection: &Connection, phase: &str) -> StoreResult<()> {
    let violation = connection
        .query_row("pragma foreign_key_check", [], |row| {
            let table: String = row.get(0)?;
            let row_id: i64 = row.get(1)?;
            Ok(format!("{table}:{row_id}"))
        })
        .optional()?;
    if let Some(violation) = violation {
        return Err(StoreError::InvalidState(format!(
            "store foreign key violation during {phase}: {violation}"
        )));
    }
    Ok(())
}

// 校验 approval request 的显式 thread/turn 绑定。
fn ensure_approval_request_binding(
    connection: &Connection,
    request: &ApprovalRequest,
) -> StoreResult<()> {
    if request.thread_id.trim().is_empty() || request.turn_id.trim().is_empty() {
        return Err(StoreError::InvalidState(
            APPROVAL_BINDING_REQUIRED.to_string(),
        ));
    }
    ensure_request_turn_binding(connection, request)
}

// 校验 pending checkpoint 与 request 的 turn 绑定。
fn ensure_request_turn_binding(
    connection: &Connection,
    request: &ApprovalRequest,
) -> StoreResult<()> {
    let thread_id = connection
        .query_row(
            "select thread_id from turns where turn_id = ?1",
            params![request.turn_id],
            |row| row.get::<_, String>(0),
        )
        .map_err(|error| match error {
            rusqlite::Error::QueryReturnedNoRows => {
                StoreError::NotFound(format!("turn {}", request.turn_id))
            }
            other => StoreError::Sqlite(other),
        })?;
    if thread_id != request.thread_id {
        return Err(StoreError::InvalidState(
            APPROVAL_TURN_THREAD_MISMATCH.to_string(),
        ));
    }
    Ok(())
}

// 判断 turn status 是否已经不可再推进。
fn is_terminal_turn_status(status: &TurnStatus) -> bool {
    matches!(
        status,
        TurnStatus::Completed | TurnStatus::Failed | TurnStatus::Interrupted
    )
}

// 校验状态更新没有覆盖终态或制造非法迁移。
fn validate_turn_status_update(
    current: &Turn,
    next_status: &TurnStatus,
    next_agent_loop_status: Option<&str>,
    authority: TurnOutcomeAuthority,
) -> StoreResult<()> {
    if authority == TurnOutcomeAuthority::InfrastructureFailure
        && (*next_status != TurnStatus::Failed || next_agent_loop_status != Some("failed"))
    {
        return Err(StoreError::InvalidState(
            "infrastructure failure can only finalize as failed".to_string(),
        ));
    }
    if current.agent_loop_status == "cancel_requested"
        && *next_status != TurnStatus::Interrupted
        && authority != TurnOutcomeAuthority::InfrastructureFailure
    {
        return Err(StoreError::InvalidState(
            "cancel-requested turn can only finalize as interrupted".to_string(),
        ));
    }
    if is_terminal_turn_status(&current.status) {
        if current.status != *next_status {
            return Err(StoreError::InvalidState(
                "terminal turn status cannot be overwritten".to_string(),
            ));
        }
        if next_agent_loop_status.is_some_and(|status| status != current.agent_loop_status) {
            return Err(StoreError::InvalidState(
                "terminal turn agent_loop_status cannot be overwritten".to_string(),
            ));
        }
    }
    Ok(())
}

// 将 approval request 编码并写入 approvals 表。
fn insert_approval(connection: &Connection, request: &ApprovalRequest) -> StoreResult<()> {
    ensure_approval_request_binding(connection, request)?;
    connection
        .execute(
            "insert into approvals(
                 request_id, thread_id, turn_id, payload, decision_outcome, decision_reason
             ) values(?1, ?2, ?3, ?4, null, null)",
            params![
                request.request_id,
                request.thread_id,
                request.turn_id,
                serde_json::to_string(request)?
            ],
        )
        .map_err(|error| match error {
            rusqlite::Error::SqliteFailure(ref sqlite_error, _)
                if sqlite_error.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                StoreError::AlreadyExists(format!("approval {}", request.request_id))
            }
            other => StoreError::Sqlite(other),
        })?;
    Ok(())
}

// 生成用于持久化记录的短随机 id。
fn short_id() -> String {
    Uuid::new_v4().simple().to_string()[..12].to_string()
}

// 验证 artifact registration 的 thread/turn/item 绑定。
fn validate_artifact_binding(
    connection: &Connection,
    run_id: &str,
    item_id: Option<&str>,
) -> StoreResult<()> {
    validate_artifact_run(connection, run_id)?;
    if let Some(item_id) = item_id {
        validate_artifact_item(connection, run_id, item_id)?;
    }
    Ok(())
}

// 读取时重新验证持久化 artifact，避免数据库中被篡改的 ref 进入公共 fetch。
fn validate_stored_artifact(connection: &Connection, artifact: &ArtifactRef) -> StoreResult<()> {
    validate_artifact_id(&artifact.artifact_id)?;
    validate_artifact_binding(connection, &artifact.run_id, artifact.item_id.as_deref())?;
    let normalized_digest = validate_artifact_fields(
        &artifact.kind,
        &artifact.uri,
        &artifact.content_digest,
        &artifact.summary,
        &artifact.metadata,
    )?;
    if normalized_digest != artifact.content_digest {
        return Err(StoreError::InvalidState(format!(
            "artifact {} content digest is not canonical",
            artifact.artifact_id
        )));
    }
    if redact_secret_like_text(&artifact.uri) != artifact.uri
        || redact_secret_like_text(&artifact.summary) != artifact.summary
        || redact_secret_like_value(artifact.metadata.clone()) != artifact.metadata
    {
        return Err(StoreError::InvalidState(format!(
            "artifact {} contains unredacted sensitive data",
            artifact.artifact_id
        )));
    }
    Ok(())
}

fn validate_artifact_run(connection: &Connection, run_id: &str) -> StoreResult<()> {
    if run_id.trim().is_empty() {
        return Err(StoreError::InvalidState(
            "artifact run_id must not be empty".to_string(),
        ));
    }
    let thread_exists = connection
        .query_row(
            "select 1 from threads where thread_id = ?1",
            params![run_id],
            |_| Ok(()),
        )
        .optional()?;
    if thread_exists.is_some() {
        return Ok(());
    }
    let turn_exists = connection
        .query_row(
            "select 1 from turns where turn_id = ?1",
            params![run_id],
            |_| Ok(()),
        )
        .optional()?;
    if turn_exists.is_some() {
        return Err(StoreError::InvalidState(
            "artifact run_id must identify a thread, not a turn".to_string(),
        ));
    }
    Err(StoreError::NotFound(format!("artifact run {run_id}")))
}

fn validate_artifact_item(connection: &Connection, run_id: &str, item_id: &str) -> StoreResult<()> {
    if item_id.trim().is_empty() {
        return Err(StoreError::InvalidState(
            "artifact item_id must not be empty".to_string(),
        ));
    }
    let turn_id = connection
        .query_row(
            "select turn_id from items where item_id = ?1",
            params![item_id],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .ok_or_else(|| StoreError::NotFound(format!("artifact item {item_id}")))?;
    let item_thread_id = connection
        .query_row(
            "select thread_id from turns where turn_id = ?1",
            params![turn_id],
            |row| row.get::<_, String>(0),
        )
        .map_err(|error| match error {
            rusqlite::Error::QueryReturnedNoRows => StoreError::InvalidState(format!(
                "artifact item {item_id} references a missing turn"
            )),
            other => StoreError::Sqlite(other),
        })?;
    if item_thread_id != run_id {
        return Err(StoreError::InvalidState(
            "artifact item does not belong to run".to_string(),
        ));
    }
    Ok(())
}

fn validate_artifact_fields(
    kind: &str,
    uri: &str,
    content_digest: &str,
    summary: &str,
    metadata: &Value,
) -> StoreResult<String> {
    if kind.trim().is_empty()
        || kind.len() > ARTIFACT_KIND_MAX_BYTES
        || !kind.chars().enumerate().all(|(index, character)| {
            (index == 0 && character.is_ascii_alphanumeric())
                || (index > 0
                    && (character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.')))
        })
        || contains_artifact_reference(kind)
    {
        return Err(StoreError::InvalidState(
            "artifact kind is invalid".to_string(),
        ));
    }
    validate_artifact_text("uri", uri)?;
    let Some(uri_path) = uri.strip_prefix(ARTIFACT_URI_PREFIX) else {
        return Err(StoreError::InvalidState(
            "artifact uri must use artifact://".to_string(),
        ));
    };
    if uri_path.is_empty()
        || contains_sensitive_text(uri)
        || is_protected_path(uri_path)
        || contains_artifact_reference(uri_path)
    {
        return Err(StoreError::InvalidState(
            "artifact uri is not safe".to_string(),
        ));
    }
    validate_artifact_text("summary", summary)?;
    if contains_artifact_reference(summary) {
        return Err(StoreError::InvalidState(
            "artifact summary contains an unregistered reference".to_string(),
        ));
    }
    let normalized_digest = validate_artifact_digest(content_digest)?;
    validate_artifact_metadata(metadata)?;
    Ok(normalized_digest)
}

fn validate_artifact_id(artifact_id: &str) -> StoreResult<()> {
    if artifact_id.len() <= "artifact_".len()
        || artifact_id.len() > ARTIFACT_TEXT_MAX_BYTES
        || !artifact_id.starts_with("artifact_")
        || !artifact_id["artifact_".len()..]
            .chars()
            .all(|character| character.is_ascii_alphanumeric())
    {
        return Err(StoreError::InvalidState(
            "artifact id is invalid".to_string(),
        ));
    }
    Ok(())
}

fn validate_artifact_text(field: &str, value: &str) -> StoreResult<()> {
    if value.trim().is_empty()
        || value.len() > ARTIFACT_TEXT_MAX_BYTES
        || value.chars().any(char::is_control)
    {
        return Err(StoreError::InvalidState(format!(
            "artifact {field} is invalid"
        )));
    }
    Ok(())
}

fn validate_artifact_digest(value: &str) -> StoreResult<String> {
    let Some(hex) = value.strip_prefix(TRACE_HASH_PREFIX) else {
        return Err(StoreError::InvalidState(
            "artifact content digest must use sha256".to_string(),
        ));
    };
    if hex.len() != SHA256_HEX_LENGTH || !hex.chars().all(|character| character.is_ascii_hexdigit())
    {
        return Err(StoreError::InvalidState(
            "artifact content digest is invalid".to_string(),
        ));
    }
    Ok(format!("{TRACE_HASH_PREFIX}{}", hex.to_ascii_lowercase()))
}

fn validate_artifact_metadata(value: &Value) -> StoreResult<()> {
    let size = serde_json::to_vec(value)?.len();
    if size > ARTIFACT_METADATA_MAX_BYTES {
        return Err(StoreError::InvalidState(
            "artifact metadata is too large".to_string(),
        ));
    }
    validate_artifact_metadata_value(value, 0)
}

fn validate_artifact_metadata_value(value: &Value, depth: usize) -> StoreResult<()> {
    if depth > ARTIFACT_METADATA_MAX_DEPTH {
        return Err(StoreError::InvalidState(
            "artifact metadata is too deeply nested".to_string(),
        ));
    }
    match value {
        Value::Object(entries) => {
            for (key, value) in entries {
                if key.trim().is_empty()
                    || key.len() > ARTIFACT_TEXT_MAX_BYTES
                    || key.chars().any(char::is_control)
                    || is_artifact_reference_key(key)
                {
                    return Err(StoreError::InvalidState(
                        "artifact metadata contains an invalid reference field".to_string(),
                    ));
                }
                validate_artifact_metadata_value(value, depth + 1)?;
            }
        }
        Value::Array(values) => {
            for value in values {
                validate_artifact_metadata_value(value, depth + 1)?;
            }
        }
        Value::String(text) => {
            if text.chars().any(char::is_control)
                || contains_artifact_reference(text)
                || is_protected_path(text)
            {
                return Err(StoreError::InvalidState(
                    "artifact metadata contains unsafe text".to_string(),
                ));
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
    Ok(())
}

fn is_artifact_reference_key(key: &str) -> bool {
    matches!(
        key.to_ascii_lowercase().as_str(),
        "artifact_ref"
            | "artifact_refs"
            | "artifactref"
            | "artifactrefs"
            | "diff_ref"
            | "diff_refs"
            | "diffref"
            | "diffrefs"
    )
}

fn contains_artifact_reference(value: &str) -> bool {
    value.to_ascii_lowercase().contains(ARTIFACT_URI_PREFIX)
}

// 将 artifact_refs 行解码为 ArtifactRef。
fn artifact_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ArtifactRef> {
    let metadata: String = row.get(7)?;
    Ok(ArtifactRef {
        artifact_id: row.get(0)?,
        run_id: row.get(1)?,
        item_id: row.get(2)?,
        kind: row.get(3)?,
        uri: row.get(4)?,
        content_digest: row.get(5)?,
        summary: row.get(6)?,
        metadata: serde_json::from_str(&metadata).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                7,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?,
        redacted: row.get(8)?,
    })
}

// 解码现行 trace 行，同时验证列与 payload 的身份投影一致。
fn decode_stored_trace_row(
    event_id: &str,
    run_id: &str,
    session_id: &str,
    payload: &str,
) -> StoreResult<TraceEvent> {
    let event = decode_trace_payload(payload)?;
    if event.event_id != event_id || event.run_id != run_id || event.session_id != session_id {
        return Err(StoreError::InvalidState(format!(
            "trace {event_id} columns do not match payload"
        )));
    }
    Ok(event)
}

// 解码 trace payload 并恢复完整性校验所需对象。
fn decode_trace_payload(payload: &str) -> StoreResult<TraceEvent> {
    let event: TraceEvent = serde_json::from_str(payload)?;
    if !event.redaction_applied {
        return Err(StoreError::TraceIntegrity(
            "stored trace was not sanitized".to_string(),
        ));
    }
    let expected_hash = trace_payload_hash(&event.payload);
    if event.payload_hash != expected_hash {
        return Err(StoreError::TraceIntegrity(format!(
            "payload hash mismatch for {}",
            event.event_id
        )));
    }
    Ok(event)
}

// 对 trace 的 payload 与可见文本执行脱敏投影。
fn sanitize_trace_event(event: &TraceEvent) -> TraceEvent {
    let mut sanitized = event.clone();
    sanitized.summary = redact_secret_like_text(&sanitized.summary);
    sanitized.payload = redact_secret_like_value(sanitized.payload);
    sanitized.artifact_refs = sanitized
        .artifact_refs
        .into_iter()
        .map(|artifact_ref| redact_secret_like_text(&artifact_ref))
        .collect();
    sanitized.redaction_applied = true;
    sanitized.payload_hash = trace_payload_hash(&sanitized.payload);
    sanitized
}

// 对 canonical payload 计算带前缀的 SHA-256 摘要。
fn trace_payload_hash(payload: &Value) -> String {
    let canonical = canonical_json(payload);
    let digest = Sha256::digest(canonical.as_bytes());
    format!("{TRACE_HASH_PREFIX}{digest:x}")
}

// 以稳定 key 顺序序列化 JSON，作为哈希输入。
fn canonical_json(value: &Value) -> String {
    match value {
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {
            serde_json::to_string(value).expect("JSON scalar serialization cannot fail")
        }
        Value::Array(items) => format!(
            "[{}]",
            items
                .iter()
                .map(canonical_json)
                .collect::<Vec<_>>()
                .join(",")
        ),
        Value::Object(fields) => {
            let ordered = fields.iter().collect::<BTreeMap<_, _>>();
            let entries = ordered
                .into_iter()
                .map(|(key, value)| {
                    format!(
                        "{}:{}",
                        serde_json::to_string(key).expect("JSON key serialization cannot fail"),
                        canonical_json(value)
                    )
                })
                .collect::<Vec<_>>()
                .join(",");
            format!("{{{entries}}}")
        }
    }
}

// 递归识别并替换 secret-like JSON 值。
fn redact_secret_like_value(value: Value) -> Value {
    match value {
        Value::String(text) => Value::String(redact_secret_like_text(&text)),
        Value::Array(items) => {
            Value::Array(items.into_iter().map(redact_secret_like_value).collect())
        }
        Value::Object(entries) => Value::Object(
            entries
                .into_iter()
                .map(|(key, value)| {
                    if contains_secret_like(&key) {
                        (key, Value::String(REDACTED_ARTIFACT_VALUE.to_string()))
                    } else {
                        (key, redact_secret_like_value(value))
                    }
                })
                .collect(),
        ),
        other => other,
    }
}

// 判断 artifact URI、摘要或 metadata 是否触发脱敏。
fn artifact_needs_redaction(uri: &str, summary: &str, metadata: &Value) -> bool {
    contains_secret_like(uri)
        || contains_secret_like(summary)
        || value_contains_secret_like(metadata)
}

// 递归判断 JSON 值是否包含 secret-like 内容。
fn value_contains_secret_like(value: &Value) -> bool {
    match value {
        Value::String(text) => contains_secret_like(text),
        Value::Array(items) => items.iter().any(value_contains_secret_like),
        Value::Object(entries) => entries
            .iter()
            .any(|(key, value)| contains_secret_like(key) || value_contains_secret_like(value)),
        _ => false,
    }
}

// 将文本中的 secret-like 片段替换为统一占位符。
fn redact_secret_like_text(text: &str) -> String {
    if contains_secret_like(text) {
        REDACTED_ARTIFACT_VALUE.to_string()
    } else {
        text.to_string()
    }
}

// 判断文本是否命中敏感 marker 或 core 敏感文本规则。
fn contains_secret_like(text: &str) -> bool {
    let lowered = text.to_ascii_lowercase();
    contains_sensitive_text(text)
        || SENSITIVE_ARTIFACT_MARKERS
            .iter()
            .any(|marker| lowered.contains(marker))
}

#[cfg(test)]
// store crate 内部的 SQLite pragma 单元测试。
mod tests {
    use super::*;

    // 验证新连接启用外键、WAL、busy timeout 与 secure delete pragma。
    #[test]
    fn open_configures_sqlite_connection_pragmas() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
        let foreign_keys: u32 = store
            .connection
            .query_row("pragma foreign_keys", [], |row| row.get(0))
            .expect("foreign keys pragma");
        let journal_mode: String = store
            .connection
            .query_row("pragma journal_mode", [], |row| row.get(0))
            .expect("journal mode pragma");
        let busy_timeout_ms: u64 = store
            .connection
            .query_row("pragma busy_timeout", [], |row| row.get(0))
            .expect("busy timeout pragma");
        let secure_delete: u32 = store
            .connection
            .query_row("pragma secure_delete", [], |row| row.get(0))
            .expect("secure delete pragma");

        assert_eq!(foreign_keys, 1);
        assert_eq!(journal_mode.to_ascii_lowercase(), "wal");
        assert_eq!(busy_timeout_ms, SQLITE_BUSY_TIMEOUT_MS);
        assert_eq!(secure_delete, 1);
    }

    #[cfg(windows)]
    #[test]
    fn windows_file_identity_distinguishes_files_with_equal_attributes() {
        use std::os::windows::fs::MetadataExt;

        let dir = tempfile::tempdir().expect("temp dir");
        let first_path = dir.path().join("first.sqlite3");
        let second_path = dir.path().join("second.sqlite3");
        std::fs::write(&first_path, b"first").expect("first file");
        std::fs::write(&second_path, b"second").expect("second file");
        assert_eq!(
            std::fs::metadata(&first_path)
                .expect("first metadata")
                .file_attributes(),
            std::fs::metadata(&second_path)
                .expect("second metadata")
                .file_attributes()
        );

        let first = open_store_file(&first_path, false).expect("open first");
        let second = open_store_file(&second_path, false).expect("open second");
        assert_ne!(
            checked_store_file_identity(&first).expect("first identity"),
            checked_store_file_identity(&second).expect("second identity")
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_file_identity_rejects_hard_links() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("sessions.sqlite3");
        let alias = dir.path().join("sessions-alias.sqlite3");
        std::fs::write(&path, b"store").expect("store file");
        std::fs::hard_link(&path, &alias).expect("hard link");
        let file = open_store_file(&path, false).expect("open hard-linked store");
        let error = checked_store_file_identity(&file).expect_err("hard link rejected");
        assert!(matches!(
            error,
            StoreError::InvalidState(message) if message.contains("hard links")
        ));
    }
}
