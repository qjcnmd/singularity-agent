#![deny(unsafe_code)]

//! 由 SQLite 支持的会话、turn、追踪、产物和恢复状态。
//!
//! 变更操作使用事务和显式绑定，使 turn 结果和执行所有权能够恢复，
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
use singularity_core::{
    Timestamp, bounded_stable_code, contains_sensitive_text, is_protected_path,
};
use singularity_protocol::{
    ArtifactRef, Item, ItemKind, ItemStatus, Thread, ThreadStatus, TraceBindingError, TraceEvent,
    TraceMetric, TraceMetricAvailability, TraceMetricDistribution, TraceMetricName,
    TraceMetricSample, TraceMetricSampleKind, TraceMetricUnavailableReason, TraceMetrics,
    TraceProviderProtocol, TraceSpanKind, TraceSpanPhase, TraceSpanProjection, TraceSpanStatus,
    TraceToolStatus, TraceUsage, Turn, TurnInputDelivery, TurnStatus,
};
/// 供上层重建 conversation history 的 protocol 类型。
pub use singularity_protocol::{ConversationMessage, ConversationRole};
use thiserror::Error;
use uuid::Uuid;

const SCHEMA_VERSION: u32 = 13;
const THREAD_POLICY_SCHEMA_VERSION: u32 = 9;
const INITIAL_SCHEMA_MIGRATION: &str = "0001_initial_session_store";
// 保留历史 migration id；当前代码表达 trace event history，不表达密码学 ledger。
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

mod checkpoint_recovery;
mod connection;
mod error;
mod file_identity;
mod migration;
mod support;
mod thread_turn;
mod trace_artifact;
mod turn_input;

pub use checkpoint_recovery::{ToolExecution, ToolExecutionState};
pub use connection::{SessionStore, SessionStoreDescriptor, WorkspaceExecutionGuard};
pub use error::{StoreError, StoreResult};
pub(crate) use thread_turn::typed_turn_end_trace;
pub use thread_turn::{
    AllocatedAssistantItemId, AllocatedTurnId, CommitTurnOutcomeParams, CommittedTurnOutcome,
    CreateStartedTurnParams, StartedTurn, ThreadHistoryPage, TurnOutcomeAuthority,
};
pub use trace_artifact::RegisterArtifactRefParams;
pub use turn_input::{PendingTurnInput, TurnBoundaryState};

pub(crate) use connection::WorkspaceExecutionScope;
pub(crate) use error::{DbEnum, decode_db_enum, unknown_db_enum};
pub(crate) use file_identity::StoreIdentityGuard;
pub(crate) use trace_artifact::{
    find_trace_span_start, validate_trace_span_batch, validate_trace_span_rows,
    validate_turn_trace_binding,
};

#[cfg(test)]
mod tests;
