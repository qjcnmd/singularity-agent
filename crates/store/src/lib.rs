#![deny(unsafe_code)]

//! 轻量会话索引：SQLite 只保存 `session_index` 元数据，JSONL rollout 是唯一权威正文。

use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::path::{Component, Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::ambient_authority;
use cap_std::fs::{Dir as CapabilityDir, OpenOptions as CapabilityOpenOptions};
use rusqlite::{Connection, OpenFlags, params};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

const SCHEMA_VERSION: u32 = 1;
const STORE_INITIALIZATION_LOCK_RETRY_MS: u64 = 10;
const SQLITE_BUSY_TIMEOUT_MS: u64 = 5_000;
const SQLITE_FOREIGN_KEYS_PRAGMA: &str = "foreign_keys";
const SQLITE_JOURNAL_MODE_PRAGMA: &str = "journal_mode";
const SQLITE_JOURNAL_MODE_WAL: &str = "WAL";
const SQLITE_SECURE_DELETE_PRAGMA: &str = "secure_delete";

mod connection;
mod error;
mod file_identity;
mod migration;
mod owner_only;
mod session_index;

pub use connection::{SessionStore, SessionStoreDescriptor, quarantine_corrupted_store_files};
pub use error::{StoreError, StoreResult};
pub use owner_only::{ensure_owner_only_dir, ensure_owner_only_file};
pub use session_index::{SessionMetadataUpdate, SessionRecord, SessionStatus, now_iso};

pub(crate) use file_identity::StoreIdentityGuard;

#[cfg(test)]
mod tests;
