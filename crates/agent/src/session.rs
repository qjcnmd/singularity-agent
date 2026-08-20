//! JSONL 会话管理器。
//!
//! 会话以 append-only 线性序列存储在 JSONL 文件中：每条 entry 有 `id`/`parentId`，
//! 后一条 entry 必须直接引用前一条 entry；不提供 branch/tree 语义。
//!
//! 新格式定义为唯一支持的 `version: 1`，严格校验 Header 与 Entry，拒绝任何未知字段。

use std::collections::{HashMap, HashSet};
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, Weak};

use serde::ser::Error as _;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use singularity_model::{ModelMessage, ModelRole, ModelToolCall, ModelToolParseStatus};
use thiserror::Error;
use time::OffsetDateTime;
use time::macros::format_description;
use uuid::Uuid;

use super::message::{
    AgentMessage, AgentMessageRole, COMPACTION_SUMMARY_PREFIX, COMPACTION_SUMMARY_SUFFIX,
    ContentBlock, LlmMessage,
};

/// 唯一支持的当前会话格式版本。它是一次干净格式重置，不兼容历史 v1-v4 语义。
pub const CURRENT_SESSION_VERSION: u32 = 1;
/// 单条 session JSONL 行（含 header）的字节硬上限。
const MAX_SESSION_LINE_BYTES: usize = 16 * 1024 * 1024;
/// 单次打开 session 文件允许解析的总字节上限（有界读取，超限 fail closed）。
const MAX_SESSION_FILE_BYTES: usize = 512 * 1024 * 1024;
/// 单次打开 session 文件允许解析的条目数上限。
const MAX_SESSION_ENTRIES: usize = 200_000;

#[derive(Debug, Clone, Copy)]
struct AppendLimits {
    line_bytes: usize,
    file_bytes: u64,
    entries: usize,
}

const DEFAULT_APPEND_LIMITS: AppendLimits = AppendLimits {
    line_bytes: MAX_SESSION_LINE_BYTES,
    file_bytes: MAX_SESSION_FILE_BYTES as u64,
    entries: MAX_SESSION_ENTRIES,
};

/// 会话读写错误。
#[derive(Debug, Error)]
pub enum SessionError {
    #[error("session io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("session json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("session header is invalid: {0}")]
    InvalidHeader(String),
    #[error("session line {line} is malformed: {cause}")]
    MalformedLine { line: usize, cause: String },
    #[error("session entry at line {line} is invalid: {cause}")]
    InvalidEntry { line: usize, cause: String },
    #[error("session entry id is duplicated: {0}")]
    DuplicateId(String),
    #[error("session entry {entry_id} refers to missing parent {parent_id}")]
    MissingParent { entry_id: String, parent_id: String },
    #[error("session parent cycle detected at entry {0}")]
    ParentCycle(String),
    #[error("session entry structure is invalid: {0}")]
    InvalidStructure(String),
    #[error("session repair failed: {0}")]
    Repair(String),
    #[error("session append exceeds {kind} limit {limit}; attempted value is {actual}")]
    AppendLimitExceeded {
        kind: &'static str,
        limit: u64,
        actual: u64,
    },
    #[error("{0}")]
    InvalidSession(String),
    #[error("entry {0} not found")]
    EntryNotFound(String),
}

/// 会话操作结果。
pub type Result<T> = std::result::Result<T, SessionError>;

/// session/read 的条目类型过滤。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SessionEntryFilter {
    #[default]
    All,
    Messages,
    Compactions,
}

/// `SessionRepository::read` 的有界读取选项。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionReadOptions {
    /// 返回当前 leaf 路径上的最近 N 条（0 = 只读摘要）。
    pub recent_limit: usize,
    pub filter: SessionEntryFilter,
    /// 在过滤后的路径条目上应用的半开范围（`[start, end)`）。
    pub range: Option<(usize, usize)>,
}

impl Default for SessionReadOptions {
    fn default() -> Self {
        Self {
            recent_limit: 20,
            filter: SessionEntryFilter::All,
            range: None,
        }
    }
}

/// 一次有界会话读取的结果：摘要 + 最近片段，不返回全文。
#[derive(Debug, Clone, PartialEq)]
pub struct SessionRead {
    pub summary: Option<String>,
    pub entries: Vec<SessionEntry>,
    /// 会话文件中的条目总数（不含 header）。
    pub total_entries: usize,
}

/// 按 `~/.singularity/sessions/<session_id>.jsonl` 布局读取会话的仓储入口。
#[derive(Debug, Clone)]
pub struct SessionRepository {
    sessions_dir: PathBuf,
}

impl SessionRepository {
    pub fn new(sessions_dir: impl Into<PathBuf>) -> Self {
        Self {
            sessions_dir: sessions_dir.into(),
        }
    }

    /// 有界读取会话：按 id 定位 rollout，校验 header id，返回摘要 + 最近片段。
    pub fn read(&self, session_id: &str, options: &SessionReadOptions) -> Result<SessionRead> {
        let path = self.sessions_dir.join(format!("{session_id}.jsonl"));
        let session = SessionManager::open_existing(&path)?;
        if session.session_id() != session_id {
            return Err(SessionError::InvalidSession(format!(
                "rollout header id {} does not match requested session id {session_id}",
                session.session_id()
            )));
        }
        Ok(session.read_entries(options))
    }
}

/// compaction 条目 payload。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompactionEntry {
    pub summary: String,
    #[serde(
        rename = "firstKeptEntryId",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub first_kept_entry_id: Option<String>,
    #[serde(
        rename = "tokensBefore",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub tokens_before: Option<u64>,
    #[serde(
        rename = "previousSummary",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub previous_summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

/// JSONL 中不参与模型上下文的持久化 metadata 类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionMetadataKind {
    TurnStarted,
    TurnCompleted,
    TurnFailed,
    TurnInterrupted,
    ThreadSettings,
    Usage,
}

impl SessionMetadataKind {
    pub fn matches_turn_terminal(self) -> bool {
        matches!(
            self,
            Self::TurnCompleted | Self::TurnFailed | Self::TurnInterrupted
        )
    }
}

/// 一条可恢复的 session metadata。
#[derive(Debug, Clone, PartialEq)]
pub struct SessionMetadata {
    kind: SessionMetadataKind,
    fields: Map<String, Value>,
}

impl SessionMetadata {
    /// 构造 metadata；公开字段必须是对象，且 settings 禁止敏感键。
    pub fn new(kind: SessionMetadataKind, fields: Map<String, Value>) -> Result<Self> {
        if fields.keys().any(|key| is_reserved_metadata_key(key)) {
            return Err(SessionError::InvalidStructure(
                "metadata contains a reserved session entry field".to_string(),
            ));
        }
        if matches!(kind, SessionMetadataKind::ThreadSettings)
            && fields.keys().any(|key| is_sensitive_metadata_key(key))
        {
            return Err(SessionError::InvalidStructure(
                "thread settings metadata contains a sensitive field".to_string(),
            ));
        }
        Ok(Self { kind, fields })
    }

    pub fn kind(&self) -> SessionMetadataKind {
        self.kind
    }

    pub fn field(&self, name: &str) -> Option<&Value> {
        self.fields.get(name)
    }

    pub fn field_string(&self, name: &str) -> Option<&str> {
        self.field(name).and_then(Value::as_str)
    }

    pub fn turn_id(&self) -> Option<&str> {
        self.field_string("turnId")
    }

    pub fn synthetic(&self) -> bool {
        self.field("synthetic")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }

    pub fn turn_started(turn_id: impl Into<String>) -> Self {
        Self::simple(SessionMetadataKind::TurnStarted, "turnId", turn_id.into())
    }

    pub fn turn_completed(turn_id: impl Into<String>) -> Self {
        Self::simple(SessionMetadataKind::TurnCompleted, "turnId", turn_id.into())
    }

    pub fn turn_failed(turn_id: impl Into<String>, error: impl Into<String>) -> Self {
        let mut fields = Map::new();
        fields.insert("turnId".to_string(), Value::String(turn_id.into()));
        fields.insert("error".to_string(), Value::String(error.into()));
        Self::unchecked(SessionMetadataKind::TurnFailed, fields)
    }

    pub fn turn_interrupted(
        turn_id: impl Into<String>,
        reason: impl Into<String>,
        synthetic: bool,
    ) -> Self {
        let mut fields = Map::new();
        fields.insert("turnId".to_string(), Value::String(turn_id.into()));
        fields.insert("reason".to_string(), Value::String(reason.into()));
        fields.insert("synthetic".to_string(), Value::Bool(synthetic));
        Self::unchecked(SessionMetadataKind::TurnInterrupted, fields)
    }

    pub fn thread_settings(
        provider: impl Into<String>,
        model: impl Into<String>,
        reasoning: Option<String>,
    ) -> Result<Self> {
        let mut fields = Map::new();
        fields.insert("provider".to_string(), Value::String(provider.into()));
        fields.insert("model".to_string(), Value::String(model.into()));
        if let Some(reasoning) = reasoning {
            fields.insert("reasoning".to_string(), Value::String(reasoning));
        }
        Self::new(SessionMetadataKind::ThreadSettings, fields)
    }

    pub fn usage(turn_id: impl Into<String>, usage: Value) -> Result<Self> {
        if !usage.is_object() {
            return Err(SessionError::InvalidStructure(
                "usage metadata must be a JSON object".to_string(),
            ));
        }
        let mut fields = Map::new();
        fields.insert("turnId".to_string(), Value::String(turn_id.into()));
        fields.insert("usage".to_string(), usage);
        Self::new(SessionMetadataKind::Usage, fields)
    }

    fn simple(kind: SessionMetadataKind, key: &str, value: String) -> Self {
        let mut fields = Map::new();
        fields.insert(key.to_string(), Value::String(value));
        Self::unchecked(kind, fields)
    }

    fn unchecked(kind: SessionMetadataKind, fields: Map<String, Value>) -> Self {
        Self { kind, fields }
    }
}

impl Serialize for SessionMetadata {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut map = self.fields.clone();
        map.insert("metadataType".to_string(), json!(self.kind));
        Value::Object(map).serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for SessionMetadata {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let mut map = Map::<String, Value>::deserialize(deserializer)?;
        let kind_value = map
            .remove("metadataType")
            .ok_or_else(|| serde::de::Error::custom("metadata entry has no metadataType"))?;
        let kind = serde_json::from_value(kind_value)
            .map_err(|error| serde::de::Error::custom(format!("invalid metadataType: {error}")))?;
        if map.keys().any(|key| is_reserved_metadata_key(key)) {
            return Err(serde::de::Error::custom(
                "metadata contains a reserved session entry field",
            ));
        }
        if matches!(kind, SessionMetadataKind::ThreadSettings)
            && map.keys().any(|key| is_sensitive_metadata_key(key))
        {
            return Err(serde::de::Error::custom(
                "thread settings metadata contains a sensitive field",
            ));
        }
        Ok(Self { kind, fields: map })
    }
}

fn is_sensitive_metadata_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    [
        "api_key",
        "apikey",
        "authorization",
        "auth_token",
        "token",
        "secret",
        "password",
    ]
    .iter()
    .any(|sensitive| key.contains(sensitive))
}

fn is_reserved_metadata_key(key: &str) -> bool {
    matches!(
        key,
        "id" | "parentId" | "timestamp" | "type" | "metadataType"
    )
}

fn metadata_payload(value: &Value) -> Value {
    let mut payload = value.as_object().cloned().unwrap_or_default();
    for key in ["id", "parentId", "timestamp", "type"] {
        payload.remove(key);
    }
    Value::Object(payload)
}

/// 会话条目类型。仅保留 Message, Compaction, Metadata。
#[derive(Debug, Clone, PartialEq)]
pub enum SessionEntryType {
    Message(AgentMessage),
    Compaction(CompactionEntry),
    Metadata(SessionMetadata),
}

impl SessionEntryType {
    fn type_name(&self) -> &'static str {
        match self {
            Self::Message(_) => "message",
            Self::Compaction(_) => "compaction",
            Self::Metadata(_) => "metadata",
        }
    }
}

/// 一条会话树 entry。`parent_id` 为空串表示根条目。
#[derive(Debug, Clone, PartialEq)]
pub struct SessionEntry {
    pub id: String,
    pub parent_id: String,
    pub timestamp: Option<String>,
    pub entry_type: SessionEntryType,
}

impl Serialize for SessionEntry {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut map = Map::new();
        map.insert("id".to_string(), json!(self.id));
        map.insert(
            "parentId".to_string(),
            if self.parent_id.is_empty() {
                Value::Null
            } else {
                json!(self.parent_id)
            },
        );
        if let Some(timestamp) = &self.timestamp {
            map.insert("timestamp".to_string(), json!(timestamp));
        }
        map.insert("type".to_string(), json!(self.entry_type.type_name()));
        match &self.entry_type {
            SessionEntryType::Message(message) => {
                map.insert(
                    "message".to_string(),
                    serde_json::to_value(message).map_err(S::Error::custom)?,
                );
            }
            SessionEntryType::Compaction(compaction) => {
                merge_payload(
                    &mut map,
                    serde_json::to_value(compaction).map_err(S::Error::custom)?,
                );
            }
            SessionEntryType::Metadata(metadata) => {
                merge_payload(
                    &mut map,
                    serde_json::to_value(metadata).map_err(S::Error::custom)?,
                );
            }
        }
        Value::Object(map).serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for SessionEntry {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        strict_entry_from_value(&value).map_err(serde::de::Error::custom)
    }
}

fn merge_payload(map: &mut Map<String, Value>, payload: Value) {
    if let Some(object) = payload.as_object() {
        for (key, value) in object {
            map.insert(key.clone(), value.clone());
        }
    }
}

/// 会话上下文的 LLM 消息序列与恢复所需的模型设置。
#[derive(Debug, Clone, PartialEq)]
pub struct SessionContext {
    pub messages: Vec<LlmMessage>,
    pub model: Option<String>,
    pub thinking_level: Option<String>,
}

/// JSONL 会话管理器。
pub struct SessionManager {
    file: PathBuf,
    cwd: PathBuf,
    entries: Vec<SessionEntry>,
    by_id: HashMap<String, usize>,
    leaf_id: Option<String>,
    session_id: String,
    header_timestamp: String,
    append_lock: Arc<Mutex<()>>,
}

impl std::fmt::Debug for SessionManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionManager")
            .field("file", &self.file)
            .field("cwd", &self.cwd)
            .field("session_id", &self.session_id)
            .field("header_timestamp", &self.header_timestamp)
            .field("leaf_id", &self.leaf_id)
            .field("entries_count", &self.entries.len())
            .finish()
    }
}

impl SessionManager {
    /// 新建会话：生成 UUID 并创建文件。
    pub fn create(cwd: &Path, sessions_dir: &Path) -> Result<Self> {
        let session_id = Uuid::now_v7().to_string();
        let timestamp = now_iso();
        Self::create_with_file(
            cwd,
            sessions_dir,
            format!("{session_id}.jsonl"),
            session_id,
            timestamp,
        )
    }

    /// 新建会话：文件名与 header id 都是调用方指定的 UUID。
    pub fn create_with_id(cwd: &Path, sessions_dir: &Path, session_id: &str) -> Result<Self> {
        Uuid::parse_str(session_id).map_err(|_| {
            SessionError::InvalidSession(format!("session id is not a UUID: {session_id}"))
        })?;
        let timestamp = now_iso();
        Self::create_with_file(
            cwd,
            sessions_dir,
            format!("{session_id}.jsonl"),
            session_id.to_string(),
            timestamp,
        )
    }

    /// 打开必须已存在的会话文件；缺失或损坏直接报错，不静默创建新会话。
    pub fn open_existing(path: &Path) -> Result<Self> {
        if !path.is_file() {
            return Err(SessionError::InvalidSession(format!(
                "session file does not exist: {}",
                path.display()
            )));
        }
        Self::open(path)
    }

    /// Open an existing rollout for a bounded discovery/index rebuild.
    ///
    /// This seam validates the complete file while holding the normal append
    /// lock, but it never truncates a torn tail or adds a final newline. A
    /// file that would require the normal reopen repair path is rejected.
    pub fn open_existing_read_only(path: &Path) -> Result<Self> {
        if !path.is_file() {
            return Err(SessionError::InvalidSession(format!(
                "session file does not exist: {}",
                path.display()
            )));
        }
        Self::open_read_only(path)
    }

    /// 共用的新建会话实现：写入 header 并打开新文件。
    fn create_with_file(
        cwd: &Path,
        sessions_dir: &Path,
        file_name: String,
        session_id: String,
        timestamp: String,
    ) -> Result<Self> {
        let cwd = normalize_abs_path(cwd)?;
        std::fs::create_dir_all(sessions_dir)?;
        let file = sessions_dir.join(file_name);
        let header = json!({
            "type": "session",
            "version": CURRENT_SESSION_VERSION,
            "id": session_id,
            "timestamp": timestamp,
            "cwd": normalize_cwd_string(&cwd),
        });
        let mut handle = singularity_core::create_owner_only_file(&file)?;
        writeln!(handle, "{}", serde_json::to_string(&header)?)?;
        handle.flush()?;
        let append_lock = append_lock_for(&file);
        Ok(Self {
            file,
            cwd,
            entries: Vec::new(),
            by_id: HashMap::new(),
            leaf_id: None,
            session_id,
            header_timestamp: timestamp,
            append_lock,
        })
    }

    /// 打开会话文件：严格逐行解析。
    pub fn open(path: &Path) -> Result<Self> {
        let append_lock = append_lock_for(path);
        let _guard = lock_append(&append_lock);
        Self::open_unlocked(path)
    }

    fn open_unlocked(path: &Path) -> Result<Self> {
        let file = path.to_path_buf();
        let metadata = match std::fs::symlink_metadata(&file) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Self::create_empty_at(&file);
            }
            Err(error) => return Err(error.into()),
        };
        if metadata.len() == 0 {
            return Err(SessionError::InvalidSession(format!(
                "Session file is empty and cannot be opened: {}",
                file.display()
            )));
        }
        let parsed = parse_session_lines(&file)?;
        if parsed.entries.is_empty() {
            return Err(SessionError::InvalidSession(format!(
                "Session file is not a valid session: {}",
                file.display()
            )));
        }
        let header = &parsed.entries[0];
        let (session_id, _version, header_cwd, header_timestamp) = validate_header(header)?;
        let entries = validate_entries(&parsed.entries, &parsed.lines)?;
        if !matches!(parsed.repair, TailRepair::None) {
            rewrite_file(&file, &parsed.entries)?;
        }
        let cwd = header_cwd
            .map(|cwd| normalize_abs_path(Path::new(&cwd)))
            .transpose()?
            .unwrap_or(std::env::current_dir()?);
        let mut by_id = HashMap::new();
        let mut leaf_id: Option<String> = None;
        for (index, entry) in entries.iter().enumerate() {
            by_id.insert(entry.id.clone(), index);
            leaf_id = Some(entry.id.clone());
        }
        let append_lock = append_lock_for(&file);
        Ok(Self {
            file,
            cwd,
            entries,
            by_id,
            leaf_id,
            session_id,
            header_timestamp,
            append_lock,
        })
    }

    fn open_read_only(path: &Path) -> Result<Self> {
        let append_lock = append_lock_for(path);
        let _guard = lock_append(&append_lock);
        let file = path.to_path_buf();
        let metadata = std::fs::symlink_metadata(&file)?;
        if metadata.len() == 0 {
            return Err(SessionError::InvalidSession(format!(
                "Session file is empty and cannot be opened: {}",
                file.display()
            )));
        }
        let parsed = parse_session_lines(&file)?;
        if !matches!(parsed.repair, TailRepair::None) {
            return Err(SessionError::InvalidSession(
                "read-only session scan rejected a rollout requiring tail repair".to_string(),
            ));
        }
        let header = parsed
            .entries
            .first()
            .ok_or_else(|| SessionError::InvalidSession("session header is missing".to_string()))?;
        let (session_id, _version, header_cwd, header_timestamp) = validate_header(header)?;
        let entries = validate_entries(&parsed.entries, &parsed.lines)?;
        let cwd = header_cwd
            .map(|cwd| normalize_abs_path(Path::new(&cwd)))
            .transpose()?
            .unwrap_or(std::env::current_dir()?);
        let mut by_id = HashMap::new();
        let mut leaf_id: Option<String> = None;
        for (index, entry) in entries.iter().enumerate() {
            by_id.insert(entry.id.clone(), index);
            leaf_id = Some(entry.id.clone());
        }
        drop(_guard);
        Ok(Self {
            file,
            cwd,
            entries,
            by_id,
            leaf_id,
            session_id,
            header_timestamp,
            append_lock,
        })
    }

    fn create_empty_at(file: &Path) -> Result<Self> {
        let cwd = std::env::current_dir()?;
        let session_id = Uuid::now_v7().to_string();
        let timestamp = now_iso();
        let header = json!({
            "type": "session",
            "version": CURRENT_SESSION_VERSION,
            "id": session_id,
            "timestamp": timestamp,
            "cwd": normalize_cwd_string(&cwd),
        });
        let mut handle = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(file)?;
        writeln!(handle, "{}", serde_json::to_string(&header)?)?;
        handle.flush()?;
        let append_lock = append_lock_for(file);
        Ok(Self {
            file: file.to_path_buf(),
            cwd,
            entries: Vec::new(),
            by_id: HashMap::new(),
            leaf_id: None,
            session_id,
            header_timestamp: timestamp,
            append_lock,
        })
    }

    /// 追加消息为当前 leaf 的子条目并推进 leaf，立即写盘。返回新条目 id。
    pub fn append_message(&mut self, message: AgentMessage) -> Result<String> {
        self.append_entry(SessionEntryType::Message(message))
    }

    /// 追加 compaction 条目为当前 leaf 的子条目并推进 leaf，立即写盘。返回新条目 id。
    pub fn append_compaction(&mut self, entry: CompactionEntry) -> Result<String> {
        self.append_entry(SessionEntryType::Compaction(entry))
    }

    /// 追加不进入模型上下文的 metadata。
    pub fn append_metadata(&mut self, metadata: SessionMetadata) -> Result<String> {
        let metadata = SessionMetadata::new(metadata.kind, metadata.fields)?;
        self.append_entry(SessionEntryType::Metadata(metadata))
    }

    /// 重开时把当前 leaf 上没有终态的 turn 标记为 synthetic interrupted。
    pub fn repair_interrupted_turns(&mut self) -> Result<usize> {
        let path = self.session_path();
        let mut started = HashSet::new();
        let mut terminal = HashSet::new();
        for &entry_index in &path {
            let SessionEntryType::Metadata(metadata) = &self.entries[entry_index].entry_type else {
                continue;
            };
            let Some(turn_id) = metadata.turn_id() else {
                continue;
            };
            match metadata.kind() {
                SessionMetadataKind::TurnStarted => {
                    started.insert(turn_id.to_string());
                }
                SessionMetadataKind::TurnCompleted
                | SessionMetadataKind::TurnFailed
                | SessionMetadataKind::TurnInterrupted => {
                    terminal.insert(turn_id.to_string());
                }
                _ => {}
            }
        }
        let mut repaired = 0;
        for turn_id in started {
            if terminal.contains(&turn_id) {
                continue;
            }
            self.append_metadata(SessionMetadata::turn_interrupted(
                turn_id,
                "session reopened with an incomplete turn",
                true,
            ))?;
            repaired += 1;
        }
        Ok(repaired)
    }

    /// 返回当前 leaf 路径上的 metadata。
    pub fn metadata_entries(&self) -> Vec<SessionMetadata> {
        self.session_path()
            .into_iter()
            .filter_map(|index| match &self.entries[index].entry_type {
                SessionEntryType::Metadata(metadata) => Some(metadata.clone()),
                _ => None,
            })
            .collect()
    }

    fn append_entry(&mut self, entry_type: SessionEntryType) -> Result<String> {
        self.append_entry_with_limits(entry_type, DEFAULT_APPEND_LIMITS)
    }

    fn append_entry_with_limits(
        &mut self,
        entry_type: SessionEntryType,
        limits: AppendLimits,
    ) -> Result<String> {
        let (id, entry) = {
            let append_lock = Arc::clone(&self.append_lock);
            let _guard = lock_append(&append_lock);
            self.refresh_from_disk_locked()?;
            let id = generate_id(|candidate| self.by_id.contains_key(candidate));
            let entry = SessionEntry {
                id: id.clone(),
                parent_id: self.leaf_id.clone().unwrap_or_default(),
                timestamp: Some(now_iso()),
                entry_type,
            };
            let serialized = serde_json::to_string(&entry)?;
            let file_bytes = std::fs::metadata(&self.file)?.len();
            validate_append_limits(file_bytes, self.entries.len(), serialized.len(), limits)?;
            let mut handle = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.file)?;
            handle.write_all(serialized.as_bytes())?;
            handle.write_all(b"\n")?;
            handle.flush()?;
            (id, entry)
        };
        self.by_id.insert(id.clone(), self.entries.len());
        self.entries.push(entry);
        self.leaf_id = Some(id.clone());
        Ok(id)
    }

    fn refresh_from_disk_locked(&mut self) -> Result<()> {
        let refreshed = Self::open_unlocked(&self.file)?;
        if refreshed.session_id != self.session_id {
            return Err(SessionError::InvalidSession(format!(
                "session header id {} does not match current session {}",
                refreshed.session_id, self.session_id
            )));
        }
        self.cwd = refreshed.cwd;
        self.entries = refreshed.entries;
        self.by_id = refreshed.by_id;
        self.leaf_id = refreshed.leaf_id;
        Ok(())
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Header timestamp is the authoritative creation fact for index rebuilds.
    pub fn created_at(&self) -> &str {
        &self.header_timestamp
    }

    pub fn leaf_id(&self) -> &str {
        self.leaf_id.as_deref().unwrap_or("")
    }

    pub fn path(&self) -> &Path {
        &self.file
    }

    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    pub fn entries(&self) -> &[SessionEntry] {
        &self.entries
    }

    /// 构建活跃的、compaction 感知的条目列表。
    pub fn build_context_entries(&self) -> Result<Vec<SessionEntry>> {
        let path = self.session_path();
        let mut compaction_index = None;
        for (index, &entry_index) in path.iter().enumerate() {
            if matches!(
                self.entries[entry_index].entry_type,
                SessionEntryType::Compaction(_)
            ) {
                compaction_index = Some(index);
            }
        }
        let Some(compaction_index) = compaction_index else {
            return Ok(path
                .iter()
                .map(|&index| self.entries[index].clone())
                .collect());
        };
        let compaction = &self.entries[path[compaction_index]];
        let first_kept = match &compaction.entry_type {
            SessionEntryType::Compaction(entry) => entry.first_kept_entry_id.clone(),
            _ => None,
        };
        let mut context = vec![compaction.clone()];
        let mut found_first_kept = false;
        for &entry_index in &path[..compaction_index] {
            let entry = &self.entries[entry_index];
            if Some(entry.id.as_str()) == first_kept.as_deref() {
                found_first_kept = true;
            }
            if found_first_kept {
                context.push(entry.clone());
            }
        }
        context.extend(
            path[compaction_index + 1..]
                .iter()
                .map(|&entry_index| self.entries[entry_index].clone()),
        );
        Ok(context)
    }

    /// 修复活动路径中崩溃遗留的孤立 assistant tool call。
    pub fn repair_orphaned_tool_calls(&mut self) -> Result<usize> {
        let path = self.session_path();
        let mut repaired = 0usize;
        for &entry_index in &path {
            let tool_call_ids: Vec<String> = match &self.entries[entry_index].entry_type {
                SessionEntryType::Message(message)
                    if message.role == AgentMessageRole::Assistant =>
                {
                    message
                        .tool_calls()
                        .into_iter()
                        .filter_map(|block| match block {
                            ContentBlock::ToolCall { id, .. } => Some(id.clone()),
                            _ => None,
                        })
                        .collect()
                }
                _ => Vec::new(),
            };
            for tool_call_id in tool_call_ids {
                let paired = path[path
                    .iter()
                    .position(|&index| index == entry_index)
                    .expect("path index")
                    + 1..]
                    .iter()
                    .any(|&later_index| {
                        matches!(
                            &self.entries[later_index].entry_type,
                            SessionEntryType::Message(message)
                                if message.role == AgentMessageRole::ToolResult
                                    && message.tool_call_id.as_deref()
                                        == Some(tool_call_id.as_str())
                        )
                    });
                if paired {
                    continue;
                }
                self.append_entry(SessionEntryType::Message(AgentMessage {
                    role: AgentMessageRole::ToolResult,
                    content: vec![ContentBlock::Text {
                        text: "[previous execution outcome unknown; do not retry]".to_string(),
                    }],
                    provider_reasoning_replay: None,
                    tool_call_id: Some(tool_call_id),
                    tool_name: None,
                    is_error: Some(true),
                    timestamp: None,
                }))?;
                repaired += 1;
            }
        }
        Ok(repaired)
    }

    pub fn summary(&self) -> Option<String> {
        let path = self.session_path();
        path.iter()
            .rev()
            .find_map(|&index| match &self.entries[index].entry_type {
                SessionEntryType::Compaction(entry) => Some(entry.summary.clone()),
                _ => None,
            })
    }

    pub fn total_entries(&self) -> usize {
        self.entries.len()
    }

    pub fn recent_entries(&self, limit: usize) -> Vec<SessionEntry> {
        let path = self.session_path();
        let start = path.len().saturating_sub(limit);
        path[start..]
            .iter()
            .map(|&index| self.entries[index].clone())
            .collect()
    }

    pub fn read_entries(&self, options: &SessionReadOptions) -> SessionRead {
        let path = self.session_path();
        let filtered = path
            .iter()
            .filter(|&&index| match options.filter {
                SessionEntryFilter::All => true,
                SessionEntryFilter::Messages => {
                    matches!(self.entries[index].entry_type, SessionEntryType::Message(_))
                }
                SessionEntryFilter::Compactions => {
                    matches!(
                        self.entries[index].entry_type,
                        SessionEntryType::Compaction(_)
                    )
                }
            })
            .copied()
            .collect::<Vec<_>>();
        let (start, end) = options.range.unwrap_or((0, filtered.len()));
        let start = start.min(filtered.len());
        let end = end.min(filtered.len());
        let selected = if start >= end {
            &filtered[..0]
        } else {
            &filtered[start..end]
        };
        let start = selected.len().saturating_sub(options.recent_limit);
        SessionRead {
            summary: self.summary(),
            entries: selected[start..]
                .iter()
                .map(|&index| self.entries[index].clone())
                .collect(),
            total_entries: self.entries.len(),
        }
    }

    /// 构建发送给 LLM 的会话上下文。
    pub fn build_session_context(&self) -> Result<SessionContext> {
        let mut model = None;
        for entry_index in self.session_path() {
            if let SessionEntryType::Metadata(metadata) = &self.entries[entry_index].entry_type
                && metadata.kind() == SessionMetadataKind::ThreadSettings
                && let (Some(provider), Some(model_id)) = (
                    metadata.field_string("provider"),
                    metadata.field_string("model"),
                )
            {
                model = Some(if provider.is_empty() {
                    model_id.to_string()
                } else {
                    format!("{provider}/{model_id}")
                });
            }
        }
        let messages = self
            .build_context_entries()?
            .iter()
            .flat_map(entry_to_llm_messages)
            .collect();
        Ok(SessionContext {
            messages,
            model,
            thinking_level: None,
        })
    }

    fn session_path(&self) -> Vec<usize> {
        let Some(leaf_id) = &self.leaf_id else {
            return Vec::new();
        };
        let current = self
            .by_id
            .get(leaf_id)
            .copied()
            .or_else(|| self.entries.len().checked_sub(1));
        let Some(mut current) = current else {
            return Vec::new();
        };
        let mut path = Vec::new();
        let mut visited = HashSet::new();
        loop {
            if !visited.insert(current) {
                break;
            }
            path.push(current);
            let parent = &self.entries[current].parent_id;
            current = match self.by_id.get(parent) {
                Some(&next) => next,
                None => break,
            };
        }
        path.reverse();
        path
    }
}

fn entry_to_llm_messages(entry: &SessionEntry) -> Vec<LlmMessage> {
    match &entry.entry_type {
        SessionEntryType::Message(message) => match message.role {
            AgentMessageRole::User => {
                vec![ModelMessage::text(ModelRole::User, message.content_text())]
            }
            AgentMessageRole::Assistant => {
                let tool_calls = message
                    .tool_calls()
                    .into_iter()
                    .filter_map(|block| match block {
                        ContentBlock::ToolCall { id, name, args } => {
                            if id.trim().is_empty() || name.trim().is_empty() {
                                return None;
                            }
                            Some(ModelToolCall {
                                tool_call_id: id.clone(),
                                tool_name: name.clone(),
                                arguments: args.clone(),
                                raw_arguments: serde_json::to_string(args).unwrap_or_default(),
                                parse_status: ModelToolParseStatus::Valid,
                                validation_errors: Vec::new(),
                            })
                        }
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                if tool_calls.is_empty() {
                    return vec![ModelMessage::text(
                        ModelRole::Assistant,
                        message.content_text(),
                    )];
                }
                let mut llm = ModelMessage::assistant_tool_calls(tool_calls);
                llm.content = message.content_text();
                vec![llm]
            }
            AgentMessageRole::ToolResult => {
                let mut llm = ModelMessage::text(ModelRole::Tool, message.content_text());
                llm.tool_call_id = message.tool_call_id.clone();
                vec![llm]
            }
        },
        SessionEntryType::Compaction(compaction) => vec![ModelMessage::text(
            ModelRole::User,
            format!(
                "{COMPACTION_SUMMARY_PREFIX}{}{COMPACTION_SUMMARY_SUFFIX}",
                compaction.summary
            ),
        )],
        SessionEntryType::Metadata(_) => Vec::new(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TailRepair {
    None,
    RemoveTornTail,
    AddFinalNewline,
}

struct ParsedSessionLines {
    entries: Vec<Value>,
    lines: Vec<usize>,
    repair: TailRepair,
}

fn validate_append_limits(
    current_file_bytes: u64,
    current_entries: usize,
    serialized_line_bytes: usize,
    limits: AppendLimits,
) -> Result<()> {
    if serialized_line_bytes > limits.line_bytes {
        return Err(SessionError::AppendLimitExceeded {
            kind: "line bytes",
            limit: limits.line_bytes as u64,
            actual: serialized_line_bytes as u64,
        });
    }
    let attempted_file_bytes = current_file_bytes
        .checked_add(serialized_line_bytes as u64)
        .and_then(|bytes| bytes.checked_add(1))
        .ok_or(SessionError::AppendLimitExceeded {
            kind: "file bytes",
            limit: limits.file_bytes,
            actual: u64::MAX,
        })?;
    if attempted_file_bytes > limits.file_bytes {
        return Err(SessionError::AppendLimitExceeded {
            kind: "file bytes",
            limit: limits.file_bytes,
            actual: attempted_file_bytes,
        });
    }
    let attempted_entries =
        current_entries
            .checked_add(1)
            .ok_or(SessionError::AppendLimitExceeded {
                kind: "entry count",
                limit: limits.entries as u64,
                actual: u64::MAX,
            })?;
    if attempted_entries > limits.entries {
        return Err(SessionError::AppendLimitExceeded {
            kind: "entry count",
            limit: limits.entries as u64,
            actual: attempted_entries as u64,
        });
    }
    Ok(())
}

fn parse_session_lines(file: &Path) -> Result<ParsedSessionLines> {
    parse_session_lines_with_limits(
        file,
        MAX_SESSION_FILE_BYTES,
        MAX_SESSION_LINE_BYTES,
        MAX_SESSION_ENTRIES,
    )
}

fn parse_session_lines_with_limits(
    file: &Path,
    max_file_bytes: usize,
    max_line_bytes: usize,
    max_content_entries: usize,
) -> Result<ParsedSessionLines> {
    let metadata = std::fs::metadata(file)?;
    if metadata.len() > max_file_bytes as u64 {
        return Err(SessionError::InvalidSession(format!(
            "session file exceeds bounded parse limits ({} bytes / {max_content_entries} entries)",
            max_file_bytes
        )));
    }

    let handle = std::fs::File::open(file)?;
    let mut reader = BufReader::new(handle);
    let mut entries = Vec::new();
    let mut lines = Vec::new();
    let mut repair = TailRepair::None;
    let mut line_number = 1usize;
    while let Some(bounded_line) = read_bounded_session_line(&mut reader, max_line_bytes)? {
        if bounded_line.too_long {
            return Err(SessionError::InvalidSession(format!(
                "session entry exceeds {max_line_bytes} bytes at line {line_number}"
            )));
        }
        let has_newline = bounded_line.has_newline;
        let mut line = bounded_line.bytes.as_slice();
        if line.ends_with(b"\r") {
            line = &line[..line.len() - 1];
        }
        if line.len() > max_line_bytes {
            return Err(SessionError::InvalidSession(format!(
                "session entry exceeds {max_line_bytes} bytes at line {line_number}"
            )));
        }
        if line.iter().all(u8::is_ascii_whitespace) {
            if !has_newline {
                repair = TailRepair::AddFinalNewline;
                break;
            }
            line_number += 1;
            continue;
        }

        let text = match std::str::from_utf8(line) {
            Ok(text) => text,
            Err(error) if !has_newline && error.error_len().is_none() => {
                repair = TailRepair::RemoveTornTail;
                break;
            }
            Err(error) => {
                return Err(SessionError::MalformedLine {
                    line: line_number,
                    cause: format!("invalid UTF-8: {error}"),
                });
            }
        };
        let value = match serde_json::from_str::<Value>(text) {
            Ok(value) => value,
            Err(error) if !has_newline && error.is_eof() => {
                repair = TailRepair::RemoveTornTail;
                break;
            }
            Err(error) => {
                return Err(SessionError::MalformedLine {
                    line: line_number,
                    cause: error.to_string(),
                });
            }
        };
        if !value.is_object() {
            return Err(SessionError::InvalidEntry {
                line: line_number,
                cause: "session entry is not a JSON object".to_string(),
            });
        }
        let content_entries = entries.len().saturating_sub(1);
        if content_entries >= max_content_entries {
            return Err(SessionError::InvalidSession(format!(
                "session file exceeds bounded parse limits ({} bytes / {max_content_entries} entries)",
                max_file_bytes
            )));
        }
        entries.push(value);
        lines.push(line_number);
        if !has_newline {
            repair = TailRepair::AddFinalNewline;
            break;
        }
        line_number += 1;
    }
    Ok(ParsedSessionLines {
        entries,
        lines,
        repair,
    })
}

struct BoundedSessionLine {
    bytes: Vec<u8>,
    has_newline: bool,
    too_long: bool,
}

fn read_bounded_session_line<R: BufRead>(
    reader: &mut R,
    limit: usize,
) -> std::io::Result<Option<BoundedSessionLine>> {
    let mut bytes = Vec::with_capacity(limit.min(4096) + 1);
    loop {
        let buffer = reader.fill_buf()?;
        if buffer.is_empty() {
            if bytes.is_empty() {
                return Ok(None);
            }
            return Ok(Some(BoundedSessionLine {
                bytes,
                has_newline: false,
                too_long: false,
            }));
        }
        let newline = buffer.iter().position(|byte| *byte == b'\n');
        let content_len = newline.unwrap_or(buffer.len());
        if bytes.len().saturating_add(content_len) > limit.saturating_add(1) {
            return Ok(Some(BoundedSessionLine {
                bytes: Vec::new(),
                has_newline: newline.is_some(),
                too_long: true,
            }));
        }
        bytes.extend_from_slice(&buffer[..content_len]);
        let consumed = newline.map_or(content_len, |position| position + 1);
        reader.consume(consumed);
        if newline.is_some() {
            return Ok(Some(BoundedSessionLine {
                bytes,
                has_newline: true,
                too_long: false,
            }));
        }
    }
}

fn validate_header(value: &Value) -> Result<(String, u32, Option<String>, String)> {
    let object = value
        .as_object()
        .ok_or_else(|| SessionError::InvalidHeader("header is not a JSON object".into()))?;
    if object.get("type").and_then(Value::as_str) != Some("session") {
        return Err(SessionError::InvalidHeader(
            "first entry is not a session header".into(),
        ));
    }
    for key in object.keys() {
        if !matches!(
            key.as_str(),
            "type" | "version" | "id" | "timestamp" | "cwd"
        ) {
            return Err(SessionError::InvalidHeader(format!(
                "unknown header field: {key}"
            )));
        }
    }
    let session_id = object
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.trim().is_empty())
        .ok_or_else(|| SessionError::InvalidHeader("header id must be a non-empty string".into()))?
        .to_string();
    Uuid::parse_str(&session_id).map_err(|_| {
        SessionError::InvalidHeader(format!("header id must be a valid UUID: {session_id}"))
    })?;
    let version = object
        .get("version")
        .and_then(Value::as_u64)
        .and_then(|version| u32::try_from(version).ok())
        .filter(|version| *version == CURRENT_SESSION_VERSION)
        .ok_or_else(|| {
            SessionError::InvalidHeader(format!(
                "header version must be exactly {CURRENT_SESSION_VERSION}"
            ))
        })?;
    let cwd = match object.get("cwd") {
        Some(Value::String(cwd)) if !cwd.trim().is_empty() => Some(cwd.clone()),
        Some(Value::String(cwd)) => Some(cwd.clone()),
        Some(_) => {
            return Err(SessionError::InvalidHeader(
                "header cwd must be a string".into(),
            ));
        }
        None => return Err(SessionError::InvalidHeader("header cwd is required".into())),
    };
    let timestamp = object
        .get("timestamp")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| SessionError::InvalidHeader("header timestamp is required".into()))?
        .to_string();
    Ok((session_id, version, cwd, timestamp))
}

fn validate_entries(raw_entries: &[Value], lines: &[usize]) -> Result<Vec<SessionEntry>> {
    let mut entries = Vec::with_capacity(raw_entries.len().saturating_sub(1));
    let mut ids = HashSet::new();
    for (index, raw) in raw_entries.iter().enumerate().skip(1) {
        let line = lines.get(index).copied().unwrap_or(index + 1);
        if raw.get("type").and_then(Value::as_str) == Some("session") {
            return Err(SessionError::InvalidStructure(format!(
                "intermediate session header at line {line}"
            )));
        }
        let entry = strict_entry_from_value(raw)
            .map_err(|cause| SessionError::InvalidEntry { line, cause })?;
        if !ids.insert(entry.id.clone()) {
            return Err(SessionError::DuplicateId(entry.id));
        }
        entries.push(entry);
    }

    let parent_by_id: HashMap<String, String> = entries
        .iter()
        .map(|entry| (entry.id.clone(), entry.parent_id.clone()))
        .collect();
    for entry in &entries {
        if !entry.parent_id.is_empty() && !parent_by_id.contains_key(&entry.parent_id) {
            return Err(SessionError::MissingParent {
                entry_id: entry.id.clone(),
                parent_id: entry.parent_id.clone(),
            });
        }
    }
    let mut resolved = HashSet::new();
    for entry in &entries {
        if resolved.contains(&entry.id) {
            continue;
        }
        let mut cursor = entry.id.clone();
        let mut path = Vec::new();
        let mut path_set = HashSet::new();
        loop {
            if resolved.contains(&cursor) {
                break;
            }
            if !path_set.insert(cursor.clone()) {
                return Err(SessionError::ParentCycle(cursor));
            }
            path.push(cursor.clone());
            let parent = parent_by_id
                .get(&cursor)
                .expect("entry ids were collected before parent traversal");
            if parent.is_empty() {
                break;
            }
            cursor = parent.clone();
        }
        resolved.extend(path);
    }
    // v1 is deliberately linear. The first content entry is the only root;
    // every later entry must point at the immediately preceding durable entry.
    for (index, entry) in entries.iter().enumerate() {
        let expected_parent = index
            .checked_sub(1)
            .and_then(|previous| entries.get(previous))
            .map(|previous| previous.id.as_str())
            .unwrap_or("");
        if entry.parent_id != expected_parent {
            return Err(SessionError::InvalidStructure(format!(
                "session rollout is not linear at entry {}: expected parent {:?}, got {:?}",
                entry.id, expected_parent, entry.parent_id
            )));
        }
    }
    Ok(entries)
}

fn strict_entry_from_value(value: &Value) -> std::result::Result<SessionEntry, String> {
    let object = value
        .as_object()
        .ok_or_else(|| "session entry is not a JSON object".to_string())?;
    for key in object.keys() {
        if !matches!(
            key.as_str(),
            "id" | "parentId"
                | "timestamp"
                | "type"
                | "message"
                | "summary"
                | "firstKeptEntryId"
                | "tokensBefore"
                | "previousSummary"
                | "details"
                | "metadataType"
                | "turnId"
                | "error"
                | "reason"
                | "synthetic"
                | "provider"
                | "model"
                | "reasoning"
                | "usage"
        ) {
            return Err(format!("unknown session entry field: {key}"));
        }
    }
    let id = object
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.trim().is_empty())
        .ok_or_else(|| "session entry id must be a non-empty string".to_string())?
        .to_string();
    let parent_id = match object.get("parentId") {
        Some(Value::Null) => String::new(),
        Some(Value::String(parent)) => parent.clone(),
        Some(_) => return Err("session entry parentId is not a string".into()),
        None => return Err("session entry parentId is required".into()),
    };
    let timestamp = object
        .get("timestamp")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string)
        .ok_or_else(|| "session entry timestamp is required".to_string())?;
    let entry_type = match object.get("type").and_then(Value::as_str) {
        Some("message") => {
            let message = value
                .get("message")
                .ok_or_else(|| "message entry has no message payload".to_string())?;
            SessionEntryType::Message(
                serde_json::from_value(message.clone())
                    .map_err(|error| format!("invalid message payload: {error}"))?,
            )
        }
        Some("compaction") => SessionEntryType::Compaction(
            serde_json::from_value(value.clone())
                .map_err(|error| format!("invalid compaction payload: {error}"))?,
        ),
        Some("metadata") => SessionEntryType::Metadata(
            serde_json::from_value(metadata_payload(value))
                .map_err(|error| format!("invalid metadata payload: {error}"))?,
        ),
        Some(other) => return Err(format!("unknown session entry type: {other}")),
        None => return Err("session entry has no type".into()),
    };
    Ok(SessionEntry {
        id,
        parent_id,
        timestamp: Some(timestamp),
        entry_type,
    })
}

fn append_lock_for(path: &Path) -> Arc<Mutex<()>> {
    static LOCKS: OnceLock<Mutex<HashMap<PathBuf, Weak<Mutex<()>>>>> = OnceLock::new();
    let locks = LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut locks = locks
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(lock) = locks.get(path).and_then(Weak::upgrade) {
        return lock;
    }
    let lock = Arc::new(Mutex::new(()));
    locks.insert(path.to_path_buf(), Arc::downgrade(&lock));
    lock
}

fn lock_append(lock: &Mutex<()>) -> MutexGuard<'_, ()> {
    match lock.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn rewrite_file(file: &Path, entries: &[Value]) -> Result<()> {
    let serialized: Vec<String> = entries
        .iter()
        .map(serde_json::to_string)
        .collect::<std::result::Result<_, _>>()?;
    let parent = file.parent().unwrap_or_else(|| Path::new("."));
    let name = file
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("session.jsonl");
    let temporary = parent.join(format!(".{name}.tmp-{}", Uuid::new_v4().simple()));
    let mut handle = singularity_core::create_owner_only_file(&temporary).map_err(|error| {
        SessionError::Repair(format!("could not create temporary session file: {error}"))
    })?;
    let write_result = (|| -> std::io::Result<()> {
        for line in &serialized {
            handle.write_all(line.as_bytes())?;
            handle.write_all(b"\n")?;
        }
        handle.flush()?;
        handle.sync_all()?;
        Ok(())
    })();
    drop(handle);
    if let Err(error) = write_result {
        let _ = std::fs::remove_file(&temporary);
        return Err(SessionError::Repair(format!(
            "could not write temporary session file: {error}"
        )));
    }
    if let Err(error) = atomic_replace_file(&temporary, file) {
        let _ = std::fs::remove_file(&temporary);
        return Err(SessionError::Repair(format!(
            "could not atomically replace session file: {error}"
        )));
    }
    Ok(())
}

#[cfg_attr(windows, allow(unsafe_code))]
fn atomic_replace_file(from: &Path, to: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::{
            MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
        };
        let mut from_wide = from.as_os_str().encode_wide().collect::<Vec<_>>();
        from_wide.push(0);
        let mut to_wide = to.as_os_str().encode_wide().collect::<Vec<_>>();
        to_wide.push(0);
        if unsafe {
            MoveFileExW(
                from_wide.as_ptr(),
                to_wide.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        std::fs::rename(from, to)
    }
}

fn generate_id(occupied: impl Fn(&str) -> bool) -> String {
    for _ in 0..100 {
        let id: String = Uuid::new_v4()
            .simple()
            .to_string()
            .chars()
            .take(8)
            .collect();
        if !occupied(&id) {
            return id;
        }
    }
    Uuid::new_v4().to_string()
}

fn now_iso() -> String {
    OffsetDateTime::now_utc()
        .format(&format_description!(
            "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z"
        ))
        .expect("utc timestamp always formats")
}

fn normalize_abs_path(path: &Path) -> Result<PathBuf> {
    Ok(std::path::absolute(path)?)
}

fn normalize_cwd_string(cwd: &Path) -> String {
    cwd.to_string_lossy().replace('\\', "/")
}

#[cfg(test)]
#[path = "session_tests.rs"]
mod tests;
