#![forbid(unsafe_code)]

//! 由 SQLite 支持的 session、turn、approval、trace、artifact 和 recovery 状态。
//!
//! 变更操作使用事务和显式绑定，使 approval checkpoint、turn 结果和执行所有权能够恢复，
//! 且无需重放未知的外部副作用。

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use singularity_core::contains_sensitive_text;
use singularity_policy::{ApprovalDecision, ApprovalOutcome, ApprovalRequest};
use singularity_protocol::{
    ArtifactRef, Item, ItemKind, ItemStatus, Thread, ThreadStatus, TraceEvent, Turn, TurnStatus,
};
pub use singularity_protocol::{ConversationMessage, ConversationRole};
use thiserror::Error;
use uuid::Uuid;

const SCHEMA_VERSION: u32 = 8;
const INITIAL_SCHEMA_MIGRATION: &str = "0001_initial_session_store";
const DURABLE_LEDGER_SCHEMA_MIGRATION: &str = "0002_durable_ledger";
const PENDING_TOOL_CALL_SCHEMA_MIGRATION: &str = "0004_pending_tool_calls";
const STORE_HARDENING_SCHEMA_MIGRATION: &str = "0005_store_hardening";
const CONVERSATION_HISTORY_SCHEMA_MIGRATION: &str = "0006_conversation_history";
const PENDING_EXECUTION_STATE_SCHEMA_MIGRATION: &str = "0007_pending_execution_state";
const APPROVAL_EXECUTION_RECOVERY_SCHEMA_MIGRATION: &str = "0008_approval_execution_recovery";
const SQLITE_BUSY_TIMEOUT_MS: u64 = 5_000;
const STORE_INITIALIZATION_LOCK_RETRY_MS: u64 = 10;
const SQLITE_FOREIGN_KEYS_PRAGMA: &str = "foreign_keys";
const SQLITE_JOURNAL_MODE_PRAGMA: &str = "journal_mode";
const SQLITE_JOURNAL_MODE_WAL: &str = "WAL";
const SQLITE_SECURE_DELETE_PRAGMA: &str = "secure_delete";
const REDACTED_ARTIFACT_VALUE: &str = "[redacted]";
const REDACTED_USER_INPUT: &str = "[redacted sensitive user input]";
const REDACTED_ASSISTANT_OUTPUT: &str = "[redacted sensitive assistant output]";
const TRACE_HASH_PREFIX: &str = "sha256:";
const SENSITIVE_ARTIFACT_MARKERS: [&str; 5] =
    ["api_key", "authorization", "password", "secret", "token"];
const APPROVAL_BINDING_REQUIRED: &str =
    "approval request must include explicit thread_id and turn_id";
const APPROVAL_TURN_THREAD_MISMATCH: &str = "approval request thread_id must match bound turn";
const PENDING_TOOL_CALL_ID_MISMATCH: &str =
    "pending tool call tool_call_id must match approval request";
const PENDING_TOOL_CALL_NAME_MISMATCH: &str =
    "pending tool call tool_name must match approval request";
const PENDING_TOOL_CALL_RESOURCES_MISMATCH: &str =
    "pending tool call resources must match approval request";
const PENDING_TOOL_CALL_TURN_MISMATCH: &str =
    "pending tool call turn_id must match approval request";
const PENDING_TOOL_CALL_THREAD_MISMATCH: &str =
    "pending tool call thread_id must match approval request";
const APPROVAL_CHECKPOINT_REQUIRED: &str =
    "pending approval must include an internal AgentLoop checkpoint";
const APPROVAL_CHECKPOINT_VERSION: u64 = 1;
const APPROVAL_CHECKPOINT_THREAD_MISMATCH: &str =
    "approval checkpoint thread_id must match approval request";
const APPROVAL_CHECKPOINT_TURN_MISMATCH: &str =
    "approval checkpoint turn_id must match approval request";
const APPROVAL_CHECKPOINT_REQUEST_MISMATCH: &str =
    "approval checkpoint request_id must match approval request";
const APPROVAL_CHECKPOINT_TOOL_CALL_MISMATCH: &str =
    "approval checkpoint tool_call_id must match pending tool call";
const PENDING_APPROVAL_ALLOW_REQUIRES_ACTIVE_THREAD: &str =
    "pending approval allow requires an active thread";

/// 保留存储、完整性、绑定和执行所有权原因的错误。
#[derive(Debug, Error)]
pub enum StoreError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("record not found: {0}")]
    NotFound(String),
    #[error("record already exists: {0}")]
    AlreadyExists(String),
    #[error("unsupported schema version {found}; supported version is {supported}")]
    UnsupportedSchema { found: u32, supported: u32 },
    #[error("trace integrity check failed: {0}")]
    TraceIntegrity(String),
    #[error("invalid store state: {0}")]
    InvalidState(String),
    #[error("store initialization lock error: {0}")]
    InitializationLock(#[source] std::io::Error),
    #[error("workspace execution lock error: {0}")]
    ExecutionLock(#[source] std::io::Error),
    #[error("thread {thread_id} already has non-terminal turn {turn_id}")]
    ThreadHasNonterminalTurn { thread_id: String, turn_id: String },
    #[error("workspace already has non-terminal turn {turn_id} in thread {thread_id}")]
    WorkspaceHasNonterminalTurn { thread_id: String, turn_id: String },
}

/// 所有 session store 操作返回的结果类型。
pub type StoreResult<T> = Result<T, StoreError>;

/// SQLite store 的公开描述及其支持的 schema 版本。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionStoreDescriptor {
    pub backend: String,
    pub path: String,
    pub schema_version: u32,
}

/// 按模型重建所需顺序排列的一页已完成对话历史。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ThreadHistoryPage {
    pub messages: Vec<ConversationMessage>,
    pub next_before_turn_sequence: Option<u64>,
}

/// 创建 turn、用户 item、trace 和初始历史页后得到的原子结果。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct StartedTurn {
    pub turn: Turn,
    pub item: Item,
    pub trace: TraceEvent,
    pub history: ThreadHistoryPage,
}

/// turn 的原子结果，以及相关的持久化 plan、assistant item 和 trace（如有）。
#[derive(Debug, Clone, PartialEq)]
pub struct CommittedTurnOutcome {
    pub turn: Turn,
    pub plan_item: Option<Item>,
    pub assistant_item: Option<Item>,
    pub trace: TraceEvent,
}

/// 负责 turn 生命周期、approval、trace、artifact 和 recovery 的持久化 SQLite store。
pub struct SessionStore {
    connection: Connection,
    descriptor: SessionStoreDescriptor,
    runtime_path: Option<PathBuf>,
}

/// 由进程持有、用于串行化 thread 或 workspace 执行的所有权 guard。
pub struct WorkspaceExecutionGuard {
    execution_scope: WorkspaceExecutionScope,
    store_path: PathBuf,
    _lock_file: File,
}

enum WorkspaceExecutionScope {
    Workspace(String),
    Thread(String),
}

/// approval 决定，以及 AppServer 所需的 checkpoint 和 trace 数据。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RecordedApprovalDecision {
    pub request: ApprovalRequest,
    pub decision: ApprovalDecision,
    pub pending_tool_call: Option<Value>,
    pub trace: TraceEvent,
}

/// 注册脱敏、内容寻址 artifact 引用的输入。
pub struct RegisterArtifactRefParams<'a> {
    pub run_id: &'a str,
    pub item_id: Option<&'a str>,
    pub kind: &'a str,
    pub uri: &'a str,
    pub content_digest: &'a str,
    pub summary: &'a str,
    pub metadata: Value,
}

/// 为一个终止、已中断或 approval 阻塞的 turn 结果提交的持久化字段。
pub struct CommitTurnOutcomeParams<'a> {
    pub status: TurnStatus,
    pub agent_loop_status: &'a str,
    pub assistant_delta: Option<&'a str>,
    pub plan: Option<&'a Value>,
    pub trace: &'a TraceEvent,
}

impl SessionStore {
    /// 打开 SQLite store，配置 fail-closed pragma，并执行 schema 检查/迁移。
    pub fn open(path: impl AsRef<Path>) -> StoreResult<Self> {
        let path = path.as_ref();
        let _initialization_lock = acquire_store_initialization_lock(path)?;
        let connection = Connection::open(path)?;
        configure_connection(&connection)?;
        let runtime_path = if path == Path::new(":memory:") {
            None
        } else {
            Some(std::fs::canonicalize(path).map_err(StoreError::ExecutionLock)?)
        };
        let store = Self {
            connection,
            descriptor: SessionStoreDescriptor {
                backend: "sqlite".to_string(),
                path: path.to_string_lossy().to_string(),
                schema_version: SCHEMA_VERSION,
            },
            runtime_path,
        };
        store.init_schema()?;
        Ok(store)
    }

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
                    serde_json::to_string(&TurnStatus::Completed)?,
                    serde_json::to_string(&TurnStatus::Failed)?,
                    serde_json::to_string(&TurnStatus::Interrupted)?,
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

    pub fn create_thread(&self, model: Option<&str>, cwd: Option<&str>) -> StoreResult<Thread> {
        let thread = Self::new_thread(model, cwd);
        Self::insert_thread(&self.connection, &thread)?;
        Ok(thread)
    }

    pub fn list_threads(&self) -> StoreResult<Vec<Thread>> {
        let mut statement = self
            .connection
            .prepare("select thread_id, model, cwd, status from threads order by rowid")?;
        let rows = statement.query_map([], |row| self.thread_from_row(row))?;
        let mut threads = Vec::new();
        for row in rows {
            threads.push(row?);
        }
        Ok(threads)
    }

    pub fn get_thread(&self, thread_id: &str) -> StoreResult<Thread> {
        self.connection
            .query_row(
                "select thread_id, model, cwd, status from threads where thread_id = ?1",
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
            params![serde_json::to_string(&status)?, thread_id],
        )?;
        if changed == 0 {
            return Err(StoreError::NotFound(format!("thread {thread_id}")));
        }
        transaction.commit()?;
        self.get_thread(thread_id)
    }

    pub fn delete_thread(&self, thread_id: &str) -> StoreResult<()> {
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        Self::ensure_thread_has_no_nonterminal_turn(&transaction, thread_id)?;
        let mut approval_request_ids = BTreeSet::new();
        {
            let mut statement = transaction.prepare("select request_id, payload from approvals")?;
            let rows = statement.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            for row in rows {
                let (request_id, payload) = row?;
                let request: ApprovalRequest = serde_json::from_str(&payload)?;
                if request.thread_id == thread_id {
                    approval_request_ids.insert(request_id);
                }
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

    pub fn create_thread_with_trace(
        &self,
        model: Option<&str>,
        cwd: Option<&str>,
        component: &str,
        summary: &str,
    ) -> StoreResult<(Thread, TraceEvent)> {
        let transaction = self.connection.unchecked_transaction()?;
        let thread = Self::new_thread(model, cwd);
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

    /// 原子地创建 turn、清理其输入、记录 trace，并读取此前的历史。
    pub fn create_turn_with_input_trace_and_history(
        &self,
        thread_id: &str,
        agent_loop_status: &str,
        input: Value,
        component: &str,
        summary: &str,
        history_turn_limit: usize,
    ) -> StoreResult<StartedTurn> {
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        Self::ensure_active_thread(&transaction, thread_id)?;
        Self::ensure_workspace_has_no_nonterminal_turn(&transaction, thread_id, None)?;
        let history =
            Self::read_thread_history_from(&transaction, thread_id, None, history_turn_limit)?;
        let turn_sequence = Self::next_turn_sequence(&transaction, thread_id)?;
        let turn = Self::new_turn(thread_id, agent_loop_status);
        Self::insert_turn(&transaction, &turn, turn_sequence)?;
        let item_sequence = Self::next_item_sequence(&transaction, &turn.turn_id)?;
        let (input, redacted) = sanitize_item_payload(&ItemKind::UserMessage, input)?;
        let item = Self::new_item(&turn.turn_id, ItemKind::UserMessage, input);
        Self::insert_item(&transaction, &item, item_sequence, redacted)?;
        let trace = TraceEvent::new(
            format!("trace_{}", turn.turn_id),
            thread_id,
            turn.turn_id.clone(),
            component,
            summary,
        );
        let trace = Self::insert_trace(&transaction, &trace)?;
        transaction.commit()?;
        Ok(StartedTurn {
            turn,
            item,
            trace,
            history,
        })
    }

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

    pub fn update_turn_status(&self, turn_id: &str, status: TurnStatus) -> StoreResult<Turn> {
        self.ensure_turn_status_update_allowed(turn_id, &status, None)?;
        let changed = self.connection.execute(
            "update turns set status = ?1 where turn_id = ?2",
            params![serde_json::to_string(&status)?, turn_id],
        )?;
        if changed == 0 {
            return Err(StoreError::NotFound(format!("turn {turn_id}")));
        }
        self.get_turn(turn_id)
    }

    pub fn update_turn_state(
        &self,
        turn_id: &str,
        status: TurnStatus,
        agent_loop_status: &str,
    ) -> StoreResult<Turn> {
        self.ensure_turn_status_update_allowed(turn_id, &status, Some(agent_loop_status))?;
        let changed = self.connection.execute(
            "update turns set status = ?1, agent_loop_status = ?2 where turn_id = ?3",
            params![serde_json::to_string(&status)?, agent_loop_status, turn_id],
        )?;
        if changed == 0 {
            return Err(StoreError::NotFound(format!("turn {turn_id}")));
        }
        self.get_turn(turn_id)
    }

    /// 在一个事务中提交 turn 状态及其持久化 item 和 trace。
    pub fn commit_turn_outcome(
        &self,
        turn_id: &str,
        params: CommitTurnOutcomeParams<'_>,
    ) -> StoreResult<CommittedTurnOutcome> {
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        let committed = self.commit_turn_outcome_in_transaction(&transaction, turn_id, params)?;
        transaction.commit()?;
        Ok(committed)
    }

    fn commit_turn_outcome_in_transaction(
        &self,
        transaction: &Transaction<'_>,
        turn_id: &str,
        params: CommitTurnOutcomeParams<'_>,
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
        validate_turn_status_update(&current, &status, Some(agent_loop_status))?;
        if trace.run_id != current.thread_id || trace.session_id != current.turn_id {
            return Err(StoreError::InvalidState(
                "turn outcome trace must be bound to the same thread and turn".to_string(),
            ));
        }
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
            params![serde_json::to_string(&status)?, agent_loop_status, turn_id],
        )?;
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
        let trace = Self::insert_trace(transaction, trace)?;
        Ok(CommittedTurnOutcome {
            turn,
            plan_item,
            assistant_item,
            trace,
        })
    }

    /// 记录取消，同时保留 pending approval 与执行中工作之间的区别。
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
        if trace.run_id != turn.thread_id || trace.session_id != turn.turn_id {
            return Err(StoreError::InvalidState(
                "turn cancellation trace must be bound to the same thread and turn".to_string(),
            ));
        }
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
                params![serde_json::to_string(&TurnStatus::Interrupted)?, turn_id],
            )?;
            let mut terminal_trace = trace.clone();
            terminal_trace.summary = "turn interrupted while approval pending".to_string();
            terminal_trace.payload = serde_json::json!({
                "turn_id": turn_id,
                "agent_loop_status": "cancelled",
                "pending_approval_cancelled": true,
            });
            Self::insert_trace(&transaction, &terminal_trace)?;
            turn.status = TurnStatus::Interrupted;
            turn.agent_loop_status = "cancelled".to_string();
            transaction.commit()?;
            return Ok(turn);
        }
        transaction.execute(
            "update turns set agent_loop_status = 'cancel_requested' where turn_id = ?1",
            params![turn_id],
        )?;
        Self::insert_trace(&transaction, trace)?;
        turn.agent_loop_status = "cancel_requested".to_string();
        transaction.commit()?;
        Ok(turn)
    }

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

    pub fn get_turn_user_input(&self, turn_id: &str) -> StoreResult<Value> {
        let payload: String = self
            .connection
            .query_row(
                "select payload from items where turn_id = ?1 and kind = ?2 order by item_sequence limit 1",
                params![turn_id, serde_json::to_string(&ItemKind::UserMessage)?],
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
    pub fn append_trace(&self, event: &TraceEvent) -> StoreResult<()> {
        let _ = Self::insert_trace(&self.connection, event)?;
        Ok(())
    }

    pub fn list_trace(&self, run_id: &str) -> StoreResult<Vec<TraceEvent>> {
        self.list_trace_page(run_id, None, None)
    }

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
        let mut statement = self.connection.prepare(
            "select payload from trace_events where run_id = ?1 order by rowid limit ?2 offset ?3",
        )?;
        let rows = statement.query_map(params![run_id, limit, offset], |row| {
            row.get::<_, String>(0)
        })?;
        let mut events = Vec::new();
        for row in rows {
            events.push(decode_trace_payload(&row?)?);
        }
        if events.is_empty() {
            if self.trace_run_exists(run_id)? {
                return Ok(events);
            }
            return Err(StoreError::NotFound(format!("trace run {run_id}")));
        }
        Ok(events)
    }

    pub fn tail_trace(
        &self,
        run_id: &str,
        limit: usize,
        offset: Option<usize>,
    ) -> StoreResult<Vec<TraceEvent>> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let offset = i64::try_from(offset.unwrap_or(0)).unwrap_or(i64::MAX);
        let mut statement = self.connection.prepare(
            "select payload from trace_events where run_id = ?1 order by rowid desc limit ?2 offset ?3",
        )?;
        let rows = statement.query_map(params![run_id, limit, offset], |row| {
            row.get::<_, String>(0)
        })?;
        let mut events = Vec::new();
        for row in rows {
            events.push(decode_trace_payload(&row?)?);
        }
        if events.is_empty() && !self.trace_run_exists(run_id)? {
            return Err(StoreError::NotFound(format!("trace run {run_id}")));
        }
        events.reverse();
        Ok(events)
    }

    pub fn show_trace(&self, event_id: &str) -> StoreResult<TraceEvent> {
        let payload: String = self
            .connection
            .query_row(
                "select payload from trace_events where event_id = ?1",
                params![event_id],
                |row| row.get(0),
            )
            .map_err(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => {
                    StoreError::NotFound(format!("trace event {event_id}"))
                }
                other => StoreError::Sqlite(other),
            })?;
        decode_trace_payload(&payload)
    }

    pub fn create_approval(&self, request: &ApprovalRequest) -> StoreResult<()> {
        insert_approval(&self.connection, request)?;
        Ok(())
    }

    pub fn create_approval_with_trace(
        &self,
        request: &ApprovalRequest,
        component: &str,
        summary: &str,
    ) -> StoreResult<TraceEvent> {
        self.create_approval_with_pending_tool_call_and_trace(request, None, component, summary)
    }

    /// 保存 approval 请求和可选 checkpoint，并将其绑定到阻塞的 turn。
    pub fn create_approval_with_pending_tool_call_and_trace(
        &self,
        request: &ApprovalRequest,
        pending_tool_call: Option<Value>,
        component: &str,
        summary: &str,
    ) -> StoreResult<TraceEvent> {
        let transaction = self.connection.unchecked_transaction()?;
        insert_approval(&transaction, request)?;
        if let Some(payload) = pending_tool_call {
            let tool_call_id = pending_tool_call_id(&transaction, request, &payload)?;
            ensure_request_turn_binding(&transaction, request)?;
            transaction.execute(
                "insert into pending_tool_calls(request_id, thread_id, turn_id, tool_call_id, payload, execution_state) values(?1, ?2, ?3, ?4, ?5, 'pending')",
                params![
                    request.request_id,
                    request.thread_id,
                    request.turn_id,
                    tool_call_id,
                    serde_json::to_string(&payload)?
                ],
            )?;
            let changed = transaction.execute(
                "update turns set status = ?1, agent_loop_status = 'blocked'
                 where turn_id = ?2 and status in (?3, ?1)
                   and agent_loop_status in ('running', 'blocked')",
                params![
                    serde_json::to_string(&TurnStatus::Blocked)?,
                    request.turn_id,
                    serde_json::to_string(&TurnStatus::Running)?,
                ],
            )?;
            if changed != 1 {
                return Err(StoreError::InvalidState(
                    "pending approval requires a running or blocked turn".to_string(),
                ));
            }
        }
        let trace = TraceEvent::new(
            format!("trace_{}", request.request_id),
            request.thread_id.clone(),
            request.thread_id.clone(),
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
        let trace = Self::insert_trace(&transaction, &trace)?;
        transaction.commit()?;
        Ok(trace)
    }

    pub fn list_pending_approvals(&self) -> StoreResult<Vec<ApprovalRequest>> {
        let mut statement = self.connection.prepare(
            "select payload from approvals where decision_outcome is null order by rowid",
        )?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        let mut approvals = Vec::new();
        for row in rows {
            approvals.push(serde_json::from_str(&row?)?);
        }
        Ok(approvals)
    }

    pub fn get_pending_approval(&self, request_id: &str) -> StoreResult<ApprovalRequest> {
        let payload: String = self
            .connection
            .query_row(
                "select payload from approvals where request_id = ?1 and decision_outcome is null",
                params![request_id],
                |row| row.get(0),
            )
            .map_err(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => {
                    StoreError::NotFound(format!("approval {request_id}"))
                }
                other => StoreError::Sqlite(other),
            })?;
        Ok(serde_json::from_str(&payload)?)
    }

    pub fn has_pending_tool_call(&self, request_id: &str) -> StoreResult<bool> {
        Self::exists_in_transaction(
            &self.connection,
            "select 1 from pending_tool_calls where request_id = ?1",
            request_id,
        )
    }

    pub fn list_approval_decisions(&self) -> StoreResult<Vec<ApprovalDecision>> {
        let mut statement = self
            .connection
            .prepare("select payload from approval_decisions order by rowid")?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        let mut decisions = Vec::new();
        for row in rows {
            decisions.push(serde_json::from_str(&row?)?);
        }
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
        let request_payload: String = transaction
            .query_row(
                "select payload from approvals where request_id = ?1 and decision_outcome is null",
                params![decision.request_id],
                |row| row.get(0),
            )
            .map_err(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => {
                    StoreError::NotFound(format!("approval {}", decision.request_id))
                }
                other => StoreError::Sqlite(other),
            })?;
        let request: ApprovalRequest = serde_json::from_str(&request_payload)?;
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
                let payload = serde_json::from_str::<Value>(&payload)?;
                let _ = pending_tool_call_id(&transaction, &request, &payload)?;
                Some(payload)
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
            if turn_status != serde_json::to_string(&TurnStatus::Blocked)?
                || agent_loop_status != "blocked"
            {
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
            if thread_status != serde_json::to_string(&ThreadStatus::Active)? {
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
        if decision.outcome == ApprovalOutcome::Defer {
            let trace = TraceEvent {
                task_id: Some(request.turn_id.clone()),
                payload: serde_json::json!({
                    "request_id": decision.request_id,
                    "decision_id": decision.decision_id,
                    "outcome": decision.outcome,
                }),
                ..TraceEvent::new(
                    format!("trace_{}_defer_{}", decision.request_id, Uuid::new_v4()),
                    request.thread_id.clone(),
                    request.thread_id.clone(),
                    component,
                    "approval deferred",
                )
            };
            let trace = Self::insert_trace(&transaction, &trace)?;
            transaction.commit()?;
            return Ok(RecordedApprovalDecision {
                request,
                decision: decision.clone(),
                pending_tool_call,
                trace,
            });
        }
        let changed = transaction.execute(
            "update approvals set decision_outcome = ?1, decision_reason = ?2 where request_id = ?3 and decision_outcome is null",
            params![
                serde_json::to_string(&decision.outcome)?,
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
                serde_json::to_string(&decision.outcome)?,
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
                    ..TraceEvent::new(
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
                        serde_json::to_string(&TurnStatus::Failed)?,
                        request.turn_id,
                        serde_json::to_string(&TurnStatus::Blocked)?,
                    ],
                )?;
                if changed != 1 {
                    return Err(StoreError::InvalidState(
                        "pending approval is not bound to a blocked turn".to_string(),
                    ));
                }
                Self::insert_trace(&transaction, &terminal_trace)?;
            }
        }
        let trace = TraceEvent::new(
            format!("trace_{}", decision.decision_id),
            request.thread_id.clone(),
            request.thread_id.clone(),
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
        let trace = Self::insert_trace(&transaction, &trace)?;
        transaction.commit()?;
        Ok(RecordedApprovalDecision {
            request,
            decision: decision.clone(),
            pending_tool_call,
            trace,
        })
    }

    /// 原子地用 turn 结果和后续 checkpoint（如有）解决执行中的 approval。
    pub fn commit_turn_outcome_and_resolve_pending_execution(
        &self,
        request_id: &str,
        params: CommitTurnOutcomeParams<'_>,
        next_approvals: &[(ApprovalRequest, Value)],
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
        let committed =
            self.commit_turn_outcome_in_transaction(&transaction, &bound_turn_id, params)?;
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
            let tool_call_id = pending_tool_call_id(&transaction, request, checkpoint)?;
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
                ..TraceEvent::new(
                    format!("trace_{}", request.request_id),
                    request.thread_id.clone(),
                    request.thread_id.clone(),
                    "approval",
                    "approval requested",
                )
            };
            Self::insert_trace(&transaction, &approval_trace)?;
        }
        transaction.commit()?;
        Ok(committed)
    }

    /// 对执行中的 approval 进行协调，而不重放其未知的外部副作用。
    fn recover_incomplete_approval_executions_for_thread(
        transaction: &Connection,
        thread_id: &str,
    ) -> StoreResult<Vec<String>> {
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
            "select a.rowid, a.request_id, a.payload, a.decision_outcome,
                    a.decision_reason, p.rowid, p.thread_id, p.turn_id, p.tool_call_id, p.payload,
                    p.execution_state
             from approvals a
             left join pending_tool_calls p on p.request_id = a.request_id
             order by a.rowid",
        )?;
        let persisted_rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Option<i64>>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, Option<String>>(7)?,
                    row.get::<_, Option<String>>(8)?,
                    row.get::<_, Option<String>>(9)?,
                    row.get::<_, Option<String>>(10)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        drop(statement);

        let mut rows = Vec::with_capacity(persisted_rows.len());
        for (
            approval_rowid,
            request_id,
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
            let request: ApprovalRequest = serde_json::from_str(&request_payload)?;
            if request.thread_id != thread_id {
                continue;
            }
            if request.request_id != request_id {
                return Err(StoreError::InvalidState(format!(
                    "approval {request_id} payload request_id mismatch"
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
            let status: TurnStatus = serde_json::from_str(&turn_status)?;
            let decision = decision
                .as_deref()
                .map(serde_json::from_str::<ApprovalOutcome>)
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
                        "approval {request_id} has inconsistent decision ledger"
                    )));
                }
                None => {}
                Some(ApprovalOutcome::Defer) => {
                    return Err(StoreError::InvalidState(format!(
                        "approval {request_id} has inconsistent decision ledger"
                    )));
                }
                Some(expected_outcome) => {
                    let [(decision_id, ledger_request_id, outcome, reason, payload)] =
                        decision_rows.as_slice()
                    else {
                        return Err(StoreError::InvalidState(format!(
                            "approval {request_id} has inconsistent decision ledger"
                        )));
                    };
                    let ledger_decision: ApprovalDecision = serde_json::from_str(payload)?;
                    if ledger_request_id != &request_id
                        || serde_json::from_str::<ApprovalOutcome>(outcome)? != expected_outcome
                        || decision_reason.as_deref() != Some(reason.as_str())
                        || ledger_decision.decision_id != *decision_id
                        || ledger_decision.request_id != request_id
                        || ledger_decision.outcome != expected_outcome
                        || ledger_decision.reason != *reason
                    {
                        return Err(StoreError::InvalidState(format!(
                            "approval {request_id} has inconsistent decision ledger"
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
                    let payload = serde_json::from_str::<Value>(payload)?;
                    let _ = pending_tool_call_id(transaction, &request, &payload)?;
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
                    params![serde_json::to_string(&TurnStatus::Interrupted)?, turn_id],
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
                ..TraceEvent::new(
                    format!("trace_{request_id}_recovered"),
                    row.request.thread_id.clone(),
                    turn_id.clone(),
                    "app_server",
                    summary,
                )
            };
            Self::insert_trace(transaction, &trace)?;
            transaction.execute(
                "delete from pending_tool_calls where request_id = ?1",
                params![request_id],
            )?;
            recovered.push(request_id.clone());
        }
        Ok(recovered)
    }

    /// 对执行 guard 覆盖的每个 thread 应用所有权丢失恢复。
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

    fn recover_abandoned_thread_execution(&self, thread_id: &str) -> StoreResult<()> {
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        Self::recover_incomplete_approval_executions_for_thread(&transaction, thread_id)?;
        Self::recover_abandoned_turns_for_thread(&transaction, thread_id)?;
        transaction.commit()?;
        Ok(())
    }

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
                    serde_json::to_string(&TurnStatus::Completed)?,
                    serde_json::to_string(&TurnStatus::Failed)?,
                    serde_json::to_string(&TurnStatus::Interrupted)?,
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
            let status: TurnStatus = serde_json::from_str(&serialized_status)?;
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
                params![serde_json::to_string(&TurnStatus::Interrupted)?, &turn_id],
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
                ..TraceEvent::new(
                    format!("trace_{turn_id}_owner_lost_{}", Uuid::new_v4()),
                    thread_id,
                    turn_id.clone(),
                    "app_server",
                    "turn interrupted after execution owner was lost",
                )
            };
            Self::insert_trace(transaction, &trace)?;
            recovered.push(turn_id);
        }
        Ok(recovered)
    }

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

    pub fn get_approval_decision(&self, decision_id: &str) -> StoreResult<ApprovalDecision> {
        let payload: String = self
            .connection
            .query_row(
                "select payload from approval_decisions where decision_id = ?1",
                params![decision_id],
                |row| row.get(0),
            )
            .map_err(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => {
                    StoreError::NotFound(format!("approval decision {decision_id}"))
                }
                other => StoreError::Sqlite(other),
            })?;
        Ok(serde_json::from_str(&payload)?)
    }

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
        let artifact = ArtifactRef {
            artifact_id: format!("artifact_{}", short_id()),
            run_id: run_id.to_string(),
            item_id: item_id.map(str::to_string),
            kind: kind.to_string(),
            uri: redact_secret_like_text(uri),
            content_digest: content_digest.to_string(),
            summary: redact_secret_like_text(summary),
            redacted: artifact_needs_redaction(uri, summary, &metadata),
            metadata: redact_secret_like_value(metadata),
        };
        self.connection.execute(
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
        Ok(artifact)
    }

    pub fn get_artifact_ref(&self, artifact_id: &str) -> StoreResult<ArtifactRef> {
        self.connection
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
            })
    }

    pub fn list_artifact_refs(&self, run_id: &str) -> StoreResult<Vec<ArtifactRef>> {
        let mut statement = self.connection.prepare(
            "select artifact_id, run_id, item_id, kind, uri, content_digest, summary, metadata, redacted from artifact_refs where run_id = ?1 order by rowid",
        )?;
        let rows = statement.query_map(params![run_id], artifact_from_row)?;
        let mut artifacts = Vec::new();
        for row in rows {
            artifacts.push(row?);
        }
        Ok(artifacts)
    }

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

        let completed_turn = serde_json::to_string(&TurnStatus::Completed)?;
        let completed_item = serde_json::to_string(&ItemStatus::Completed)?;
        let user_message = serde_json::to_string(&ItemKind::UserMessage)?;
        let agent_message = serde_json::to_string(&ItemKind::AgentMessage)?;
        let before_turn_sequence = before_turn_sequence
            .map(|sequence| sequence_to_sql(sequence, "before turn sequence"))
            .transpose()?;
        let candidate_limit = i64::try_from(turn_limit.saturating_add(1)).unwrap_or(i64::MAX);
        let mut statement = connection.prepare(
            "select turns.turn_sequence
             from turns
             where turns.thread_id = ?1
               and turns.status = ?2
               and (?3 is null or turns.turn_sequence < ?3)
               and exists (
                   select 1
                   from items as user_item
                   join items as agent_item on agent_item.turn_id = user_item.turn_id
                   where user_item.turn_id = turns.turn_id
                     and user_item.status = ?4
                     and agent_item.status = ?4
                     and user_item.kind = ?5
                     and agent_item.kind = ?6
                     and user_item.item_sequence < agent_item.item_sequence
               )
               and 1 = (
                   select count(*)
                   from items
                   where items.turn_id = turns.turn_id
                     and items.status = ?4
                     and items.kind = ?5
               )
               and 1 = (
                   select count(*)
                   from items
                   where items.turn_id = turns.turn_id
                     and items.status = ?4
                     and items.kind = ?6
               )
             order by turns.turn_sequence desc
             limit ?7",
        )?;
        let rows = statement.query_map(
            params![
                thread_id,
                completed_turn,
                before_turn_sequence,
                completed_item,
                user_message,
                agent_message,
                candidate_limit
            ],
            |row| row.get::<_, i64>(0),
        )?;
        let mut turn_sequences = Vec::new();
        for row in rows {
            turn_sequences.push(sequence_from_sql(row?, "turn sequence")?);
        }
        if turn_sequences.is_empty() {
            return Ok(ThreadHistoryPage {
                messages: Vec::new(),
                next_before_turn_sequence: None,
            });
        }

        let has_more = turn_sequences.len() > turn_limit;
        turn_sequences.truncate(turn_limit);
        let next_before_turn_sequence = if has_more {
            Some(*turn_sequences.last().ok_or_else(|| {
                StoreError::InvalidState("history page lost its oldest turn".to_string())
            })?)
        } else {
            None
        };
        let selected_turn_limit = i64::try_from(turn_sequences.len()).unwrap_or(i64::MAX);
        let mut statement = connection.prepare(
            "with selected_turns as (
                select turns.turn_id, turns.turn_sequence
                from turns
                where turns.thread_id = ?1
                  and turns.status = ?2
                  and (?3 is null or turns.turn_sequence < ?3)
                  and exists (
                      select 1
                      from items as user_item
                      join items as agent_item on agent_item.turn_id = user_item.turn_id
                      where user_item.turn_id = turns.turn_id
                        and user_item.status = ?4
                        and agent_item.status = ?4
                        and user_item.kind = ?5
                        and agent_item.kind = ?6
                        and user_item.item_sequence < agent_item.item_sequence
                  )
                  and 1 = (
                      select count(*)
                      from items
                      where items.turn_id = turns.turn_id
                        and items.status = ?4
                        and items.kind = ?5
                  )
                  and 1 = (
                      select count(*)
                      from items
                      where items.turn_id = turns.turn_id
                        and items.status = ?4
                        and items.kind = ?6
                  )
                order by turns.turn_sequence desc
                limit ?7
            )
            select items.item_id, items.turn_id, selected_turns.turn_sequence,
                   items.item_sequence, items.kind, items.payload, items.redacted
            from selected_turns
            join items on items.turn_id = selected_turns.turn_id
            where items.status = ?4 and items.kind in (?5, ?6)
            order by selected_turns.turn_sequence, items.item_sequence",
        )?;
        let rows = statement.query_map(
            params![
                thread_id,
                completed_turn,
                before_turn_sequence,
                completed_item,
                user_message,
                agent_message,
                selected_turn_limit,
            ],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, bool>(6)?,
                ))
            },
        )?;
        let mut messages = Vec::new();
        for row in rows {
            let (item_id, turn_id, turn_sequence, item_sequence, kind, payload, stored_redacted) =
                row?;
            let kind: ItemKind = serde_json::from_str(&kind).map_err(|_| {
                StoreError::InvalidState("malformed conversation item kind".to_string())
            })?;
            let payload: Value = serde_json::from_str(&payload).map_err(|_| {
                StoreError::InvalidState("malformed conversation item payload".to_string())
            })?;
            let (payload, detected_redaction) = sanitize_item_payload(&kind, payload)?;
            let (role, content) = conversation_projection(&kind, &payload)?;
            messages.push(ConversationMessage {
                item_id,
                turn_id,
                turn_sequence: sequence_from_sql(turn_sequence, "turn sequence")?,
                item_sequence: sequence_from_sql(item_sequence, "item sequence")?,
                role,
                content,
                redacted: stored_redacted || detected_redaction,
            });
        }

        Ok(ThreadHistoryPage {
            messages,
            next_before_turn_sequence,
        })
    }

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
        let status: ThreadStatus = serde_json::from_str(&status).map_err(|_| {
            StoreError::InvalidState(format!("thread {thread_id} has malformed status"))
        })?;
        if status != ThreadStatus::Active {
            return Err(StoreError::InvalidState(format!(
                "thread {thread_id} is not active"
            )));
        }
        Ok(())
    }

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
                    serde_json::to_string(&TurnStatus::Completed)?,
                    serde_json::to_string(&TurnStatus::Failed)?,
                    serde_json::to_string(&TurnStatus::Interrupted)?,
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
                    serde_json::to_string(&TurnStatus::Completed)?,
                    serde_json::to_string(&TurnStatus::Failed)?,
                    serde_json::to_string(&TurnStatus::Interrupted)?,
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

    fn next_turn_sequence(connection: &Connection, thread_id: &str) -> StoreResult<u64> {
        let current = connection.query_row(
            "select max(turn_sequence) from turns where thread_id = ?1",
            params![thread_id],
            |row| row.get::<_, Option<i64>>(0),
        )?;
        next_sequence(current, "turn sequence")
    }

    fn next_item_sequence(connection: &Connection, turn_id: &str) -> StoreResult<u64> {
        let current = connection.query_row(
            "select max(item_sequence) from items where turn_id = ?1",
            params![turn_id],
            |row| row.get::<_, Option<i64>>(0),
        )?;
        next_sequence(current, "item sequence")
    }
    fn trace_run_exists(&self, run_id: &str) -> StoreResult<bool> {
        self.exists(
            "select 1 from trace_events where run_id = ?1 limit 1",
            run_id,
        )
    }

    fn thread_from_row(&self, row: &rusqlite::Row<'_>) -> rusqlite::Result<Thread> {
        let status: String = row.get(3)?;
        Ok(Thread {
            thread_id: row.get(0)?,
            model: row.get(1)?,
            cwd: row.get(2)?,
            status: serde_json::from_str(&status).map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    3,
                    rusqlite::types::Type::Text,
                    Box::new(error),
                )
            })?,
        })
    }

    fn turn_from_row(&self, row: &rusqlite::Row<'_>) -> rusqlite::Result<Turn> {
        let status: String = row.get(2)?;
        Ok(Turn {
            turn_id: row.get(0)?,
            thread_id: row.get(1)?,
            status: serde_json::from_str(&status).map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    2,
                    rusqlite::types::Type::Text,
                    Box::new(error),
                )
            })?,
            agent_loop_status: row.get(3)?,
        })
    }

    fn exists(&self, query: &str, value: &str) -> StoreResult<bool> {
        let result = self.connection.query_row(query, params![value], |_| Ok(()));
        match result {
            Ok(()) => Ok(true),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(false),
            Err(error) => Err(StoreError::Sqlite(error)),
        }
    }

    /// 初始化或校验 schema；遇到不完整版本或未来版本时保持 fail-closed。
    fn init_schema(&self) -> StoreResult<()> {
        self.connection.execute_batch(
            "
            create table if not exists schema_meta(
                schema_version integer not null
            );
            ",
        )?;
        self.fail_closed_on_future_schema()?;
        self.connection.execute_batch(
            "
            create table if not exists schema_migrations(
                migration_id text primary key,
                applied_at text not null default current_timestamp
            );
            create table if not exists threads(
                thread_id text primary key,
                model text,
                cwd text,
                status text not null
            );
            create table if not exists turns(
                turn_id text primary key,
                thread_id text not null,
                turn_sequence integer not null check(turn_sequence > 0),
                status text not null,
                agent_loop_status text not null,
                foreign key(thread_id) references threads(thread_id)
            );
            create table if not exists items(
                item_id text primary key,
                turn_id text not null,
                item_sequence integer not null check(item_sequence > 0),
                kind text not null,
                payload text not null,
                status text not null,
                redacted integer not null check(redacted in (0, 1)),
                foreign key(turn_id) references turns(turn_id)
            );
            create table if not exists trace_events(
                event_id text primary key,
                run_id text not null,
                session_id text not null default '',
                payload text not null
            );
            create table if not exists approvals(
                request_id text primary key,
                payload text not null,
                decision_outcome text,
                decision_reason text
            );
            create table if not exists approval_decisions(
                decision_id text primary key,
                request_id text not null,
                outcome text not null,
                reason text not null,
                payload text not null,
                foreign key(request_id) references approvals(request_id)
            );
            create table if not exists artifact_refs(
                artifact_id text primary key,
                run_id text not null,
                item_id text,
                kind text not null,
                uri text not null,
                content_digest text not null,
                summary text not null,
                metadata text not null,
                redacted integer not null
            );
            create table if not exists pending_tool_calls(
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
            ",
        )?;
        self.ensure_trace_session_id_column()?;
        self.ensure_pending_tool_call_thread_id_column()?;
        self.ensure_pending_tool_call_tool_call_id_column()?;
        self.ensure_pending_execution_state_column()?;
        for migration in [
            INITIAL_SCHEMA_MIGRATION,
            DURABLE_LEDGER_SCHEMA_MIGRATION,
            PENDING_TOOL_CALL_SCHEMA_MIGRATION,
            STORE_HARDENING_SCHEMA_MIGRATION,
            PENDING_EXECUTION_STATE_SCHEMA_MIGRATION,
        ] {
            self.connection.execute(
                "insert or ignore into schema_migrations(migration_id) values(?1)",
                params![migration],
            )?;
        }
        self.migrate_conversation_history()?;
        self.ensure_required_foreign_keys()?;
        self.migrate_approval_execution_schema()?;
        self.fail_closed_on_foreign_key_violations()?;
        Ok(())
    }

    fn migrate_approval_execution_schema(&self) -> StoreResult<()> {
        self.connection
            .pragma_update(None, SQLITE_FOREIGN_KEYS_PRAGMA, "OFF")?;
        let migration_result = (|| -> StoreResult<()> {
            let transaction =
                Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
            let applied = Self::exists_in_transaction(
                &transaction,
                "select 1 from schema_migrations where migration_id = ?1",
                APPROVAL_EXECUTION_RECOVERY_SCHEMA_MIGRATION,
            )?;
            let table_sql: String = transaction.query_row(
                "select sql from sqlite_master where type = 'table' and name = 'pending_tool_calls'",
                [],
                |row| row.get(0),
            )?;
            let normalized = table_sql
                .to_ascii_lowercase()
                .replace(char::is_whitespace, "");
            let has_v8_check =
                normalized.contains("check(execution_statein('pending','executing'))");
            if applied && !has_v8_check {
                return Err(StoreError::InvalidState(
                    "approval execution migration is recorded but schema is incomplete".to_string(),
                ));
            }
            if !applied {
                transaction.execute_batch(
                    "
                    create table pending_tool_calls_v8(
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
                    insert into pending_tool_calls_v8(
                        request_id, thread_id, turn_id, tool_call_id, payload, execution_state
                    )
                    select request_id, thread_id, turn_id, tool_call_id, payload,
                           case when execution_state = 'pending' then 'pending' else 'executing' end
                    from pending_tool_calls;
                    drop table pending_tool_calls;
                    alter table pending_tool_calls_v8 rename to pending_tool_calls;
                    ",
                )?;
                transaction.execute(
                    "insert into schema_migrations(migration_id) values(?1)",
                    params![APPROVAL_EXECUTION_RECOVERY_SCHEMA_MIGRATION],
                )?;
                write_schema_version(&transaction)?;
            }
            transaction.commit()?;
            Ok(())
        })();
        let foreign_keys_result = self
            .connection
            .pragma_update(None, SQLITE_FOREIGN_KEYS_PRAGMA, "ON")
            .map_err(StoreError::Sqlite);
        migration_result?;
        foreign_keys_result?;
        Ok(())
    }

    fn migrate_conversation_history(&self) -> StoreResult<()> {
        self.connection
            .pragma_update(None, SQLITE_FOREIGN_KEYS_PRAGMA, "OFF")?;
        let migration_result = (|| -> StoreResult<(bool, bool)> {
            let transaction =
                Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
            let migration_applied = Self::exists_in_transaction(
                &transaction,
                "select 1 from schema_migrations where migration_id = ?1",
                CONVERSATION_HISTORY_SCHEMA_MIGRATION,
            )?;
            let has_turn_sequence = table_has_column(&transaction, "turns", "turn_sequence")?;
            let has_item_sequence = table_has_column(&transaction, "items", "item_sequence")?;
            let has_item_redacted = table_has_column(&transaction, "items", "redacted")?;
            let has_complete_schema = has_turn_sequence && has_item_sequence && has_item_redacted;
            let has_partial_schema = has_turn_sequence || has_item_sequence || has_item_redacted;
            if migration_applied && !has_complete_schema {
                return Err(StoreError::InvalidState(
                    "conversation history migration is recorded but schema is incomplete"
                        .to_string(),
                ));
            }
            if !has_complete_schema && has_partial_schema {
                return Err(StoreError::InvalidState(
                    "conversation history schema is partially migrated".to_string(),
                ));
            }
            let legacy_item_count: u64 =
                transaction.query_row("select count(*) from items", [], |row| row.get(0))?;
            let needs_secure_rewrite = !migration_applied && legacy_item_count > 0;
            if !has_complete_schema {
                transaction.execute_batch(
                    "
                    create table turns_v6(
                        turn_id text primary key,
                        thread_id text not null,
                        turn_sequence integer not null check(turn_sequence > 0),
                        status text not null,
                        agent_loop_status text not null,
                        foreign key(thread_id) references threads(thread_id)
                    );
                    insert into turns_v6(
                        turn_id, thread_id, turn_sequence, status, agent_loop_status
                    )
                    select turn_id, thread_id,
                           row_number() over(partition by thread_id order by rowid),
                           status, agent_loop_status
                    from turns;

                    create table items_v6(
                        item_id text primary key,
                        turn_id text not null,
                        item_sequence integer not null check(item_sequence > 0),
                        kind text not null,
                        payload text not null,
                        status text not null,
                        redacted integer not null check(redacted in (0, 1)),
                        foreign key(turn_id) references turns_v6(turn_id)
                    );
                    insert into items_v6(
                        item_id, turn_id, item_sequence, kind, payload, status, redacted
                    )
                    select item_id, turn_id,
                           row_number() over(partition by turn_id order by rowid),
                           kind, payload, status, 0
                    from items;

                    drop table items;
                    drop table turns;
                    alter table turns_v6 rename to turns;
                    alter table items_v6 rename to items;
                    ",
                )?;
            }
            if !migration_applied {
                sanitize_migrated_items(&transaction)?;
            }
            let invalid_turn_sequences: u64 = transaction.query_row(
                "select count(*) from turns where turn_sequence is null or turn_sequence <= 0",
                [],
                |row| row.get(0),
            )?;
            let invalid_items: u64 = transaction.query_row(
                "select count(*) from items where item_sequence is null or item_sequence <= 0 or redacted not in (0, 1)",
                [],
                |row| row.get(0),
            )?;
            if invalid_turn_sequences != 0 || invalid_items != 0 {
                return Err(StoreError::InvalidState(
                    "conversation history contains invalid sequence or redaction values"
                        .to_string(),
                ));
            }
            transaction.execute_batch(
                "
                create unique index if not exists turns_thread_sequence_unique
                    on turns(thread_id, turn_sequence);
                create unique index if not exists items_turn_sequence_unique
                    on items(turn_id, item_sequence);
                create index if not exists turns_history_lookup
                    on turns(thread_id, status, turn_sequence);
                create index if not exists items_history_lookup
                    on items(turn_id, status, kind, item_sequence);
                ",
            )?;
            fail_closed_on_foreign_key_violations(&transaction)?;
            if migration_applied {
                write_schema_version(&transaction)?;
            }
            transaction.commit()?;
            Ok((migration_applied, needs_secure_rewrite))
        })();
        let foreign_keys_result = self
            .connection
            .pragma_update(None, SQLITE_FOREIGN_KEYS_PRAGMA, "ON")
            .map_err(StoreError::Sqlite);
        let (migration_applied, needs_secure_rewrite) = migration_result?;
        foreign_keys_result?;
        if needs_secure_rewrite {
            secure_rewrite_database(&self.connection)?;
        }
        if !migration_applied {
            let transaction =
                Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
            fail_closed_on_foreign_key_violations(&transaction)?;
            transaction.execute(
                "insert into schema_migrations(migration_id) values(?1)",
                params![CONVERSATION_HISTORY_SCHEMA_MIGRATION],
            )?;
            write_schema_version(&transaction)?;
            transaction.commit()?;
        }
        Ok(())
    }

    fn fail_closed_on_future_schema(&self) -> StoreResult<()> {
        let schema_version = self
            .connection
            .query_row("select max(schema_version) from schema_meta", [], |row| {
                row.get::<_, Option<u32>>(0)
            })
            .optional()?
            .flatten();
        if let Some(found) = schema_version
            && found > SCHEMA_VERSION
        {
            return Err(StoreError::UnsupportedSchema {
                found,
                supported: SCHEMA_VERSION,
            });
        }
        Ok(())
    }

    fn ensure_trace_session_id_column(&self) -> StoreResult<()> {
        let mut statement = self.connection.prepare("pragma table_info(trace_events)")?;
        let rows = statement.query_map([], |row| row.get::<_, String>(1))?;
        for row in rows {
            if row? == "session_id" {
                return Ok(());
            }
        }
        self.connection.execute(
            "alter table trace_events add column session_id text not null default ''",
            [],
        )?;
        Ok(())
    }

    fn ensure_pending_tool_call_tool_call_id_column(&self) -> StoreResult<()> {
        let mut statement = self
            .connection
            .prepare("pragma table_info(pending_tool_calls)")?;
        let rows = statement.query_map([], |row| row.get::<_, String>(1))?;
        for row in rows {
            if row? == "tool_call_id" {
                return Ok(());
            }
        }
        self.connection.execute(
            "alter table pending_tool_calls add column tool_call_id text not null default ''",
            [],
        )?;
        Ok(())
    }

    fn ensure_pending_tool_call_thread_id_column(&self) -> StoreResult<()> {
        if self.table_has_column("pending_tool_calls", "thread_id")? {
            return Ok(());
        }
        self.connection.execute(
            "alter table pending_tool_calls add column thread_id text not null default ''",
            [],
        )?;
        self.connection.execute(
            "
            update pending_tool_calls
            set thread_id = (
                select turns.thread_id from turns where turns.turn_id = pending_tool_calls.turn_id
            )
            where thread_id = ''
            ",
            [],
        )?;
        Ok(())
    }

    fn ensure_pending_execution_state_column(&self) -> StoreResult<()> {
        if self.table_has_column("pending_tool_calls", "execution_state")? {
            return Ok(());
        }
        self.connection.execute(
            "alter table pending_tool_calls add column execution_state text not null default 'pending'",
            [],
        )?;
        Ok(())
    }

    fn ensure_required_foreign_keys(&self) -> StoreResult<()> {
        if self.table_references("turns", "threads")?
            && self.table_references("items", "turns")?
            && self.table_references("approval_decisions", "approvals")?
            && self.table_references("pending_tool_calls", "approvals")?
            && self.table_references("pending_tool_calls", "threads")?
            && self.table_references("pending_tool_calls", "turns")?
        {
            return Ok(());
        }
        self.rebuild_foreign_key_tables()?;
        self.fail_closed_on_foreign_key_violations()
    }

    fn rebuild_foreign_key_tables(&self) -> StoreResult<()> {
        self.connection
            .pragma_update(None, SQLITE_FOREIGN_KEYS_PRAGMA, "OFF")?;
        let rebuild_result = (|| -> StoreResult<()> {
            let transaction = self.connection.unchecked_transaction()?;
            transaction.execute_batch(
                "
                create table turns_new(
                    turn_id text primary key,
                    thread_id text not null,
                    turn_sequence integer not null check(turn_sequence > 0),
                    status text not null,
                    agent_loop_status text not null,
                    foreign key(thread_id) references threads(thread_id)
                );
                insert into turns_new(
                    turn_id, thread_id, turn_sequence, status, agent_loop_status
                )
                select turn_id, thread_id, turn_sequence, status, agent_loop_status from turns;

                create table items_new(
                    item_id text primary key,
                    turn_id text not null,
                    item_sequence integer not null check(item_sequence > 0),
                    kind text not null,
                    payload text not null,
                    status text not null,
                    redacted integer not null check(redacted in (0, 1)),
                    foreign key(turn_id) references turns_new(turn_id)
                );
                insert into items_new(
                    item_id, turn_id, item_sequence, kind, payload, status, redacted
                )
                select item_id, turn_id, item_sequence, kind, payload, status, redacted from items;
                drop table items;
                drop table turns;
                alter table turns_new rename to turns;
                alter table items_new rename to items;
                create unique index turns_thread_sequence_unique
                    on turns(thread_id, turn_sequence);
                create unique index items_turn_sequence_unique
                    on items(turn_id, item_sequence);
                create index turns_history_lookup
                    on turns(thread_id, status, turn_sequence);
                create index items_history_lookup
                    on items(turn_id, status, kind, item_sequence);

                create table approval_decisions_new(
                    decision_id text primary key,
                    request_id text not null,
                    outcome text not null,
                    reason text not null,
                    payload text not null,
                    foreign key(request_id) references approvals(request_id)
                );
                insert into approval_decisions_new(decision_id, request_id, outcome, reason, payload)
                select decision_id, request_id, outcome, reason, payload from approval_decisions;
                drop table approval_decisions;
                alter table approval_decisions_new rename to approval_decisions;

                create table pending_tool_calls_new(
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
                insert into pending_tool_calls_new(request_id, thread_id, turn_id, tool_call_id, payload, execution_state)
                select request_id, thread_id, turn_id, tool_call_id, payload,
                       case when execution_state = 'pending' then 'pending' else 'executing' end
                from pending_tool_calls;
                drop table pending_tool_calls;
                alter table pending_tool_calls_new rename to pending_tool_calls;
                ",
            )?;
            fail_closed_on_foreign_key_violations(&transaction)?;
            transaction.commit()?;
            Ok(())
        })();
        let foreign_keys_result = self
            .connection
            .pragma_update(None, SQLITE_FOREIGN_KEYS_PRAGMA, "ON")
            .map_err(StoreError::Sqlite);
        rebuild_result?;
        foreign_keys_result?;
        Ok(())
    }
    fn fail_closed_on_foreign_key_violations(&self) -> StoreResult<()> {
        fail_closed_on_foreign_key_violations(&self.connection)
    }

    fn table_references(&self, table: &str, parent: &str) -> StoreResult<bool> {
        let query = format!("pragma foreign_key_list({table})");
        let mut statement = self.connection.prepare(&query)?;
        let rows = statement.query_map([], |row| row.get::<_, String>(2))?;
        for row in rows {
            if row? == parent {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn table_has_column(&self, table: &str, column: &str) -> StoreResult<bool> {
        table_has_column(&self.connection, table, column)
    }

    fn ensure_turn_status_update_allowed(
        &self,
        turn_id: &str,
        next_status: &TurnStatus,
        next_agent_loop_status: Option<&str>,
    ) -> StoreResult<()> {
        let current = self.get_turn(turn_id)?;
        validate_turn_status_update(&current, next_status, next_agent_loop_status)
    }

    fn new_thread(model: Option<&str>, cwd: Option<&str>) -> Thread {
        Thread {
            thread_id: format!("thread_{}", short_id()),
            model: model.map(str::to_string),
            cwd: cwd.map(str::to_string),
            status: ThreadStatus::Active,
        }
    }

    fn new_turn(thread_id: &str, agent_loop_status: &str) -> Turn {
        Turn {
            turn_id: format!("turn_{}", short_id()),
            thread_id: thread_id.to_string(),
            status: TurnStatus::Running,
            agent_loop_status: agent_loop_status.to_string(),
        }
    }

    fn new_item(turn_id: &str, kind: ItemKind, payload: Value) -> Item {
        Item {
            item_id: format!("item_{}", short_id()),
            turn_id: turn_id.to_string(),
            kind,
            payload,
            status: ItemStatus::Completed,
        }
    }

    fn insert_thread(connection: &Connection, thread: &Thread) -> StoreResult<()> {
        connection.execute(
            "insert into threads(thread_id, model, cwd, status) values(?1, ?2, ?3, ?4)",
            params![
                thread.thread_id,
                thread.model,
                thread.cwd,
                serde_json::to_string(&thread.status)?
            ],
        )?;
        Ok(())
    }

    fn insert_turn(connection: &Connection, turn: &Turn, turn_sequence: u64) -> StoreResult<()> {
        connection.execute(
            "insert into turns(turn_id, thread_id, turn_sequence, status, agent_loop_status) values(?1, ?2, ?3, ?4, ?5)",
            params![
                turn.turn_id,
                turn.thread_id,
                sequence_to_sql(turn_sequence, "turn sequence")?,
                serde_json::to_string(&turn.status)?,
                turn.agent_loop_status
            ],
        )?;
        Ok(())
    }

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
                serde_json::to_string(&item.kind)?,
                serde_json::to_string(&item.payload)?,
                serde_json::to_string(&item.status)?,
                redacted,
            ],
        )?;
        Ok(())
    }
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

fn configure_connection(connection: &Connection) -> StoreResult<()> {
    connection.busy_timeout(Duration::from_millis(SQLITE_BUSY_TIMEOUT_MS))?;
    connection.pragma_update(None, SQLITE_SECURE_DELETE_PRAGMA, "ON")?;
    connection.pragma_update(None, SQLITE_FOREIGN_KEYS_PRAGMA, "ON")?;
    connection.pragma_update(None, SQLITE_JOURNAL_MODE_PRAGMA, SQLITE_JOURNAL_MODE_WAL)?;
    Ok(())
}

fn workspace_execution_scope(thread: &Thread) -> WorkspaceExecutionScope {
    match thread.cwd.as_deref().filter(|cwd| !cwd.trim().is_empty()) {
        Some(cwd) => WorkspaceExecutionScope::Workspace(cwd.to_string()),
        None => WorkspaceExecutionScope::Thread(thread.thread_id.clone()),
    }
}

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

fn write_schema_version(connection: &Connection) -> StoreResult<()> {
    connection.execute("delete from schema_meta", [])?;
    connection.execute(
        "insert into schema_meta(schema_version) values(?1)",
        params![SCHEMA_VERSION],
    )?;
    Ok(())
}

fn secure_rewrite_database(connection: &Connection) -> StoreResult<()> {
    truncate_wal(connection)?;
    connection.execute_batch("vacuum")?;
    truncate_wal(connection)
}

fn truncate_wal(connection: &Connection) -> StoreResult<()> {
    let busy: i64 =
        connection.query_row("pragma wal_checkpoint(TRUNCATE)", [], |row| row.get(0))?;
    if busy != 0 {
        return Err(StoreError::InvalidState(
            "sqlite WAL checkpoint remained busy during secure rewrite".to_string(),
        ));
    }
    Ok(())
}

fn table_has_column(connection: &Connection, table: &str, column: &str) -> StoreResult<bool> {
    let query = format!("pragma table_info({table})");
    let mut statement = connection.prepare(&query)?;
    let rows = statement.query_map([], |row| row.get::<_, String>(1))?;
    for row in rows {
        if row? == column {
            return Ok(true);
        }
    }
    Ok(false)
}

fn next_sequence(current: Option<i64>, label: &str) -> StoreResult<u64> {
    let current = current
        .map(|sequence| sequence_from_sql(sequence, label))
        .transpose()?
        .unwrap_or(0);
    current
        .checked_add(1)
        .ok_or_else(|| StoreError::InvalidState(format!("{label} overflow")))
}

fn sequence_to_sql(sequence: u64, label: &str) -> StoreResult<i64> {
    i64::try_from(sequence)
        .map_err(|_| StoreError::InvalidState(format!("{label} exceeds sqlite integer range")))
}

fn sequence_from_sql(sequence: i64, label: &str) -> StoreResult<u64> {
    u64::try_from(sequence)
        .map_err(|_| StoreError::InvalidState(format!("{label} must be non-negative")))
}

fn sanitize_migrated_items(connection: &Connection) -> StoreResult<()> {
    let items = {
        let mut statement = connection
            .prepare("select item_id, kind, payload from items order by turn_id, item_sequence")?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        rows.collect::<Result<Vec<_>, _>>()?
    };
    for (item_id, kind, payload) in items {
        let kind: ItemKind = serde_json::from_str(&kind)?;
        let payload: Value = serde_json::from_str(&payload)?;
        let (payload, redacted) = sanitize_item_payload(&kind, payload)?;
        connection.execute(
            "update items set payload = ?1, redacted = ?2 where item_id = ?3",
            params![serde_json::to_string(&payload)?, redacted, item_id],
        )?;
    }
    Ok(())
}
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

fn fail_closed_on_foreign_key_violations(connection: &Connection) -> StoreResult<()> {
    let violation = connection
        .query_row("pragma foreign_key_check", [], |row| {
            let table: String = row.get(0)?;
            let row_id: i64 = row.get(1)?;
            Ok(format!("{table}:{row_id}"))
        })
        .optional()?;
    if let Some(violation) = violation {
        return Err(StoreError::InvalidState(format!(
            "store foreign key violation after migration: {violation}"
        )));
    }
    Ok(())
}

fn pending_tool_call_id(
    connection: &Connection,
    request: &ApprovalRequest,
    payload: &Value,
) -> StoreResult<String> {
    ensure_request_turn_binding(connection, request)?;
    let payload_request_id = payload
        .get("request_id")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            StoreError::InvalidState("pending tool call request_id is required".to_string())
        })?;
    if payload_request_id != request.request_id {
        return Err(StoreError::InvalidState(
            "pending tool call request_id must match approval request".to_string(),
        ));
    }
    let tool_call_id = payload
        .get("tool_call_id")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            StoreError::InvalidState("pending tool call tool_call_id is required".to_string())
        })?;
    if request.tool_call_id.as_deref() != Some(tool_call_id) {
        return Err(StoreError::InvalidState(
            PENDING_TOOL_CALL_ID_MISMATCH.to_string(),
        ));
    }
    let tool_name = payload
        .get("tool_name")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| StoreError::InvalidState(APPROVAL_CHECKPOINT_REQUIRED.to_string()))?;
    if tool_name != request.action {
        return Err(StoreError::InvalidState(
            PENDING_TOOL_CALL_NAME_MISMATCH.to_string(),
        ));
    }
    let resources = payload
        .get("resources")
        .and_then(Value::as_array)
        .ok_or_else(|| StoreError::InvalidState(APPROVAL_CHECKPOINT_REQUIRED.to_string()))?
        .iter()
        .map(|value| value.as_str().map(str::to_string))
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| StoreError::InvalidState(APPROVAL_CHECKPOINT_REQUIRED.to_string()))?;
    if resources != request.resources {
        return Err(StoreError::InvalidState(
            PENDING_TOOL_CALL_RESOURCES_MISMATCH.to_string(),
        ));
    }
    let checkpoint_version = payload
        .get("checkpoint_version")
        .and_then(Value::as_u64)
        .ok_or_else(|| StoreError::InvalidState(APPROVAL_CHECKPOINT_REQUIRED.to_string()))?;
    if checkpoint_version != APPROVAL_CHECKPOINT_VERSION {
        return Err(StoreError::InvalidState(
            "unsupported approval checkpoint version".to_string(),
        ));
    }
    let checkpoint_request_id = payload
        .get("request_id")
        .and_then(Value::as_str)
        .ok_or_else(|| StoreError::InvalidState(APPROVAL_CHECKPOINT_REQUIRED.to_string()))?;
    if checkpoint_request_id != request.request_id {
        return Err(StoreError::InvalidState(
            APPROVAL_CHECKPOINT_REQUEST_MISMATCH.to_string(),
        ));
    }
    let checkpoint_thread_id = payload
        .get("thread_id")
        .and_then(Value::as_str)
        .ok_or_else(|| StoreError::InvalidState(APPROVAL_CHECKPOINT_REQUIRED.to_string()))?;
    if checkpoint_thread_id != request.thread_id {
        return Err(StoreError::InvalidState(
            APPROVAL_CHECKPOINT_THREAD_MISMATCH.to_string(),
        ));
    }
    let checkpoint_turn_id = payload
        .get("turn_id")
        .and_then(Value::as_str)
        .ok_or_else(|| StoreError::InvalidState(APPROVAL_CHECKPOINT_REQUIRED.to_string()))?;
    if checkpoint_turn_id != request.turn_id {
        return Err(StoreError::InvalidState(
            APPROVAL_CHECKPOINT_TURN_MISMATCH.to_string(),
        ));
    }
    let checkpoint_tool_call_id = payload
        .get("tool_call_id")
        .and_then(Value::as_str)
        .ok_or_else(|| StoreError::InvalidState(APPROVAL_CHECKPOINT_REQUIRED.to_string()))?;
    if checkpoint_tool_call_id != tool_call_id {
        return Err(StoreError::InvalidState(
            APPROVAL_CHECKPOINT_TOOL_CALL_MISMATCH.to_string(),
        ));
    }
    for field in [
        "messages",
        "tool_results",
        "used_approval_grants",
        "approval_count",
        "model_turns",
        "completion",
    ] {
        if payload.get(field).is_none() {
            return Err(StoreError::InvalidState(
                APPROVAL_CHECKPOINT_REQUIRED.to_string(),
            ));
        }
    }
    Ok(tool_call_id.to_string())
}

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

fn is_terminal_turn_status(status: &TurnStatus) -> bool {
    matches!(
        status,
        TurnStatus::Completed | TurnStatus::Failed | TurnStatus::Interrupted
    )
}

fn validate_turn_status_update(
    current: &Turn,
    next_status: &TurnStatus,
    next_agent_loop_status: Option<&str>,
) -> StoreResult<()> {
    if current.agent_loop_status == "cancel_requested" && *next_status != TurnStatus::Interrupted {
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

fn insert_approval(connection: &Connection, request: &ApprovalRequest) -> StoreResult<()> {
    ensure_approval_request_binding(connection, request)?;
    connection
        .execute(
            "insert into approvals(request_id, payload, decision_outcome, decision_reason) values(?1, ?2, null, null)",
            params![request.request_id, serde_json::to_string(request)?],
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

fn short_id() -> String {
    Uuid::new_v4().simple().to_string()[..12].to_string()
}

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

fn trace_payload_hash(payload: &Value) -> String {
    let canonical = canonical_json(payload);
    let digest = Sha256::digest(canonical.as_bytes());
    format!("{TRACE_HASH_PREFIX}{digest:x}")
}

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

fn artifact_needs_redaction(uri: &str, summary: &str, metadata: &Value) -> bool {
    contains_secret_like(uri)
        || contains_secret_like(summary)
        || value_contains_secret_like(metadata)
}

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

fn redact_secret_like_text(text: &str) -> String {
    if contains_secret_like(text) {
        REDACTED_ARTIFACT_VALUE.to_string()
    } else {
        text.to_string()
    }
}

fn contains_secret_like(text: &str) -> bool {
    let lowered = text.to_ascii_lowercase();
    contains_sensitive_text(text)
        || SENSITIVE_ARTIFACT_MARKERS
            .iter()
            .any(|marker| lowered.contains(marker))
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
