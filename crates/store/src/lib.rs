#![deny(unsafe_code)]

//! 由 SQLite 支持的会话、turn、items 与执行所有权恢复状态。
//!
//! 变更操作使用事务和显式绑定，使 turn 结果与执行所有权能够恢复，
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
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use singularity_core::contains_sensitive_text;
use singularity_protocol::{
    Item, ItemKind, ItemStatus, Thread, ThreadStatus, Turn, TurnInputDelivery, TurnStatus,
};
/// 供上层重建 conversation history 的 protocol 类型。
pub use singularity_protocol::{ConversationMessage, ConversationRole};
use thiserror::Error;
use uuid::Uuid;

const SCHEMA_VERSION: u32 = 13;
const THREAD_POLICY_SCHEMA_VERSION: u32 = 9;
const INITIAL_SCHEMA_MIGRATION: &str = "0001_initial_session_store";
// 保留历史 migration id；当前代码不表达密码学 ledger。
const DURABLE_EVENT_HISTORY_SCHEMA_MIGRATION: &str = "0002_durable_ledger";
const PENDING_TOOL_CALL_SCHEMA_MIGRATION: &str = "0004_pending_tool_calls";
const STORE_HARDENING_SCHEMA_MIGRATION: &str = "0005_store_hardening";
const CONVERSATION_HISTORY_SCHEMA_MIGRATION: &str = "0006_conversation_history";
const PENDING_EXECUTION_STATE_SCHEMA_MIGRATION: &str = "0007_pending_execution_state";
const APPROVAL_EXECUTION_RECOVERY_SCHEMA_MIGRATION: &str = "0008_approval_execution_recovery";
const THREAD_POLICY_SNAPSHOT_SCHEMA_MIGRATION: &str = "0009_thread_policy_snapshot";
const STABLE_ENUM_TEXT_SCHEMA_MIGRATION: &str = "0010_stable_enum_text";
const TYPED_PERMISSION_RESOURCE_SCHEMA_MIGRATION: &str = "0011_typed_permission_resources";
const TYPED_TRACE_SPAN_SCHEMA_MIGRATION: &str = "0012_typed_trace_spans";
const TURN_RESUME_CHECKPOINT_SCHEMA_MIGRATION: &str = "0013_turn_resume_checkpoints";
// This migration existed only while the removed sidecar-run runtime was live.
// It is accepted while reading old databases and deliberately not retained in the current schema.
const RETIRED_ACTIVE_SIDECAR_RUN_SCHEMA_MIGRATION: &str = "0003_active_sidecar_runs";
const STORE_INITIALIZATION_LOCK_RETRY_MS: u64 = 10;
const HISTORY_SCAN_BATCH_TURNS: usize = 64;
const SQLITE_BUSY_TIMEOUT_MS: u64 = 5_000;
const SQLITE_FOREIGN_KEYS_PRAGMA: &str = "foreign_keys";
const SQLITE_JOURNAL_MODE_PRAGMA: &str = "journal_mode";
const SQLITE_JOURNAL_MODE_WAL: &str = "WAL";
const SQLITE_SECURE_DELETE_PRAGMA: &str = "secure_delete";
const REDACTED_USER_INPUT: &str = "[redacted sensitive user input]";
const REDACTED_ASSISTANT_OUTPUT: &str = "[redacted sensitive assistant output]";

mod connection;
mod error;
mod file_identity;
mod migration;
mod support;
mod thread_turn;
mod turn_input;

pub use connection::{SessionStore, SessionStoreDescriptor, WorkspaceExecutionGuard};
pub use error::{StoreError, StoreResult};
pub use thread_turn::{
    AllocatedAssistantItemId, AllocatedTurnId, CommitTurnOutcomeParams, CommittedTurnOutcome,
    CreateStartedTurnParams, StartedTurn, ThreadHistoryPage, TurnOutcomeAuthority,
};
pub use turn_input::{PendingTurnInput, TurnBoundaryState};

pub(crate) use error::{DbEnum, decode_db_enum, unknown_db_enum};
pub(crate) use file_identity::StoreIdentityGuard;

#[cfg(test)]
mod tests;
