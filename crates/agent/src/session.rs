//! Pi 式 JSONL 会话管理器（语义基线：`@earendil-works/pi-coding-agent` v0.84.1 的
//! `dist/core/session-manager.js`、`dist/core/agent-session.js`、`docs/session-format.md`）。
//!
//! 会话以 append-only 树存储在 JSONL 文件中：每条 entry 有 `id`/`parentId` 形成树，
//! `leaf` 指针跟踪当前位置；分支只移动 leaf，不改写历史。
//!
//! 与 Pi 源码的已知差异（Phase 2a 简化，见主代理确认）：
//! - 条目 JSON 判别字段跟随 Pi 用 `"type"`（规格草案中的 `entryType` 为笔误）。
//! - `SessionEntryType` 在契约三个变体之外增加 `ModelChange`/`ThinkingLevelChange`
//!   （build_session_context 提取 model/thinking 所需）与 `Other(Value)`（未建模条目
//!   原样保留，迁移重写不丢数据）。
//! - 消息 `content` 使用 `Vec<ContentBlock>`，并在 assistant 条目中持久化
//!   provider-private continuation state（例如 Responses opaque output items）；
//!   该私有状态原样重放到 provider boundary，不进入公开 Debug 文本。
//! - `append_message` 立即写盘（Pi 延迟到首个 assistant 消息才建文件，规格明确要求
//!   立即写盘）。
//! - 路径遍历带防环保护（Pi 对损坏的 parentId 环会死循环）。

use std::collections::{HashMap, HashSet};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, Weak};

use serde::de::DeserializeOwned;
use serde::ser::Error as _;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use singularity_model::{ModelMessage, ModelRole, ModelToolCall, ModelToolParseStatus};
use thiserror::Error;
use time::OffsetDateTime;
use time::macros::format_description;
use uuid::Uuid;

use super::message::{
    AgentMessage, AgentMessageRole, BRANCH_SUMMARY_PREFIX, BRANCH_SUMMARY_SUFFIX,
    COMPACTION_SUMMARY_PREFIX, COMPACTION_SUMMARY_SUFFIX, ContentBlock, LlmMessage,
};

/// 当前会话文件格式版本，对齐 Pi `CURRENT_SESSION_VERSION`（v4：消息 content
/// 内容块化，见 `message.rs`）。
pub const CURRENT_SESSION_VERSION: u32 = 4;
/// 单条 session JSONL 行（含 header）的字节硬上限。
const MAX_SESSION_LINE_BYTES: usize = 16 * 1024 * 1024;
/// 单次打开 session 文件允许解析的总字节上限（有界读取，超限 fail closed）。
const MAX_SESSION_FILE_BYTES: usize = 512 * 1024 * 1024;
/// 单次打开 session 文件允许解析的条目数上限。
const MAX_SESSION_ENTRIES: usize = 200_000;

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

/// compaction 条目 payload，字段对齐 Pi `CompactionEntry`。
///
/// `previous_summary` 是 Singularity 扩展字段（Pi 0.84.1 无此字段），
/// 序列化为 `previousSummary`，仅在 Some 时输出。
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

/// custom 条目 payload（扩展状态，不参与 LLM 上下文），对齐 Pi `CustomEntry`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CustomEntry {
    #[serde(rename = "customType")]
    pub custom_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// JSONL 中不参与模型上下文的持久化 metadata 类型。
///
/// metadata 只描述可恢复的产品事实；具体字段由 `SessionMetadata` 的受限构造器
/// 写入，解析时保留未知字段以便新旧版本之间安全重开。它不是第二个事件存储，
/// 而是同一条 session JSONL 树上的非消息 entry。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionMetadataKind {
    TurnStarted,
    TurnCompleted,
    TurnFailed,
    TurnInterrupted,
    ThreadSettings,
    Usage,
    Item,
}

impl SessionMetadataKind {
    pub fn matches_turn_terminal(self) -> bool {
        matches!(
            self,
            Self::TurnCompleted | Self::TurnFailed | Self::TurnInterrupted
        )
    }
}

/// 一条可恢复的 session metadata。`fields` 采用 JSON 对象以允许未来增加非敏感
/// 字段，同时所有当前写入路径都通过下面的类型化构造器，避免把认证信息落盘。
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

    pub fn item(
        turn_id: impl Into<String>,
        item_id: impl Into<String>,
        item_type: impl Into<String>,
        status: impl Into<String>,
        payload: Option<Value>,
    ) -> Result<Self> {
        let mut fields = Map::new();
        fields.insert("turnId".to_string(), Value::String(turn_id.into()));
        fields.insert("itemId".to_string(), Value::String(item_id.into()));
        fields.insert("itemType".to_string(), Value::String(item_type.into()));
        fields.insert("status".to_string(), Value::String(status.into()));
        if let Some(payload) = payload {
            fields.insert("payload".to_string(), payload);
        }
        Self::new(SessionMetadataKind::Item, fields)
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

/// Strip the JSONL envelope before decoding a metadata payload. Keeping this
/// boundary explicit prevents `id`/`parentId`/`type` from becoming metadata
/// fields when a session is reopened, while still preserving unknown payload
/// fields for forward-compatible reads.
fn metadata_payload(value: &Value) -> Value {
    let mut payload = value.as_object().cloned().unwrap_or_default();
    for key in ["id", "parentId", "timestamp", "type"] {
        payload.remove(key);
    }
    Value::Object(payload)
}

/// 会话条目类型。`Other` 保留未建模条目（label/session_info/custom_message/
/// branch_summary 及未知类型）的原始 JSON，保证迁移重写不丢数据。
#[derive(Debug, Clone, PartialEq)]
pub enum SessionEntryType {
    Message(AgentMessage),
    Compaction(CompactionEntry),
    Custom(CustomEntry),
    Metadata(SessionMetadata),
    ModelChange { provider: String, model_id: String },
    ThinkingLevelChange { thinking_level: String },
    Other(Value),
}

impl SessionEntryType {
    fn type_name(&self) -> &'static str {
        match self {
            Self::Message(_) => "message",
            Self::Compaction(_) => "compaction",
            Self::Custom(_) => "custom",
            Self::Metadata(_) => "metadata",
            Self::ModelChange { .. } => "model_change",
            Self::ThinkingLevelChange { .. } => "thinking_level_change",
            Self::Other(_) => unreachable!("Other 条目保留原始 type，不调用 type_name"),
        }
    }
}

/// 一条会话树 entry。`parent_id` 为空串表示根条目（Pi 的 `parentId: null`）。
#[derive(Debug, Clone, PartialEq)]
pub struct SessionEntry {
    pub id: String,
    pub parent_id: String,
    /// Pi 条目级 ISO8601 时间戳（`"2025-01-15T10:30:00.000Z"`）。
    pub timestamp: Option<String>,
    pub entry_type: SessionEntryType,
}

impl SessionEntry {
    fn from_value(value: &Value) -> std::result::Result<Self, String> {
        let object = value
            .as_object()
            .ok_or("session entry is not a json object")?;
        let id = object
            .get("id")
            .and_then(Value::as_str)
            .ok_or("session entry has no id")?;
        let parent_id = match object.get("parentId") {
            None | Some(Value::Null) => String::new(),
            Some(Value::String(parent)) => parent.clone(),
            Some(_) => return Err("session entry parentId is not a string".into()),
        };
        let timestamp = object
            .get("timestamp")
            .and_then(Value::as_str)
            .map(str::to_string);
        let entry_type = match object.get("type").and_then(Value::as_str) {
            // 消息 payload 嵌套在 "message" 键下；不能把整个条目传给 AgentMessage
            // 反序列化（条目级 "timestamp" 是 ISO 字符串，会与消息级 u64 冲突）。
            Some("message") => value.get("message").map_or_else(
                || SessionEntryType::Other(value.clone()),
                |message| typed_or_other(message, SessionEntryType::Message),
            ),
            Some("compaction") => typed_or_other(value, SessionEntryType::Compaction),
            Some("custom") => typed_or_other(value, SessionEntryType::Custom),
            Some("metadata") => {
                let payload = metadata_payload(value);
                typed_or_other(&payload, SessionEntryType::Metadata)
            }
            Some("model_change") => typed_or_other(value, |wire: ModelChangeWire| {
                SessionEntryType::ModelChange {
                    provider: wire.provider,
                    model_id: wire.model_id,
                }
            }),
            Some("thinking_level_change") => {
                typed_or_other(value, |wire: ThinkingLevelChangeWire| {
                    SessionEntryType::ThinkingLevelChange {
                        thinking_level: wire.thinking_level,
                    }
                })
            }
            _ => SessionEntryType::Other(value.clone()),
        };
        Ok(Self {
            id: id.to_string(),
            parent_id,
            timestamp,
            entry_type,
        })
    }
}

/// `SessionEntry` 的通用反序列化保留未知/旧 payload；严格 reopen 路径使用
/// `strict_entry_from_value`，不会把已知 schema 错误降级为 `Other`。
fn typed_or_other<T: DeserializeOwned>(
    value: &Value,
    build: impl FnOnce(T) -> SessionEntryType,
) -> SessionEntryType {
    match serde_json::from_value::<T>(value.clone()) {
        Ok(parsed) => build(parsed),
        Err(_) => SessionEntryType::Other(value.clone()),
    }
}

#[derive(Deserialize)]
struct ModelChangeWire {
    provider: String,
    #[serde(rename = "modelId")]
    model_id: String,
}

#[derive(Deserialize)]
struct ThinkingLevelChangeWire {
    #[serde(rename = "thinkingLevel")]
    thinking_level: String,
}

impl Serialize for SessionEntry {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut map = Map::new();
        // `Other` 携带完整原始对象：先整体铺入，再覆盖公共字段保证与结构体一致。
        if let SessionEntryType::Other(raw) = &self.entry_type
            && let Some(object) = raw.as_object()
        {
            for (key, value) in object {
                map.insert(key.clone(), value.clone());
            }
        }
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
        // Other 条目原样保留原始 type；其余类型统一写判别字段。
        if !matches!(self.entry_type, SessionEntryType::Other(_)) {
            map.insert("type".to_string(), json!(self.entry_type.type_name()));
        }
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
            SessionEntryType::Custom(custom) => {
                merge_payload(
                    &mut map,
                    serde_json::to_value(custom).map_err(S::Error::custom)?,
                );
            }
            SessionEntryType::Metadata(metadata) => {
                merge_payload(
                    &mut map,
                    serde_json::to_value(metadata).map_err(S::Error::custom)?,
                );
            }
            SessionEntryType::ModelChange { provider, model_id } => {
                map.insert("provider".to_string(), json!(provider));
                map.insert("modelId".to_string(), json!(model_id));
            }
            SessionEntryType::ThinkingLevelChange { thinking_level } => {
                map.insert("thinkingLevel".to_string(), json!(thinking_level));
            }
            SessionEntryType::Other(_) => {}
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
        SessionEntry::from_value(&value).map_err(serde::de::Error::custom)
    }
}

fn merge_payload(map: &mut Map<String, Value>, payload: Value) {
    if let Some(object) = payload.as_object() {
        for (key, value) in object {
            map.insert(key.clone(), value.clone());
        }
    }
}

/// 会话上下文的 LLM 消息序列与恢复所需的模型/思考档位设置。
///
/// `model` 使用仓库既有选择器约定 `"provider/modelId"`（同 `model_preferences.model_name`）。
/// `thinking_level` 仅在路径中存在 `thinking_level_change` 条目时为 Some。
#[derive(Debug, Clone, PartialEq)]
pub struct SessionContext {
    pub messages: Vec<LlmMessage>,
    pub model: Option<String>,
    pub thinking_level: Option<String>,
}

/// Pi 式 JSONL 会话管理器（见模块文档）。
pub struct SessionManager {
    file: PathBuf,
    /// 会话 cwd（header 字段来源；Phase 2d 起由 agent loop 提供给工具执行）。
    cwd: PathBuf,
    entries: Vec<SessionEntry>,
    by_id: HashMap<String, usize>,
    leaf_id: Option<String>,
    /// `branch()` deliberately selects an in-memory historical leaf.  A normal
    /// append follows the latest durable leaf so separate long-lived managers
    /// cannot fork a session; the pin lasts for that one explicit branch append.
    leaf_pinned: bool,
    /// 会话 id（header 字段与文件名来源）。Phase 2a 无读取方，契约保留。
    #[allow(dead_code)]
    session_id: String,
    /// 同一 session 的 append 必须串行化；锁只保护 durable append 窗口。
    append_lock: Arc<Mutex<()>>,
}

impl SessionManager {
    /// 新建会话：生成 Pi 风格文件名 `<ts>_<uuid>.jsonl` 并立即写入 header。
    ///
    /// header：`{"type":"session","version":4,"id":...,"timestamp":...,"cwd":...}`，
    /// 无父会话时省略 `parentSession`。`cwd` 取绝对路径（无需已存在）。
    pub fn create(cwd: &Path, sessions_dir: &Path) -> Result<Self> {
        let session_id = Uuid::now_v7().to_string();
        let timestamp = now_iso();
        let file_timestamp = timestamp.replace([':', '.'], "-");
        Self::create_with_file(
            cwd,
            sessions_dir,
            format!("{file_timestamp}_{session_id}.jsonl"),
            session_id,
            timestamp,
        )
    }

    /// 新建会话：使用调用方指定的确定性文件名（`<name>.jsonl`，无时间戳前缀）。
    ///
    /// 供需要 thread ↔ 会话文件稳定绑定的调用方使用（如 app-server 的
    /// `thread_id.jsonl` 映射）；文件已存在时 `create_new` 会报错，调用方应先检查。
    /// 其余语义与 `create` 相同。
    pub fn create_with_name(cwd: &Path, sessions_dir: &Path, name: &str) -> Result<Self> {
        let session_id = Uuid::now_v7().to_string();
        let timestamp = now_iso();
        Self::create_with_file(
            cwd,
            sessions_dir,
            format!("{name}.jsonl"),
            session_id,
            timestamp,
        )
    }

    /// 新建会话：文件名与 header id 都是调用方指定的 UUID（架构收敛后的稳定布局）。
    ///
    /// 用于 `~/.singularity/sessions/<uuid>.jsonl`：会话 id 与文件名统一，不再使用
    /// `thread_xxx` 文件名或随机 header id。
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

    /// 共用的新建会话实现：写入 header 并打开新文件（`create_new` 语义）。
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
            leaf_pinned: false,
            session_id,
            append_lock,
        })
    }

    /// 打开会话文件：逐行解析，v1/v2/v3 文件按 Pi `session-manager.js` 迁移逻辑升级为 v4，
    /// 发生迁移时重写文件（Pi `_setSessionFile` 行为）。
    ///
    /// 文件不存在或为空时按 Pi 语义创建新会话并写入 header；非空但无法解析为合法
    /// pi session 时报错。
    pub fn open(path: &Path) -> Result<Self> {
        // Readers share the same process-local append lock as writers. This keeps
        // settings/history/reopen paths from observing a partially written JSONL
        // line while a long-lived turn appends an entry.
        let append_lock = append_lock_for(path);
        let _guard = lock_append(&append_lock);
        Self::open_unlocked(path)
    }

    /// Open a session while the caller already owns its per-file append lock.
    /// This is used by append refresh to avoid recursively locking a non-reentrant
    /// mutex.
    fn open_unlocked(path: &Path) -> Result<Self> {
        let file = path.to_path_buf();
        let metadata = match std::fs::symlink_metadata(&file) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Self::create_empty_at(&file);
            }
            Err(error) => return Err(error.into()),
        };
        // 文件已存在但为空：fail closed，不静默重写为新随机 UUID 的新会话
        // （空文件不可能是合法 pi session；重写会掩盖身份丢失）。
        if metadata.len() == 0 {
            return Err(SessionError::InvalidSession(format!(
                "Session file is empty and cannot be opened: {}",
                file.display()
            )));
        }
        let mut parsed = parse_session_lines(&file)?;
        if parsed.entries.is_empty() {
            return Err(SessionError::InvalidSession(format!(
                "Session file is not a valid pi session: {}",
                file.display()
            )));
        }
        let header = &parsed.entries[0];
        let (session_id, version, header_cwd) = validate_header(header)?;
        let migrated = version < CURRENT_SESSION_VERSION;
        if migrated {
            migrate_entries(&mut parsed.entries, version)?;
        }
        let entries = validate_entries(&parsed.entries, &parsed.lines)?;
        if migrated || !matches!(parsed.repair, TailRepair::None) {
            rewrite_file(&file, &parsed.entries)?;
        }
        // header 的 cwd 已在迁移前取出；迁移只改变持久化 JSON，不改变会话身份。
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
            leaf_pinned: false,
            session_id,
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
            leaf_pinned: false,
            session_id,
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

    /// 追加不进入模型上下文的 metadata。所有 lifecycle/settings/usage/item
    /// 持久化都经过这一条 append seam，沿用同一 append lock 与 flush 边界。
    pub fn append_metadata(&mut self, metadata: SessionMetadata) -> Result<String> {
        // 重新走构造器校验，避免未来新增的公开字段绕过 settings 脱敏约束。
        let metadata = SessionMetadata::new(metadata.kind, metadata.fields)?;
        self.append_entry(SessionEntryType::Metadata(metadata))
    }

    /// 重开时把当前 leaf 上没有终态的 turn 标记为 synthetic interrupted。
    /// 已有终态的 turn 不重复追加；该方法不执行任何工具，也不自动重放请求。
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

    /// 返回当前 leaf 路径上的 metadata，供索引修复和公开投影读取；调用方不应
    /// 将这些条目直接作为模型消息。
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
        let (id, entry) = {
            let append_lock = Arc::clone(&self.append_lock);
            let _guard = lock_append(&append_lock);
            // A long-lived app-server may open a second SessionManager for
            // thread/settings while the turn worker still owns its manager.
            // Refresh under the shared per-file lock so the next parentId
            // follows the latest durable leaf instead of creating a branch
            // from stale in-memory state.
            if !self.leaf_pinned {
                self.refresh_from_disk_locked()?;
            }
            let id = generate_id(|candidate| self.by_id.contains_key(candidate));
            let entry = SessionEntry {
                id: id.clone(),
                parent_id: self.leaf_id.clone().unwrap_or_default(),
                timestamp: Some(now_iso()),
                entry_type,
            };
            let serialized = serde_json::to_string(&entry)?;
            let mut handle = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.file)?;
            handle.write_all(serialized.as_bytes())?;
            handle.write_all(b"\n")?;
            handle.flush()?;
            (id, entry)
        };
        // 只有 durable append 完成后才推进内存索引。
        self.by_id.insert(id.clone(), self.entries.len());
        self.entries.push(entry);
        self.leaf_id = Some(id.clone());
        self.leaf_pinned = false;
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
        self.leaf_pinned = false;
        Ok(())
    }

    /// 将 leaf 指针移动到指定条目；下次追加将成为该条目的子条目（Pi `branch`）。
    /// 已有条目不被修改或删除。
    pub fn branch(&mut self, at_entry_id: &str) -> Result<()> {
        if !self.by_id.contains_key(at_entry_id) {
            return Err(SessionError::EntryNotFound(at_entry_id.to_string()));
        }
        self.leaf_id = Some(at_entry_id.to_string());
        self.leaf_pinned = true;
        Ok(())
    }

    /// 会话 header 中的 UUID（架构收敛后也是索引主键）。
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// 当前 leaf id；无条目时为空串（Pi 的 `null`）。
    pub fn leaf_id(&self) -> &str {
        self.leaf_id.as_deref().unwrap_or("")
    }

    /// 会话文件路径。
    pub fn path(&self) -> &Path {
        &self.file
    }

    /// 会话工作目录（header 中记录的绝对路径）。
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// 构建活跃的、compaction 感知的条目列表（Pi `buildContextEntries`）。
    ///
    /// 沿 leaf→root 路径取最后一个 compaction 条目；结果 = [compaction] +
    /// (firstKeptEntryId 起、compaction 之前的所有条目) + (compaction 之后的条目)。
    /// 路径上无 compaction 时返回全部路径条目。
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
    ///
    /// 对每个没有后续 ToolResult 配对的 tool_call 块 id，追加一条 synthetic failed
    /// ToolResult（保留原 call id，明确 unknown/禁止自动重试）；不修改或删除
    /// 原始 assistant entry，也不执行任何工具。
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

    /// 当前 leaf 路径上最近一次 compaction 摘要（session/read 的默认摘要）。
    pub fn summary(&self) -> Option<String> {
        let path = self.session_path();
        path.iter()
            .rev()
            .find_map(|&index| match &self.entries[index].entry_type {
                SessionEntryType::Compaction(entry) => Some(entry.summary.clone()),
                _ => None,
            })
    }

    /// 会话文件中的条目总数（不含 header）。
    pub fn total_entries(&self) -> usize {
        self.entries.len()
    }

    /// 当前 leaf 路径上的最近 `limit` 条会话条目（旧→新；`limit=0` 返回空）。
    pub fn recent_entries(&self, limit: usize) -> Vec<SessionEntry> {
        let path = self.session_path();
        let start = path.len().saturating_sub(limit);
        path[start..]
            .iter()
            .map(|&index| self.entries[index].clone())
            .collect()
    }

    /// 按 filter/range/recent_limit 有界读取 leaf 路径条目（`SessionRepository::read`）。
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

    /// 构建发送给 LLM 的会话上下文（Pi `buildSessionContext`）。
    ///
    /// 从完整 leaf 路径提取 model/thinking 设置；消息序列由 `build_context_entries`
    /// 结果转换：user/assistant/toolResult 直转，compaction/摘要消息按 Pi
    /// `messages.js` 的前缀后缀包裹为 user 文本，custom 与其他非消息条目跳过。
    pub fn build_session_context(&self) -> Result<SessionContext> {
        let mut thinking_level = None;
        let mut model = None;
        for entry_index in self.session_path() {
            match &self.entries[entry_index].entry_type {
                SessionEntryType::ThinkingLevelChange {
                    thinking_level: level,
                } => {
                    thinking_level = Some(level.clone());
                }
                SessionEntryType::ModelChange { provider, model_id } => {
                    model = Some(format!("{provider}/{model_id}"));
                }
                _ => {}
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
            thinking_level,
        })
    }

    /// leaf→root 的路径（index 列表）。leaf 未知时回退到最后一个条目（Pi 行为）。
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
            // Pi 对损坏的 parentId 环无保护；此处防环避免死循环。
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

/// 把单条 context entry 投影为 LLM 消息（Pi `sessionEntryToContextMessages` +
/// `convertToLlm`；v4 内容块形态）。
fn entry_to_llm_messages(entry: &SessionEntry) -> Vec<LlmMessage> {
    match &entry.entry_type {
        SessionEntryType::Message(message) => match message.role {
            AgentMessageRole::User => {
                vec![ModelMessage::text(ModelRole::User, message.content_text())]
            }
            AgentMessageRole::Assistant => {
                // 带工具调用块的 assistant 消息必须重放 tool_calls（对齐 Pi convertToLlm：
                // assistant 消息原样进入 LLM 上下文）。真实 provider 拒绝孤立 tool_call_id
                // 的 tool 消息（无对应 assistant tool_calls 的历史，实测 HTTP 400）。
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
                // thinking 块不进入 ModelMessage（N2：续接时由 build_request 投影为
                // provider reasoning replay）。
                let mut llm = ModelMessage::assistant_tool_calls(tool_calls);
                llm.content = message.content_text();
                vec![llm]
            }
            AgentMessageRole::ToolResult => {
                let mut llm = ModelMessage::text(ModelRole::Tool, message.content_text());
                llm.tool_call_id = message.tool_call_id.clone();
                vec![llm]
            }
            // Pi 用 bashExecutionToText 拼接 command/output；v4 消息模型 content 为
            // 文本块，直接作为 user 文本。
            AgentMessageRole::BashExecution | AgentMessageRole::Custom => {
                vec![ModelMessage::text(ModelRole::User, message.content_text())]
            }
            AgentMessageRole::BranchSummary => vec![ModelMessage::text(
                ModelRole::User,
                format!(
                    "{BRANCH_SUMMARY_PREFIX}{}{BRANCH_SUMMARY_SUFFIX}",
                    message.content_text()
                ),
            )],
            AgentMessageRole::CompactionSummary => vec![ModelMessage::text(
                ModelRole::User,
                format!(
                    "{COMPACTION_SUMMARY_PREFIX}{}{COMPACTION_SUMMARY_SUFFIX}",
                    message.content_text()
                ),
            )],
        },
        SessionEntryType::Compaction(compaction) => vec![ModelMessage::text(
            ModelRole::User,
            format!(
                "{COMPACTION_SUMMARY_PREFIX}{}{COMPACTION_SUMMARY_SUFFIX}",
                compaction.summary
            ),
        )],
        SessionEntryType::Custom(_)
        | SessionEntryType::Metadata(_)
        | SessionEntryType::ModelChange { .. }
        | SessionEntryType::ThinkingLevelChange { .. }
        | SessionEntryType::Other(_) => Vec::new(),
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

/// 有界逐行解析 session 文件：中间坏行 fail closed，只有最后物理行明确是
/// 未完成的 JSON/UTF-8 append 才允许丢弃并原子修复。
fn parse_session_lines(file: &Path) -> Result<ParsedSessionLines> {
    let bytes = std::fs::read(file)?;
    if bytes.len() > MAX_SESSION_FILE_BYTES {
        return Err(SessionError::InvalidSession(format!(
            "session file exceeds bounded parse limits ({} bytes / {MAX_SESSION_ENTRIES} entries)",
            MAX_SESSION_FILE_BYTES
        )));
    }

    let mut entries = Vec::new();
    let mut lines = Vec::new();
    let mut repair = TailRepair::None;
    let mut start = 0usize;
    let mut line_number = 1usize;
    while start < bytes.len() {
        let newline = bytes[start..]
            .iter()
            .position(|byte| *byte == b'\n')
            .map(|offset| start + offset);
        let (end, has_newline) = match newline {
            Some(end) => (end, true),
            None => (bytes.len(), false),
        };
        let mut line = &bytes[start..end];
        if line.ends_with(b"\r") {
            line = &line[..line.len() - 1];
        }
        if line.len() > MAX_SESSION_LINE_BYTES {
            return Err(SessionError::InvalidSession(format!(
                "session entry exceeds {MAX_SESSION_LINE_BYTES} bytes at line {line_number}"
            )));
        }
        if line.iter().all(u8::is_ascii_whitespace) {
            if !has_newline {
                break;
            }
            start = end + 1;
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
        if entries.len() >= MAX_SESSION_ENTRIES {
            return Err(SessionError::InvalidSession(format!(
                "session file exceeds bounded parse limits ({} bytes / {MAX_SESSION_ENTRIES} entries)",
                MAX_SESSION_FILE_BYTES
            )));
        }
        entries.push(value);
        lines.push(line_number);
        if !has_newline {
            repair = TailRepair::AddFinalNewline;
            break;
        }
        start = end + 1;
        line_number += 1;
    }
    Ok(ParsedSessionLines {
        entries,
        lines,
        repair,
    })
}

fn validate_header(value: &Value) -> Result<(String, u32, Option<String>)> {
    let object = value
        .as_object()
        .ok_or_else(|| SessionError::InvalidHeader("header is not a JSON object".into()))?;
    if object.get("type").and_then(Value::as_str) != Some("session") {
        return Err(SessionError::InvalidHeader(
            "first entry is not a session header".into(),
        ));
    }
    let session_id = object
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.trim().is_empty())
        .ok_or_else(|| SessionError::InvalidHeader("header id must be a non-empty string".into()))?
        .to_string();
    let version = object
        .get("version")
        .and_then(Value::as_u64)
        .and_then(|version| u32::try_from(version).ok())
        .filter(|version| (1..=CURRENT_SESSION_VERSION).contains(version))
        .ok_or_else(|| {
            SessionError::InvalidHeader(format!(
                "header version must be an integer in 1..={CURRENT_SESSION_VERSION}"
            ))
        })?;
    let cwd = match object.get("cwd") {
        None | Some(Value::Null) => None,
        Some(Value::String(cwd)) => Some(cwd.clone()),
        Some(_) => {
            return Err(SessionError::InvalidHeader(
                "header cwd must be a string".into(),
            ));
        }
    };
    Ok((session_id, version, cwd))
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
    let roots = entries
        .iter()
        .filter(|entry| entry.parent_id.is_empty())
        .count();
    if roots > 1 {
        return Err(SessionError::InvalidStructure(format!(
            "session tree has {roots} roots; at most one is allowed"
        )));
    }
    for entry in &entries {
        if !entry.parent_id.is_empty() && !parent_by_id.contains_key(&entry.parent_id) {
            return Err(SessionError::MissingParent {
                entry_id: entry.id.clone(),
                parent_id: entry.parent_id.clone(),
            });
        }
    }
    // 每个节点至多沿 parent 链走一次，避免 200k 条目线性链退化为 O(n²)。
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
    Ok(entries)
}

fn strict_entry_from_value(value: &Value) -> std::result::Result<SessionEntry, String> {
    let object = value
        .as_object()
        .ok_or_else(|| "session entry is not a JSON object".to_string())?;
    let id = object
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.trim().is_empty())
        .ok_or_else(|| "session entry id must be a non-empty string".to_string())?
        .to_string();
    let parent_id = match object.get("parentId") {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(parent)) => parent.clone(),
        Some(_) => return Err("session entry parentId is not a string".into()),
    };
    let timestamp = object
        .get("timestamp")
        .and_then(Value::as_str)
        .map(str::to_string);
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
        Some("custom") => SessionEntryType::Custom(
            serde_json::from_value(value.clone())
                .map_err(|error| format!("invalid custom payload: {error}"))?,
        ),
        Some("metadata") => SessionEntryType::Metadata(
            serde_json::from_value(metadata_payload(value))
                .map_err(|error| format!("invalid metadata payload: {error}"))?,
        ),
        Some("model_change") => {
            let wire: ModelChangeWire = serde_json::from_value(value.clone())
                .map_err(|error| format!("invalid model_change payload: {error}"))?;
            SessionEntryType::ModelChange {
                provider: wire.provider,
                model_id: wire.model_id,
            }
        }
        Some("thinking_level_change") => {
            let wire: ThinkingLevelChangeWire = serde_json::from_value(value.clone())
                .map_err(|error| format!("invalid thinking_level_change payload: {error}"))?;
            SessionEntryType::ThinkingLevelChange {
                thinking_level: wire.thinking_level,
            }
        }
        Some(_) => SessionEntryType::Other(value.clone()),
        None => return Err("session entry has no type".into()),
    };
    Ok(SessionEntry {
        id,
        parent_id,
        timestamp,
        entry_type,
    })
}

/// v1/v2/v3→v4 迁移（Pi `migrateV1ToV2`/`migrateV2ToV3` + v3→v4 内容块化，
/// 原地修改，仅 version < 4 时调用）。
fn migrate_entries(entries: &mut Vec<Value>, version: u32) -> Result<()> {
    if version < 2 {
        migrate_v1_to_v2(entries.as_mut_slice());
    }
    if version < 3 {
        migrate_v2_to_v3(entries.as_mut_slice());
    }
    if version < 4 {
        migrate_v3_to_v4(entries, version == 3)?;
    }
    Ok(())
}

/// v1 → v2：为每条 entry 分配 8 位十六进制 id 与 parentId 链；compaction 的
/// `firstKeptEntryIndex`（原始数组下标，含 header 在 0 位）转换为 `firstKeptEntryId`。
fn migrate_v1_to_v2(entries: &mut [Value]) {
    let mut ids = HashSet::new();
    let mut prev_id: Option<String> = None;
    for index in 0..entries.len() {
        let entry_type = entries[index]
            .get("type")
            .and_then(Value::as_str)
            .map(str::to_string);
        if entry_type.as_deref() == Some("session") {
            if let Some(object) = entries[index].as_object_mut() {
                object.insert("version".to_string(), json!(2));
            }
            continue;
        }
        let first_kept_index = entries[index]
            .get("firstKeptEntryIndex")
            .and_then(Value::as_u64);
        let id = generate_id(|candidate| ids.contains(candidate));
        ids.insert(id.clone());
        if let Some(object) = entries[index].as_object_mut() {
            object.insert("id".to_string(), json!(id.clone()));
            object.insert(
                "parentId".to_string(),
                prev_id.clone().map_or(Value::Null, |parent| json!(parent)),
            );
        }
        prev_id = Some(id);
        if entry_type.as_deref() == Some("compaction")
            && let Some(first_kept_index) = first_kept_index
        {
            let target_id = entries.get(first_kept_index as usize).and_then(|target| {
                if target.get("type").and_then(Value::as_str) != Some("session") {
                    target.get("id").and_then(Value::as_str).map(str::to_string)
                } else {
                    None
                }
            });
            if let Some(object) = entries[index].as_object_mut() {
                if let Some(target_id) = target_id {
                    object.insert("firstKeptEntryId".to_string(), json!(target_id));
                }
                object.remove("firstKeptEntryIndex");
            }
        }
    }
}

/// v2 → v3：header version 升为 3；message 条目的 `hookMessage` role 改名 `custom`。
fn migrate_v2_to_v3(entries: &mut [Value]) {
    for entry in entries.iter_mut() {
        let Some(object) = entry.as_object_mut() else {
            continue;
        };
        match object.get("type").and_then(Value::as_str) {
            Some("session") => {
                object.insert("version".to_string(), json!(3));
            }
            Some("message") => {
                if let Some(Value::Object(message)) = object.get_mut("message")
                    && message.get("role").and_then(Value::as_str) == Some("hookMessage")
                {
                    message.insert("role".to_string(), json!("custom"));
                }
            }
            _ => {}
        }
    }
}

/// v3 → v4：header version 升为 4；消息 `content` 字符串改写为 content block 数组：
///
/// - 全部角色：`"content": "..."` → `[{"type":"text","text":"..."}]`；
/// - v3 assistant 单工具调用消息（携带 `toolCallId`/`toolName`/`args`）：
///   文本块（非空时）+ `{"type":"tool_call","id":...,"name":...,"args":...}`，
///   并删除消息级 `toolCallId`/`toolName`/`args` 字段；
/// - `toolResult` 保留 `toolCallId`/`toolName`（关联 assistant tool call）。
///   对同一响应的多个工具调用，只有在连续 assistant 条目、同序 toolResult 条目、
///   parent 链和引用关系都完整匹配时才合并；歧义布局直接失败并保留原文件。
fn migrate_v3_to_v4(entries: &mut Vec<Value>, merge_tool_batches: bool) -> Result<()> {
    if merge_tool_batches {
        validate_v3_tool_call_batches(entries)?;
    }
    for entry in entries.iter_mut() {
        let Some(object) = entry.as_object_mut() else {
            continue;
        };
        match object.get("type").and_then(Value::as_str) {
            Some("session") => {
                object.insert("version".to_string(), json!(4));
            }
            Some("message") => {
                let Some(Value::Object(message)) = object.get_mut("message") else {
                    continue;
                };
                let role = message
                    .get("role")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let text = message
                    .get("content")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let mut blocks: Vec<Value> = Vec::new();
                if !text.is_empty() {
                    blocks.push(json!({"type": "text", "text": text}));
                }
                if role == "assistant"
                    && let (Some(id), Some(name), args) = (
                        message.get("toolCallId").and_then(Value::as_str),
                        message.get("toolName").and_then(Value::as_str),
                        message.get("args").cloned(),
                    )
                {
                    blocks.push(json!({
                        "type": "tool_call",
                        "id": id,
                        "name": name,
                        "args": args.unwrap_or(Value::Null),
                    }));
                    message.remove("toolCallId");
                    message.remove("toolName");
                    message.remove("args");
                }
                message.insert("content".to_string(), Value::Array(blocks));
            }
            _ => {}
        }
    }
    if merge_tool_batches {
        merge_v3_tool_call_batches(entries)?;
    }
    Ok(())
}

fn ambiguous_v3_batch(reason: impl Into<String>) -> SessionError {
    SessionError::InvalidStructure(format!("ambiguous v3 multi-tool batch: {}", reason.into()))
}

fn message_role(value: &Value) -> Option<&str> {
    value
        .get("message")
        .and_then(Value::as_object)
        .and_then(|message| message.get("role"))
        .and_then(Value::as_str)
}

fn message_tool_call_id(value: &Value) -> Option<&str> {
    value
        .get("message")
        .and_then(Value::as_object)
        .and_then(|message| message.get("toolCallId"))
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
}

fn message_content_non_empty(value: &Value) -> bool {
    value
        .get("message")
        .and_then(Value::as_object)
        .and_then(|message| message.get("content"))
        .and_then(Value::as_str)
        .is_some_and(|content| !content.is_empty())
}

fn entry_id(value: &Value) -> Option<&str> {
    value.get("id").and_then(Value::as_str)
}

fn entry_parent_id(value: &Value) -> Option<&str> {
    value.get("parentId").and_then(Value::as_str)
}

fn entry_first_kept_id(value: &Value) -> Option<&str> {
    value.get("firstKeptEntryId").and_then(Value::as_str)
}

fn validate_v3_tool_call_batches(entries: &[Value]) -> Result<()> {
    let mut index = 1usize;
    while index < entries.len() {
        if message_role(&entries[index]) != Some("assistant") {
            index += 1;
            continue;
        }
        let start = index;
        while index < entries.len() && message_role(&entries[index]) == Some("assistant") {
            index += 1;
        }
        let end = index;
        let count = end - start;
        let tool_count = (start..end)
            .filter(|&position| message_tool_call_id(&entries[position]).is_some())
            .count();
        if tool_count == 0 {
            continue;
        }
        if tool_count != count {
            return Err(ambiguous_v3_batch(
                "consecutive assistant entries mix tool calls and plain messages",
            ));
        }
        if count == 1 {
            continue;
        }
        if end.saturating_add(count) > entries.len() {
            return Err(ambiguous_v3_batch("toolResult run is incomplete"));
        }

        let mut call_ids = Vec::with_capacity(count);
        let mut seen_call_ids = HashSet::with_capacity(count);
        for position in start..end {
            let message = entries[position]
                .get("message")
                .and_then(Value::as_object)
                .ok_or_else(|| ambiguous_v3_batch("assistant message payload is missing"))?;
            let call_id = message_tool_call_id(&entries[position])
                .ok_or_else(|| ambiguous_v3_batch("assistant tool call id is missing"))?;
            if message
                .get("toolName")
                .and_then(Value::as_str)
                .is_none_or(|name| name.is_empty())
                || !message.contains_key("args")
            {
                return Err(ambiguous_v3_batch(
                    "assistant tool call name or args is missing",
                ));
            }
            if position > start {
                if message_content_non_empty(&entries[position]) {
                    return Err(ambiguous_v3_batch(
                        "non-first assistant entry contains text",
                    ));
                }
                let previous_id = entry_id(&entries[position - 1])
                    .ok_or_else(|| ambiguous_v3_batch("assistant entry id is missing"))?;
                if entry_parent_id(&entries[position]) != Some(previous_id) {
                    return Err(ambiguous_v3_batch("assistant parent chain is not linear"));
                }
            }
            if !seen_call_ids.insert(call_id) {
                return Err(ambiguous_v3_batch("assistant tool call ids are duplicated"));
            }
            call_ids.push(call_id.to_string());
        }

        for offset in 0..count {
            let result = &entries[end + offset];
            if message_role(result) != Some("toolResult")
                || message_tool_call_id(result) != Some(call_ids[offset].as_str())
            {
                return Err(ambiguous_v3_batch(
                    "toolResult ids or order do not match assistant calls",
                ));
            }
            let expected_parent = if offset == 0 {
                entry_id(&entries[end - 1])
            } else {
                entry_id(&entries[end + offset - 1])
            };
            if entry_parent_id(result) != expected_parent {
                return Err(ambiguous_v3_batch("toolResult parent chain is not linear"));
            }
        }

        let dropped_ids: HashSet<&str> = (start + 1..end)
            .filter_map(|position| entry_id(&entries[position]))
            .collect();
        for (position, entry) in entries.iter().enumerate() {
            if (start..end + count).contains(&position) {
                continue;
            }
            if entry_parent_id(entry).is_some_and(|id| dropped_ids.contains(id))
                || entry_first_kept_id(entry).is_some_and(|id| dropped_ids.contains(id))
            {
                return Err(ambiguous_v3_batch(
                    "another entry references an assistant id that would be removed",
                ));
            }
        }
    }
    Ok(())
}

fn single_converted_tool_call(value: &Value) -> Option<Value> {
    if message_role(value) != Some("assistant") {
        return None;
    }
    let blocks = value
        .get("message")
        .and_then(Value::as_object)
        .and_then(|message| message.get("content"))
        .and_then(Value::as_array)?;
    let calls: Vec<Value> = blocks
        .iter()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("tool_call"))
        .cloned()
        .collect();
    (calls.len() == 1).then(|| calls.into_iter().next().expect("one call exists"))
}

fn merge_v3_tool_call_batches(entries: &mut Vec<Value>) -> Result<()> {
    let mut index = 1usize;
    while index < entries.len() {
        if single_converted_tool_call(&entries[index]).is_none() {
            index += 1;
            continue;
        }
        let start = index;
        while index < entries.len() && single_converted_tool_call(&entries[index]).is_some() {
            index += 1;
        }
        let end = index;
        let count = end - start;
        if count < 2 {
            continue;
        }
        if end.saturating_add(count) > entries.len() {
            return Err(ambiguous_v3_batch("converted toolResult run is incomplete"));
        }
        let mut merged_content = entries[start]
            .get("message")
            .and_then(Value::as_object)
            .and_then(|message| message.get("content"))
            .and_then(Value::as_array)
            .cloned()
            .ok_or_else(|| ambiguous_v3_batch("converted assistant content is missing"))?;
        for entry in entries.iter().take(end).skip(start + 1) {
            merged_content.push(
                single_converted_tool_call(entry)
                    .ok_or_else(|| ambiguous_v3_batch("converted tool call is missing"))?,
            );
        }
        let assistant_id = entry_id(&entries[start])
            .ok_or_else(|| ambiguous_v3_batch("merged assistant entry id is missing"))?
            .to_string();
        entries[start]
            .get_mut("message")
            .and_then(Value::as_object_mut)
            .ok_or_else(|| ambiguous_v3_batch("merged assistant message is missing"))?
            .insert("content".to_string(), Value::Array(merged_content));
        entries[end]
            .as_object_mut()
            .ok_or_else(|| ambiguous_v3_batch("first toolResult entry is missing"))?
            .insert("parentId".to_string(), Value::String(assistant_id));
        entries.drain(start + 1..end);
        index = start + 1 + count;
    }
    Ok(())
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

/// 迁移或尾部修复后原子重写整个文件。原文件只在临时文件完整 flush/sync
/// 且 replace 成功后才会被替换。
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

/// Pi `generateId`：8 位十六进制（`randomUUID().slice(0, 8)`），冲突检查后重试，
/// 兜底完整 uuid（Pi 同样兜底）。
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

/// 当前 UTC 时间的 Pi 风格 ISO8601 毫秒字符串（`2025-01-15T10:30:00.000Z`）。
fn now_iso() -> String {
    OffsetDateTime::now_utc()
        .format(&format_description!(
            "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z"
        ))
        .expect("utc timestamp always formats")
}

/// 绝对路径（词法归一化，无需路径存在），对齐 Pi `resolvePath`。
fn normalize_abs_path(path: &Path) -> Result<PathBuf> {
    Ok(std::path::absolute(path)?)
}

/// Windows 反斜杠归一化为 `/`，对齐 Pi `normalizePath` 写盘结果。
fn normalize_cwd_string(cwd: &Path) -> String {
    cwd.to_string_lossy().replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use singularity_model::{ModelRole, ProviderReasoningReplay};

    fn user(text: &str) -> AgentMessage {
        AgentMessage {
            role: AgentMessageRole::User,
            content: vec![ContentBlock::Text {
                text: text.to_string(),
            }],
            provider_reasoning_replay: None,
            tool_call_id: None,
            tool_name: None,
            is_error: None,
            timestamp: Some(1_700_000_000_000),
        }
    }

    fn assistant(text: &str) -> AgentMessage {
        AgentMessage {
            role: AgentMessageRole::Assistant,
            content: vec![ContentBlock::Text {
                text: text.to_string(),
            }],
            provider_reasoning_replay: None,
            tool_call_id: None,
            tool_name: None,
            is_error: None,
            timestamp: Some(1_700_000_000_001),
        }
    }

    fn tool_result(call_id: &str, text: &str) -> AgentMessage {
        AgentMessage {
            role: AgentMessageRole::ToolResult,
            content: vec![ContentBlock::Text {
                text: text.to_string(),
            }],
            provider_reasoning_replay: None,
            tool_call_id: Some(call_id.to_string()),
            tool_name: Some("bash".to_string()),
            is_error: None,
            timestamp: Some(1_700_000_000_002),
        }
    }

    fn compaction(summary: &str, first_kept_entry_id: Option<String>) -> CompactionEntry {
        CompactionEntry {
            summary: summary.to_string(),
            first_kept_entry_id,
            tokens_before: Some(100),
            previous_summary: None,
            details: None,
        }
    }

    fn entry_ids(entries: &[SessionEntry]) -> Vec<String> {
        entries.iter().map(|entry| entry.id.clone()).collect()
    }

    /// 1. create → 追加 3 条消息 → 重开 open → 条目一致、leaf 一致。
    #[test]
    fn create_append_reopen_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        // cwd 无需存在（Pi resolvePath 不要求路径存在）。
        let cwd = dir.path().join("project");
        let mut manager = SessionManager::create(&cwd, &sessions).unwrap();
        assert!(manager.leaf_id().is_empty());

        let id1 = manager.append_message(user("hello")).unwrap();
        let id2 = manager.append_message(assistant("hi there")).unwrap();
        let id3 = manager
            .append_message(tool_result("call_1", "ls output"))
            .unwrap();
        let file = manager.path().to_path_buf();
        let leaf = manager.leaf_id().to_string();
        assert_eq!(leaf, id3);

        // header 已写入；文件名 <ts>_<uuid>.jsonl 且 uuid 段 = header id。
        let content = std::fs::read_to_string(&file).unwrap();
        let first_line: Value = serde_json::from_str(content.lines().next().unwrap()).unwrap();
        assert_eq!(first_line["type"], "session");
        assert_eq!(first_line["version"], 4);
        assert_eq!(
            first_line["cwd"],
            normalize_cwd_string(&std::path::absolute(&cwd).unwrap())
        );
        let file_name = manager.path().file_name().unwrap().to_str().unwrap();
        assert!(file_name.ends_with(".jsonl"));
        let header_ts = first_line["timestamp"]
            .as_str()
            .unwrap()
            .replace([':', '.'], "-");
        assert_eq!(file_name.rsplit_once('_').unwrap().0, header_ts);
        let header_id = first_line["id"].as_str().unwrap();
        assert!(file_name.ends_with(&format!("_{header_id}.jsonl")));
        drop(manager);

        let opened = SessionManager::open(&file).unwrap();
        assert_eq!(opened.leaf_id(), leaf);
        let entries = opened.build_context_entries().unwrap();
        assert_eq!(entry_ids(&entries), vec![id1, id2, id3]);
        // Pi entry id 为 8 位十六进制。
        for entry in &entries {
            assert_eq!(entry.id.len(), 8);
            assert!(entry.id.chars().all(|c| c.is_ascii_hexdigit()));
        }
        assert!(matches!(&entries[0].entry_type,
            SessionEntryType::Message(m) if m.role == AgentMessageRole::User && m.content_text() == "hello"));
        assert!(matches!(&entries[1].entry_type,
            SessionEntryType::Message(m) if m.role == AgentMessageRole::Assistant && m.content_text() == "hi there"));
        assert!(matches!(&entries[2].entry_type,
            SessionEntryType::Message(m) if m.role == AgentMessageRole::ToolResult
                && m.tool_call_id.as_deref() == Some("call_1")
                && m.tool_name.as_deref() == Some("bash")));
    }

    /// 1b. create_with_name：确定性文件名，追加/重开语义与 create 一致。
    #[test]
    fn create_with_name_uses_deterministic_file_name() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        let cwd = dir.path().join("project");
        let mut manager = SessionManager::create_with_name(&cwd, &sessions, "thread_abc").unwrap();
        assert_eq!(manager.path().file_name().unwrap(), "thread_abc.jsonl");
        assert!(manager.leaf_id().is_empty());

        let id1 = manager.append_message(user("hello")).unwrap();
        let file = manager.path().to_path_buf();
        drop(manager);

        let opened = SessionManager::open(&file).unwrap();
        assert_eq!(opened.leaf_id(), id1);
        let entries = opened.build_context_entries().unwrap();
        assert_eq!(entry_ids(&entries), vec![id1]);
        // header cwd 与 create 语义一致（绝对路径）。
        let content = std::fs::read_to_string(&file).unwrap();
        let first_line: Value = serde_json::from_str(content.lines().next().unwrap()).unwrap();
        assert_eq!(first_line["type"], "session");
        assert_eq!(
            first_line["cwd"],
            normalize_cwd_string(&std::path::absolute(&cwd).unwrap())
        );
    }

    /// 1c. 已存在但为空的 session 文件 fail closed：不静默重写为新随机 UUID。
    #[test]
    fn empty_existing_session_file_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("empty.jsonl");
        std::fs::write(&file, b"").unwrap();

        let error = SessionManager::open(&file)
            .err()
            .expect("empty session file must fail closed");
        assert!(matches!(error, SessionError::InvalidSession(_)));
        assert!(error.to_string().contains("empty"));
        // 文件保持原样，未被重写为 header（身份不得静默丢失）。
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "");

        // 缺失文件仍按 Pi 语义创建新会话（不回归）。
        let missing = dir.path().join("missing.jsonl");
        let created = SessionManager::open(&missing).unwrap();
        assert!(created.path().is_file());
    }

    /// 2. 追加顺序：每条 parent = 前一条 id，首条为根（磁盘上 parentId 为 null）。
    #[test]
    fn append_chain_parent_ids() {
        let dir = tempfile::tempdir().unwrap();
        let mut manager = SessionManager::create(dir.path(), dir.path()).unwrap();
        let id1 = manager.append_message(user("a")).unwrap();
        let id2 = manager.append_message(user("b")).unwrap();
        let id3 = manager.append_message(assistant("c")).unwrap();

        let entries = manager.build_context_entries().unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].parent_id, "");
        assert_eq!(entries[1].parent_id, id1);
        assert_eq!(entries[2].parent_id, id2);
        assert_eq!(manager.leaf_id(), id3);

        let content = std::fs::read_to_string(manager.path()).unwrap();
        let lines: Vec<Value> = content
            .lines()
            .skip(1)
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(lines[0]["parentId"], Value::Null);
        assert_eq!(lines[1]["parentId"], id1);
        assert_eq!(lines[2]["parentId"], id2);
    }

    /// 3. branch：leaf 变化；再追加挂在分支下；原路径不受影响；未知 id 报错。
    #[test]
    fn branch_moves_leaf_and_keeps_original_path() {
        let dir = tempfile::tempdir().unwrap();
        let mut manager = SessionManager::create(dir.path(), dir.path()).unwrap();
        let id1 = manager.append_message(user("first")).unwrap();
        let id2 = manager.append_message(user("second")).unwrap();
        let _id3 = manager.append_message(user("third")).unwrap();

        manager.branch(&id2).unwrap();
        assert_eq!(manager.leaf_id(), id2);
        let id4 = manager.append_message(user("branched")).unwrap();
        assert_eq!(manager.leaf_id(), id4);

        let entries = manager.build_context_entries().unwrap();
        assert_eq!(entry_ids(&entries), vec![id1, id2, id4]);

        assert!(matches!(
            manager.branch("deadbeef"),
            Err(SessionError::EntryNotFound(_))
        ));
    }

    /// 4. 迁移：手工构造 v1 与 v2 样例 → open → 内容与 v3 语义一致，文件被重写。
    #[test]
    fn open_migrates_v1_file_to_v4() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("v1.jsonl");
        let lines = [
            r#"{"type":"session","version":1,"id":"v1-session","timestamp":"2024-01-01T00:00:00.000Z","cwd":"C:/work"}"#,
            r#"{"type":"message","timestamp":"2024-01-01T00:00:01.000Z","message":{"role":"user","content":"a"}}"#,
            r#"{"type":"message","timestamp":"2024-01-01T00:00:02.000Z","message":{"role":"assistant","content":"b"}}"#,
            r#"{"type":"compaction","timestamp":"2024-01-01T00:00:03.000Z","summary":"summary of a and b","tokensBefore":100,"firstKeptEntryIndex":1}"#,
            r#"{"type":"message","timestamp":"2024-01-01T00:00:04.000Z","message":{"role":"user","content":"c"}}"#,
            r#"{"type":"label","timestamp":"2024-01-01T00:00:05.000Z","targetId":"t1","label":"checkpoint"}"#,
        ];
        std::fs::write(&file, lines.join("\n")).unwrap();

        let manager = SessionManager::open(&file).unwrap();
        let entries = manager.build_context_entries().unwrap();
        assert_eq!(entries.len(), 5);
        for entry in &entries {
            assert_eq!(entry.id.len(), 8);
            assert!(entry.id.chars().all(|c| c.is_ascii_hexdigit()));
        }
        // 切片顺序 = [compaction] + (firstKeptEntryId 起) + (compaction 之后)：
        // 路径为 msg_a→msg_b→comp→msg_c→label，firstKeptEntryId=msg_a。
        assert!(matches!(
            entries[0].entry_type,
            SessionEntryType::Compaction(_)
        ));
        assert!(matches!(&entries[1].entry_type,
            SessionEntryType::Message(m) if m.role == AgentMessageRole::User && m.content_text() == "a"));
        assert!(matches!(&entries[2].entry_type,
            SessionEntryType::Message(m) if m.role == AgentMessageRole::Assistant && m.content_text() == "b"));
        assert!(matches!(&entries[3].entry_type,
            SessionEntryType::Message(m) if m.role == AgentMessageRole::User && m.content_text() == "c"));
        // parent 链（按原文件顺序）：msg_a 为根；msg_b/comp/msg_c/label 各自挂接。
        assert_eq!(entries[1].parent_id, "");
        assert_eq!(entries[2].parent_id, entries[1].id);
        assert_eq!(entries[0].parent_id, entries[2].id);
        assert_eq!(entries[3].parent_id, entries[0].id);
        assert_eq!(entries[4].parent_id, entries[3].id);
        // firstKeptEntryIndex=1 是含 header（0 位）的原始数组下标 → 第一条消息 "a"。
        let comp = match &entries[0].entry_type {
            SessionEntryType::Compaction(comp) => comp,
            _ => unreachable!(),
        };
        assert_eq!(
            comp.first_kept_entry_id.as_deref(),
            Some(entries[1].id.as_str())
        );
        assert_eq!(comp.tokens_before, Some(100));
        assert_eq!(comp.summary, "summary of a and b");
        // label 条目以 Other 原样往返。
        let label_json: Value = serde_json::to_value(&entries[4]).unwrap();
        assert_eq!(label_json["type"], "label");
        assert_eq!(label_json["label"], "checkpoint");
        assert_eq!(label_json["targetId"], "t1");
        // 切片边界：firstKeptEntryId=msg_a（含边界）→ context 恰好包含全部 5 条。
        let context_ids: Vec<String> = manager
            .build_context_entries()
            .unwrap()
            .into_iter()
            .map(|entry| entry.id)
            .collect();
        assert_eq!(context_ids, entry_ids(&entries));
        // 文件已重写为 v4：header version 4，条目带 id/parentId，索引已转 id。
        let rewritten: Vec<Value> = std::fs::read_to_string(&file)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect();
        assert_eq!(rewritten.len(), 6);
        assert_eq!(rewritten[0]["version"], 4);
        for entry in &rewritten[1..] {
            assert!(entry.get("id").is_some());
            assert!(entry.get("parentId").is_some());
        }
        let comp_wire = rewritten
            .iter()
            .find(|entry| entry["type"] == "compaction")
            .unwrap();
        assert!(comp_wire.get("firstKeptEntryId").is_some());
        assert!(comp_wire.get("firstKeptEntryIndex").is_none());
        // 重写后重新打开语义一致（迁移幂等）。
        let reopened = SessionManager::open(&file).unwrap();
        assert_eq!(reopened.build_context_entries().unwrap().len(), 5);
    }

    #[test]
    fn open_migrates_v2_hook_message_role_to_v4() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("v2.jsonl");
        let lines = [
            r#"{"type":"session","version":2,"id":"v2-session","timestamp":"2024-01-01T00:00:00.000Z","cwd":"C:/work"}"#,
            r#"{"type":"message","id":"aaaa1111","parentId":null,"timestamp":"2024-01-01T00:00:01.000Z","message":{"role":"user","content":"hello"}}"#,
            r#"{"type":"message","id":"bbbb2222","parentId":"aaaa1111","timestamp":"2024-01-01T00:00:02.000Z","message":{"role":"hookMessage","customType":"ext","content":"injected"}}"#,
        ];
        std::fs::write(&file, lines.join("\n")).unwrap();

        let manager = SessionManager::open(&file).unwrap();
        let entries = manager.build_context_entries().unwrap();
        assert_eq!(entries.len(), 2);
        assert!(matches!(&entries[1].entry_type,
            SessionEntryType::Message(m) if m.role == AgentMessageRole::Custom && m.content_text() == "injected"));
        // 文件已重写为 v4，hookMessage → custom。
        let rewritten: Vec<Value> = std::fs::read_to_string(&file)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect();
        assert_eq!(rewritten[0]["version"], 4);
        assert_eq!(rewritten[2]["message"]["role"], "custom");
    }

    /// hookMessage → custom；v3 → v4 内容块化：user/assistant 消息 content 字符串
    /// 改写为 text 块数组，assistant 单工具调用字段迁移为 tool_call 块。
    #[test]
    fn open_migrates_v3_content_blocks_to_v4() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("v3.jsonl");
        let lines = [
            r#"{"type":"session","version":3,"id":"v3-session","timestamp":"2024-01-01T00:00:00.000Z","cwd":"C:/work"}"#,
            r#"{"type":"message","id":"aaaa1111","parentId":null,"timestamp":"2024-01-01T00:00:01.000Z","message":{"role":"user","content":"fix it"}}"#,
            r#"{"type":"message","id":"bbbb2222","parentId":"aaaa1111","timestamp":"2024-01-01T00:00:02.000Z","message":{"role":"assistant","content":"calling tool","toolCallId":"c1","toolName":"write","args":{"path":"out.txt","content":"x"}}}"#,
            r#"{"type":"message","id":"cccc3333","parentId":"bbbb2222","timestamp":"2024-01-01T00:00:03.000Z","message":{"role":"toolResult","content":"wrote c1","toolCallId":"c1","toolName":"write"}}"#,
        ];
        std::fs::write(&file, lines.join("\n")).unwrap();

        let manager = SessionManager::open(&file).unwrap();
        let entries = manager.build_context_entries().unwrap();
        assert!(matches!(&entries[0].entry_type,
            SessionEntryType::Message(m) if m.role == AgentMessageRole::User
                && m.content_text() == "fix it"));
        assert!(matches!(&entries[1].entry_type,
        SessionEntryType::Message(m) if m.role == AgentMessageRole::Assistant
            && m.content_text() == "calling tool"
            && m.tool_calls().len() == 1
            && matches!(
                m.tool_calls()[0],
                ContentBlock::ToolCall { id, name, args }
                    if id == "c1"
                        && name == "write"
                        && args == &serde_json::json!({"path": "out.txt", "content": "x"})
            )));
        assert!(matches!(&entries[2].entry_type,
            SessionEntryType::Message(m) if m.role == AgentMessageRole::ToolResult
                && m.tool_call_id.as_deref() == Some("c1")));
        // 文件已重写为 v4：header version 4，content 为数组，toolCallId 已移除。
        let rewritten: Vec<Value> = std::fs::read_to_string(&file)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect();
        assert_eq!(rewritten[0]["version"], 4);
        let assistant = &rewritten[2]["message"];
        assert!(assistant["content"].is_array());
        assert_eq!(assistant["content"][0]["type"], "text");
        assert_eq!(assistant["content"][1]["type"], "tool_call");
        assert_eq!(assistant["content"][1]["id"], "c1");
        assert_eq!(assistant["content"][1]["name"], "write");
        assert!(assistant.get("toolCallId").is_none());
        assert!(assistant.get("args").is_none());
    }

    #[test]
    fn open_migrates_v3_multi_tool_batch_to_one_assistant_entry() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("v3-multi.jsonl");
        let lines = [
            r#"{"type":"session","version":3,"id":"v3-multi-session","timestamp":"2024-01-01T00:00:00.000Z","cwd":"C:/work"}"#,
            r#"{"type":"message","id":"user0001","parentId":null,"timestamp":"2024-01-01T00:00:01.000Z","message":{"role":"user","content":"fix both"}}"#,
            r#"{"type":"message","id":"assist001","parentId":"user0001","timestamp":"2024-01-01T00:00:02.000Z","message":{"role":"assistant","content":"calling both","toolCallId":"call_1","toolName":"write","args":{"path":"a.txt","content":"a"}}}"#,
            r#"{"type":"message","id":"assist002","parentId":"assist001","timestamp":"2024-01-01T00:00:02.001Z","message":{"role":"assistant","content":"","toolCallId":"call_2","toolName":"write","args":{"path":"b.txt","content":"b"}}}"#,
            r#"{"type":"message","id":"result001","parentId":"assist002","timestamp":"2024-01-01T00:00:03.000Z","message":{"role":"toolResult","content":"wrote a","toolCallId":"call_1","toolName":"write"}}"#,
            r#"{"type":"message","id":"result002","parentId":"result001","timestamp":"2024-01-01T00:00:04.000Z","message":{"role":"toolResult","content":"wrote b","toolCallId":"call_2","toolName":"write"}}"#,
        ];
        std::fs::write(&file, lines.join("\n")).unwrap();

        let manager = SessionManager::open(&file).unwrap();
        let entries = manager.build_context_entries().unwrap();
        assert_eq!(entries.len(), 4);
        let assistant = match &entries[1].entry_type {
            SessionEntryType::Message(message) => message,
            other => panic!("expected merged assistant message, got {other:?}"),
        };
        assert_eq!(assistant.role, AgentMessageRole::Assistant);
        assert_eq!(assistant.content_text(), "calling both");
        let calls = assistant.tool_calls();
        assert_eq!(calls.len(), 2);
        assert!(matches!(
            calls[0],
            ContentBlock::ToolCall { id, name, args }
                if id == "call_1"
                    && name == "write"
                    && args == &serde_json::json!({"path":"a.txt","content":"a"})
        ));
        assert!(matches!(
            calls[1],
            ContentBlock::ToolCall { id, name, args }
                if id == "call_2"
                    && name == "write"
                    && args == &serde_json::json!({"path":"b.txt","content":"b"})
        ));
        let context = manager.build_session_context().unwrap();
        assert_eq!(context.messages[1].tool_calls.len(), 2);
        assert_eq!(context.messages[2].tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(context.messages[3].tool_call_id.as_deref(), Some("call_2"));
        assert!(matches!(
            &entries[2].entry_type,
            SessionEntryType::Message(message)
                if message.role == AgentMessageRole::ToolResult
                    && message.tool_call_id.as_deref() == Some("call_1")
        ));
        assert!(matches!(
            &entries[3].entry_type,
            SessionEntryType::Message(message)
                if message.role == AgentMessageRole::ToolResult
                    && message.tool_call_id.as_deref() == Some("call_2")
        ));

        let rewritten: Vec<Value> = std::fs::read_to_string(&file)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect();
        assert_eq!(rewritten.len(), 5);
        assert_eq!(rewritten[0]["version"], 4);
        assert_eq!(
            rewritten[2]["message"]["content"].as_array().unwrap().len(),
            3
        );
        assert_eq!(rewritten[2]["message"]["content"][1]["id"], "call_1");
        assert_eq!(rewritten[2]["message"]["content"][2]["id"], "call_2");
        assert!(rewritten[2]["message"].get("toolCallId").is_none());
        assert_eq!(rewritten[3]["parentId"], "assist001");
    }

    #[test]
    fn open_rejects_ambiguous_v3_multi_tool_batch_without_rewriting() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("v3-ambiguous.jsonl");
        let lines = [
            r#"{"type":"session","version":3,"id":"v3-ambiguous-session","timestamp":"2024-01-01T00:00:00.000Z","cwd":"C:/work"}"#,
            r#"{"type":"message","id":"user0001","parentId":null,"timestamp":"2024-01-01T00:00:01.000Z","message":{"role":"user","content":"fix both"}}"#,
            r#"{"type":"message","id":"assist001","parentId":"user0001","timestamp":"2024-01-01T00:00:02.000Z","message":{"role":"assistant","content":"calling both","toolCallId":"call_1","toolName":"write","args":{"path":"a.txt","content":"a"}}}"#,
            r#"{"type":"message","id":"assist002","parentId":"assist001","timestamp":"2024-01-01T00:00:02.001Z","message":{"role":"assistant","content":"","toolCallId":"call_2","toolName":"write","args":{"path":"b.txt","content":"b"}}}"#,
            r#"{"type":"message","id":"result001","parentId":"assist002","timestamp":"2024-01-01T00:00:03.000Z","message":{"role":"toolResult","content":"wrote unknown","toolCallId":"call_unknown","toolName":"write"}}"#,
        ];
        std::fs::write(&file, lines.join("\n")).unwrap();
        let original = std::fs::read(&file).unwrap();

        let error = match SessionManager::open(&file) {
            Ok(_) => panic!("ambiguous v3 batch must be rejected"),
            Err(error) => error,
        };
        assert!(matches!(error, SessionError::InvalidStructure(_)));
        assert_eq!(std::fs::read(&file).unwrap(), original);
    }

    /// 7. assistant 带 tool call 的消息投影为带 tool_calls 的 LLM 消息（Phase 2d 扩展，
    ///    对齐 Pi convertToLlm：assistant 消息携带 tool call 结构进入上下文）。
    #[test]
    fn build_session_context_replays_assistant_tool_calls() {
        let dir = tempfile::tempdir().unwrap();
        let mut manager = SessionManager::create(dir.path(), dir.path()).unwrap();
        manager
            .append_message(AgentMessage {
                role: AgentMessageRole::Assistant,
                content: vec![ContentBlock::ToolCall {
                    id: "call_1".to_string(),
                    name: "write".to_string(),
                    args: serde_json::json!({
                        "path": "hello.txt",
                        "content": "hello",
                    }),
                }],
                provider_reasoning_replay: None,
                tool_call_id: None,
                tool_name: None,
                is_error: None,
                timestamp: None,
            })
            .unwrap();
        manager
            .append_message(tool_result(
                "call_1",
                "Successfully wrote 5 bytes to hello.txt",
            ))
            .unwrap();

        // 落盘重开：字段持久化后投影语义一致。
        let file = manager.path().to_path_buf();
        drop(manager);
        let manager = SessionManager::open(&file).unwrap();
        let ctx = manager.build_session_context().unwrap();
        assert_eq!(ctx.messages.len(), 2);
        assert_eq!(ctx.messages[0].role, ModelRole::Assistant);
        assert_eq!(ctx.messages[0].content, "");
        assert_eq!(ctx.messages[0].tool_calls.len(), 1);
        let call = &ctx.messages[0].tool_calls[0];
        assert_eq!(call.tool_call_id, "call_1");
        assert_eq!(call.tool_name, "write");
        assert_eq!(call.parse_status, ModelToolParseStatus::Valid);
        assert!(call.validation_errors.is_empty());
        // raw_arguments 由 args 重序列化，语义等价。
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&call.raw_arguments).unwrap(),
            serde_json::json!({ "path": "hello.txt", "content": "hello" })
        );
        assert_eq!(ctx.messages[1].role, ModelRole::Tool);
        assert_eq!(ctx.messages[1].tool_call_id.as_deref(), Some("call_1"));

        // 无 tool call 的 assistant 消息保持纯文本投影；缺 call id 时退化纯文本。
        let mut plain = SessionManager::create(dir.path(), dir.path()).unwrap();
        plain.append_message(assistant("hi")).unwrap();
        let ctx = plain.build_session_context().unwrap();
        assert_eq!(ctx.messages[0].role, ModelRole::Assistant);
        assert!(ctx.messages[0].tool_calls.is_empty());
        let mut missing_id = SessionManager::create(dir.path(), dir.path()).unwrap();
        missing_id
            .append_message(AgentMessage {
                role: AgentMessageRole::Assistant,
                content: vec![ContentBlock::Text {
                    text: "text".to_string(),
                }],
                provider_reasoning_replay: None,
                tool_call_id: None,
                tool_name: None,
                is_error: None,
                timestamp: None,
            })
            .unwrap();
        let ctx = missing_id.build_session_context().unwrap();
        assert_eq!(ctx.messages[0].role, ModelRole::Assistant);
        assert!(ctx.messages[0].tool_calls.is_empty());
        assert_eq!(ctx.messages[0].content, "text");
    }

    /// 5. build_context_entries：无 compaction = 全量；有 compaction = 正确切片。
    #[test]
    fn build_context_entries_compaction_slicing() {
        let dir = tempfile::tempdir().unwrap();
        let mut manager = SessionManager::create(dir.path(), dir.path()).unwrap();

        let id_a = manager.append_message(user("a")).unwrap();
        let id_b = manager.append_message(user("b")).unwrap();
        let id_c = manager.append_message(user("c")).unwrap();
        let all = manager.build_context_entries().unwrap();
        assert_eq!(
            entry_ids(&all),
            vec![id_a.clone(), id_b.clone(), id_c.clone()]
        );

        let comp = manager
            .append_compaction(compaction("sum", Some(id_b.clone())))
            .unwrap();
        let id_d = manager.append_message(user("d")).unwrap();
        let ctx = manager.build_context_entries().unwrap();
        assert_eq!(
            entry_ids(&ctx),
            vec![comp.clone(), id_b.clone(), id_c.clone(), id_d.clone()]
        );

        // firstKeptEntryId 边界：= a（含边界本身）。
        let mut other = SessionManager::create(dir.path(), dir.path()).unwrap();
        let a2 = other.append_message(user("a")).unwrap();
        let b2 = other.append_message(user("b")).unwrap();
        let c2 = other.append_message(user("c")).unwrap();
        let comp2 = other
            .append_compaction(compaction("s", Some(a2.clone())))
            .unwrap();
        let d2 = other.append_message(user("d")).unwrap();
        let ctx2 = other.build_context_entries().unwrap();
        assert_eq!(entry_ids(&ctx2), vec![comp2, a2, b2, c2, d2]);
    }

    /// 6. build_session_context：消息顺序/role 转换正确，compaction 摘要包裹，
    ///    model/thinking 从条目提取。
    #[test]
    fn build_session_context_messages_and_settings() {
        let dir = tempfile::tempdir().unwrap();
        let mut manager = SessionManager::create(dir.path(), dir.path()).unwrap();
        manager.append_message(user("hello")).unwrap();
        manager.append_message(assistant("hi")).unwrap();
        manager
            .append_message(tool_result("call_1", "out"))
            .unwrap();

        // 默认：无 model/thinking 条目 → None。
        let ctx = manager.build_session_context().unwrap();
        assert_eq!(ctx.model, None);
        assert_eq!(ctx.thinking_level, None);
        let roles: Vec<ModelRole> = ctx.messages.iter().map(|m| m.role.clone()).collect();
        assert_eq!(
            roles,
            vec![ModelRole::User, ModelRole::Assistant, ModelRole::Tool]
        );
        assert_eq!(ctx.messages[0].content, "hello");
        assert_eq!(ctx.messages[1].content, "hi");
        assert_eq!(ctx.messages[2].content, "out");
        assert_eq!(ctx.messages[2].tool_call_id.as_deref(), Some("call_1"));

        // compaction 条目 → user 文本 + Pi 摘要包裹（firstKept 为 None 时旧条目被摘要取代）。
        manager
            .append_compaction(compaction("earlier stuff", None))
            .unwrap();
        let ctx = manager.build_session_context().unwrap();
        assert_eq!(ctx.messages.len(), 1);
        assert_eq!(ctx.messages[0].role, ModelRole::User);
        assert_eq!(
            ctx.messages[0].content,
            format!("{COMPACTION_SUMMARY_PREFIX}earlier stuff{COMPACTION_SUMMARY_SUFFIX}")
        );
    }

    /// session/read 仓储入口：只返回摘要 + 最近片段，filter/range 有界。
    #[test]
    fn repository_read_is_bounded_and_filtered() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        let cwd = dir.path().join("project");
        let session_id = "64e9177f-ef7e-42af-910d-bd0b94b99230";
        let mut manager = SessionManager::create_with_id(&cwd, &sessions, session_id).unwrap();
        let id1 = manager.append_message(user("one")).unwrap();
        let id2 = manager.append_message(assistant("two")).unwrap();
        manager
            .append_compaction(compaction("summary", Some(id1.clone())))
            .unwrap();
        let id4 = manager.append_message(user("three")).unwrap();
        drop(manager);

        let repository = SessionRepository::new(&sessions);
        let read = repository
            .read(
                session_id,
                &SessionReadOptions {
                    recent_limit: 1,
                    ..SessionReadOptions::default()
                },
            )
            .unwrap();
        assert_eq!(read.summary.as_deref(), Some("summary"));
        assert_eq!(read.total_entries, 4);
        assert_eq!(entry_ids(&read.entries), vec![id4.clone()]);

        let messages = repository
            .read(
                session_id,
                &SessionReadOptions {
                    filter: SessionEntryFilter::Messages,
                    range: Some((1, 3)),
                    recent_limit: 10,
                },
            )
            .unwrap();
        assert_eq!(entry_ids(&messages.entries), vec![id2.clone(), id4.clone()]);
    }

    #[test]
    fn repair_orphaned_tool_calls_appends_synthetic_failed_result_once() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        let cwd = dir.path().join("project");
        let session_id = "2e2c9e30-3f70-4b6c-a1de-46d55e4b9119";
        let mut manager = SessionManager::create_with_id(&cwd, &sessions, session_id).unwrap();
        manager
            .append_message(AgentMessage {
                role: AgentMessageRole::Assistant,
                content: vec![
                    ContentBlock::Text {
                        text: "calling tool".to_string(),
                    },
                    ContentBlock::ToolCall {
                        id: "orphan_call_1".to_string(),
                        name: "bash".to_string(),
                        args: json!({"command": "echo should-not-run"}),
                    },
                ],
                provider_reasoning_replay: None,
                tool_call_id: None,
                tool_name: None,
                is_error: None,
                timestamp: None,
            })
            .unwrap();
        let file = manager.path().to_path_buf();
        drop(manager);

        let mut reopened = SessionManager::open_existing(&file).unwrap();
        assert_eq!(reopened.repair_orphaned_tool_calls().unwrap(), 1);
        drop(reopened);

        let reopened = SessionManager::open_existing(&file).unwrap();
        let entries = reopened.build_context_entries().unwrap();
        let tool_results = entries
            .iter()
            .filter_map(|entry| match &entry.entry_type {
                SessionEntryType::Message(message)
                    if message.role == AgentMessageRole::ToolResult =>
                {
                    Some(message)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(tool_results.len(), 1);
        assert_eq!(
            tool_results[0].tool_call_id.as_deref(),
            Some("orphan_call_1")
        );
        assert!(
            tool_results[0]
                .content_text()
                .contains("previous execution outcome unknown")
        );
        assert!(tool_results[0].content_text().contains("do not retry"));

        // 第二次打开不再追加：幂等。
        let mut reopened = SessionManager::open_existing(&file).unwrap();
        assert_eq!(reopened.repair_orphaned_tool_calls().unwrap(), 0);
    }

    #[test]
    fn build_session_context_model_and_thinking_from_entries() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("settings.jsonl");
        let lines = [
            r#"{"type":"session","version":3,"id":"s1","timestamp":"2024-01-01T00:00:00.000Z","cwd":"C:/work"}"#,
            r#"{"type":"message","id":"aaaa1111","parentId":null,"timestamp":"2024-01-01T00:00:01.000Z","message":{"role":"user","content":"hello"}}"#,
            r#"{"type":"model_change","id":"bbbb2222","parentId":"aaaa1111","timestamp":"2024-01-01T00:00:02.000Z","provider":"openai","modelId":"gpt-4o"}"#,
            r#"{"type":"thinking_level_change","id":"cccc3333","parentId":"bbbb2222","timestamp":"2024-01-01T00:00:03.000Z","thinkingLevel":"high"}"#,
            r#"{"type":"message","id":"dddd4444","parentId":"cccc3333","timestamp":"2024-01-01T00:00:04.000Z","message":{"role":"assistant","content":"reply"}}"#,
        ];
        std::fs::write(&file, lines.join("\n")).unwrap();

        let manager = SessionManager::open(&file).unwrap();
        let ctx = manager.build_session_context().unwrap();
        assert_eq!(ctx.model.as_deref(), Some("openai/gpt-4o"));
        assert_eq!(ctx.thinking_level.as_deref(), Some("high"));
        let roles: Vec<ModelRole> = ctx.messages.iter().map(|m| m.role.clone()).collect();
        assert_eq!(roles, vec![ModelRole::User, ModelRole::Assistant]);
        assert_eq!(ctx.messages[0].content, "hello");
        assert_eq!(ctx.messages[1].content, "reply");
    }

    #[test]
    fn responses_private_replay_round_trips_exactly_through_jsonl() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        let cwd = dir.path().join("project");
        let replay = ProviderReasoningReplay::Responses {
            provider_name: "provider".to_string(),
            model_name: "model".to_string(),
            reasoning_effort: "high".to_string(),
            tool_call_ids: vec!["call_1".to_string()],
            items: vec![
                json!({
                    "type": "reasoning",
                    "id": "rs_1",
                    "encrypted_content": "opaque-secret"
                }),
                json!({
                    "type": "function_call",
                    "call_id": "call_1",
                    "name": "write",
                    "arguments": "{\"path\":\"out.txt\"}"
                }),
            ],
        };
        let mut manager =
            SessionManager::create_with_id(&cwd, &sessions, "f7be8e2a-6f7f-4b55-b1cf-ef6d00c4d8f8")
                .unwrap();
        manager
            .append_message(AgentMessage {
                role: AgentMessageRole::Assistant,
                content: vec![
                    ContentBlock::Thinking {
                        thinking: "summary projection".to_string(),
                        signature: None,
                    },
                    ContentBlock::ToolCall {
                        id: "call_1".to_string(),
                        name: "write".to_string(),
                        args: json!({"path": "out.txt"}),
                    },
                ],
                provider_reasoning_replay: Some(replay.clone()),
                tool_call_id: None,
                tool_name: None,
                is_error: None,
                timestamp: None,
            })
            .unwrap();
        let path = manager.path().to_path_buf();
        drop(manager);

        let reopened = SessionManager::open_existing(&path).unwrap();
        let entries = reopened.build_context_entries().unwrap();
        let message = match &entries[0].entry_type {
            SessionEntryType::Message(message) => message,
            other => panic!("expected assistant message, got {other:?}"),
        };
        assert_eq!(message.provider_reasoning_replay.as_ref(), Some(&replay));
        assert_eq!(message.thinking_blocks().len(), 1);
        let debug = format!("{message:?}");
        assert!(!debug.contains("opaque-secret"));
        assert!(debug.contains("output_item_count"));
        let wire = serde_json::to_value(message).unwrap();
        assert_eq!(
            wire["providerReasoningReplay"]["items"][0]["encrypted_content"],
            "opaque-secret"
        );
        assert_eq!(
            reopened.build_session_context().unwrap().messages[0].content,
            ""
        );
    }

    fn session_header(id: &str) -> String {
        format!(
            r#"{{"type":"session","version":4,"id":"{id}","timestamp":"2024-01-01T00:00:00.000Z","cwd":"C:/work"}}"#
        )
    }

    fn session_message(id: &str, parent: Option<&str>, text: &str) -> String {
        let parent = parent.map_or_else(|| "null".to_string(), |value| format!("\"{value}\""));
        format!(
            r#"{{"type":"message","id":"{id}","parentId":{parent},"timestamp":"2024-01-01T00:00:01.000Z","message":{{"role":"user","content":[{{"type":"text","text":"{text}"}}]}}}}"#
        )
    }

    #[test]
    fn strict_open_rejects_intermediate_malformed_json() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("broken-middle.jsonl");
        let content = format!(
            "{}\n{}\nnot-json\n{}\n",
            session_header("strict-middle"),
            session_message("entry-1", None, "one"),
            session_message("entry-2", Some("entry-1"), "two"),
        );
        std::fs::write(&file, content).unwrap();
        let error = SessionManager::open_existing(&file)
            .err()
            .expect("malformed middle line must be rejected");
        assert!(matches!(error, SessionError::MalformedLine { line: 3, .. }));
    }

    #[test]
    fn strict_open_repairs_torn_tail_and_missing_final_newline() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("torn-tail.jsonl");
        let prefix = format!(
            "{}\n{}\n",
            session_header("strict-tail"),
            session_message("entry-1", None, "one")
        );
        std::fs::write(&file, format!("{prefix}{{\"type\":\"message\",\"id\":\"")).unwrap();
        let opened = SessionManager::open_existing(&file).unwrap();
        assert_eq!(opened.build_context_entries().unwrap().len(), 1);
        assert!(std::fs::read(&file).unwrap().ends_with(b"\n"));
        drop(opened);
        let mut reopened = SessionManager::open_existing(&file).unwrap();
        assert_eq!(reopened.build_context_entries().unwrap().len(), 1);
        reopened.append_message(user("after repair")).unwrap();
        let reopened_again = SessionManager::open_existing(&file).unwrap();
        assert_eq!(reopened_again.build_context_entries().unwrap().len(), 2);

        let missing_newline = dir.path().join("missing-newline.jsonl");
        std::fs::write(
            &missing_newline,
            format!(
                "{}\n{}",
                session_header("strict-newline"),
                session_message("entry-1", None, "one")
            ),
        )
        .unwrap();
        let opened = SessionManager::open_existing(&missing_newline).unwrap();
        assert_eq!(opened.build_context_entries().unwrap().len(), 1);
        assert!(std::fs::read(&missing_newline).unwrap().ends_with(b"\n"));
    }

    #[test]
    fn strict_open_rejects_duplicate_missing_parent_and_cycle() {
        let dir = tempfile::tempdir().unwrap();
        let cases = [
            (
                "duplicate",
                format!(
                    "{}\n{}\n{}\n",
                    session_header("duplicate"),
                    session_message("same", None, "one"),
                    session_message("same", Some("same"), "two")
                ),
                "duplicate",
            ),
            (
                "missing-parent",
                format!(
                    "{}\n{}\n",
                    session_header("missing"),
                    session_message("entry-1", Some("no-parent"), "one")
                ),
                "missing parent",
            ),
            (
                "cycle",
                format!(
                    "{}\n{}\n{}\n",
                    session_header("cycle"),
                    session_message("a", Some("b"), "one"),
                    session_message("b", Some("a"), "two")
                ),
                "cycle",
            ),
        ];
        for (name, content, expected) in cases {
            let file = dir.path().join(format!("{name}.jsonl"));
            std::fs::write(&file, content).unwrap();
            let error = SessionManager::open_existing(&file)
                .err()
                .expect("invalid session structure must be rejected");
            assert!(error.to_string().contains(expected), "{name}: {error}");
        }
    }

    #[test]
    fn strict_open_rejects_complete_invalid_final_json() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("invalid-final.jsonl");
        std::fs::write(&file, format!("{}\n[]", session_header("invalid-final"))).unwrap();
        let error = SessionManager::open_existing(&file)
            .err()
            .expect("complete invalid final entry must be rejected");
        assert!(matches!(error, SessionError::InvalidEntry { line: 2, .. }));
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            format!("{}\n[]", session_header("invalid-final"))
        );
    }

    #[test]
    fn strict_open_rejects_invalid_header_and_known_schema() {
        let dir = tempfile::tempdir().unwrap();
        let missing_version = dir.path().join("missing-version.jsonl");
        std::fs::write(
            &missing_version,
            format!("{}\n", r#"{"type":"session","id":"missing-version"}"#),
        )
        .unwrap();
        let error = SessionManager::open_existing(&missing_version)
            .err()
            .expect("missing header version must be rejected");
        assert!(matches!(error, SessionError::InvalidHeader(_)));

        let malformed_message = dir.path().join("malformed-message.jsonl");
        let content = format!(
            "{}\n{{\"type\":\"message\",\"id\":\"bad\",\"parentId\":null,\"message\":{{\"role\":\"user\",\"content\":\"not-blocks\"}}}}\n",
            session_header("malformed-message")
        );
        std::fs::write(&malformed_message, content).unwrap();
        let error = SessionManager::open_existing(&malformed_message)
            .err()
            .expect("known message schema errors must be rejected");
        assert!(matches!(error, SessionError::InvalidEntry { line: 2, .. }));
    }

    #[test]
    fn append_io_failure_does_not_advance_memory() {
        let dir = tempfile::tempdir().unwrap();
        let mut manager = SessionManager::create(dir.path(), &dir.path().join("sessions")).unwrap();
        let before = manager.build_context_entries().unwrap();
        manager.file = dir.path().to_path_buf();
        assert!(manager.append_message(user("must fail")).is_err());
        assert_eq!(manager.build_context_entries().unwrap(), before);
        assert!(manager.leaf_id().is_empty());
    }

    #[test]
    fn atomic_publish_failure_preserves_original_target() {
        let dir = tempfile::tempdir().unwrap();
        let original = dir.path().join("session.jsonl");
        std::fs::write(&original, b"original\n").unwrap();
        let missing_temporary = dir.path().join("missing.tmp");
        let error = atomic_replace_file(&missing_temporary, &original).unwrap_err();
        assert!(!error.to_string().is_empty());
        assert_eq!(std::fs::read(&original).unwrap(), b"original\n");
    }

    #[test]
    fn metadata_round_trip_is_durable_but_never_enters_model_context() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        let mut manager =
            SessionManager::create_with_id(dir.path(), &sessions, &Uuid::now_v7().to_string())
                .unwrap();
        manager
            .append_metadata(SessionMetadata::turn_started("turn-1"))
            .unwrap();
        manager
            .append_metadata(
                SessionMetadata::thread_settings(
                    "openai_compatible",
                    "gpt-test",
                    Some("medium".to_string()),
                )
                .unwrap(),
            )
            .unwrap();
        manager.append_message(user("visible")).unwrap();
        let reopened = SessionManager::open_existing(manager.path()).unwrap();
        let metadata = reopened.metadata_entries();
        assert_eq!(metadata.len(), 2);
        assert_eq!(metadata[0].kind(), SessionMetadataKind::TurnStarted);
        assert_eq!(metadata[1].field_string("model"), Some("gpt-test"));
        assert!(
            metadata
                .iter()
                .all(|entry| entry.field("parentId").is_none())
        );
        let context = reopened.build_session_context().unwrap();
        assert_eq!(context.messages.len(), 1);
        assert_eq!(context.messages[0].content, "visible");
    }

    #[test]
    fn separate_session_managers_follow_the_latest_durable_leaf() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        let session_id = Uuid::now_v7().to_string();
        let file = sessions.join(format!("{session_id}.jsonl"));
        let mut first = SessionManager::create_with_id(dir.path(), &sessions, &session_id).unwrap();
        first.append_message(user("first")).unwrap();
        let mut second = SessionManager::open_existing(&file).unwrap();
        second
            .append_metadata(SessionMetadata::turn_started("turn-1"))
            .unwrap();
        first.append_message(assistant("second")).unwrap();

        let reopened = SessionManager::open_existing(&file).unwrap();
        let context = reopened.build_session_context().unwrap();
        assert_eq!(context.messages.len(), 2);
        assert_eq!(context.messages[0].content, "first");
        assert_eq!(context.messages[1].content, "second");
        assert_eq!(reopened.metadata_entries().len(), 1);
    }

    #[test]
    fn reopen_interrupted_repair_is_idempotent_and_synthetic() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join("sessions");
        let session_id = Uuid::now_v7().to_string();
        let file = sessions.join(format!("{session_id}.jsonl"));
        let mut manager =
            SessionManager::create_with_id(dir.path(), &sessions, &session_id).unwrap();
        manager
            .append_metadata(SessionMetadata::turn_started("turn-1"))
            .unwrap();
        drop(manager);

        let mut reopened = SessionManager::open_existing(&file).unwrap();
        assert_eq!(reopened.repair_interrupted_turns().unwrap(), 1);
        drop(reopened);
        let mut reopened_again = SessionManager::open_existing(&file).unwrap();
        assert_eq!(reopened_again.repair_interrupted_turns().unwrap(), 0);
        let metadata = reopened_again.metadata_entries();
        assert_eq!(metadata.len(), 2);
        assert_eq!(metadata[1].kind(), SessionMetadataKind::TurnInterrupted);
        assert!(metadata[1].synthetic());
    }

    #[test]
    fn thread_settings_reject_sensitive_fields() {
        let mut fields = Map::new();
        fields.insert("provider".to_string(), json!("openai_compatible"));
        fields.insert("model".to_string(), json!("gpt-test"));
        fields.insert("apiKey".to_string(), json!("do-not-persist"));
        let error = SessionMetadata::new(SessionMetadataKind::ThreadSettings, fields).unwrap_err();
        assert!(error.to_string().contains("sensitive"));
    }
}
