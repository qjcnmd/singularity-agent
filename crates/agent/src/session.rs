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
//! - 消息 `content` 为字符串：读取真实 Pi 文件中 content block 数组的消息时，该条目
//!   原样保留（`Other`）但不进入 LLM 上下文；assistant 消息的 provider/model 因此
//!   也无法参与 model 提取（Pi 从 assistant 消息读 provider/model，我们只从
//!   `model_change` 条目提取）。
//! - `append_message` 立即写盘（Pi 延迟到首个 assistant 消息才建文件，规格明确要求
//!   立即写盘）。
//! - 路径遍历带防环保护（Pi 对损坏的 parentId 环会死循环）。

use std::collections::{HashMap, HashSet};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

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
    COMPACTION_SUMMARY_PREFIX, COMPACTION_SUMMARY_SUFFIX, LlmMessage,
};

/// 当前会话文件格式版本，对齐 Pi `CURRENT_SESSION_VERSION`。
pub const CURRENT_SESSION_VERSION: u32 = 3;

/// 会话读写错误。
#[derive(Debug, Error)]
pub enum SessionError {
    #[error("session io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("session json error: {0}")]
    Json(#[from] serde_json::Error),
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

/// 会话条目类型。`Other` 保留未建模条目（label/session_info/custom_message/
/// branch_summary 及未知类型）的原始 JSON，保证迁移重写不丢数据。
#[derive(Debug, Clone, PartialEq)]
pub enum SessionEntryType {
    Message(AgentMessage),
    Compaction(CompactionEntry),
    Custom(CustomEntry),
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

/// 已知类型 payload 解析失败时降级为 `Other`（原样保留，Pi 对 payload 深度校验同样宽松）。
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
    /// 会话 id（header 字段与文件名来源）。Phase 2a 无读取方，契约保留。
    #[allow(dead_code)]
    session_id: String,
}

impl SessionManager {
    /// 新建会话：生成 Pi 风格文件名 `<ts>_<uuid>.jsonl` 并立即写入 header。
    ///
    /// header：`{"type":"session","version":3,"id":...,"timestamp":...,"cwd":...}`，
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
        let mut handle = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&file)?;
        writeln!(handle, "{}", serde_json::to_string(&header)?)?;
        handle.flush()?;
        Ok(Self {
            file,
            cwd,
            entries: Vec::new(),
            by_id: HashMap::new(),
            leaf_id: None,
            session_id,
        })
    }

    /// 打开会话文件：逐行解析，v1/v2 文件按 Pi `session-manager.js` 迁移逻辑升级为 v3，
    /// 发生迁移时重写文件（Pi `_setSessionFile` 行为）。
    ///
    /// 文件不存在或为空时按 Pi 语义创建新会话并写入 header；非空但无法解析为合法
    /// pi session 时报错。
    pub fn open(path: &Path) -> Result<Self> {
        let file = path.to_path_buf();
        let content = match std::fs::read_to_string(&file) {
            Ok(content) => content,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Self::create_empty_at(&file);
            }
            Err(error) => return Err(error.into()),
        };
        let mut raw_entries = parse_lines(&content);
        if raw_entries.is_empty() {
            if content.is_empty() {
                return Self::create_empty_at(&file);
            }
            return Err(SessionError::InvalidSession(format!(
                "Session file is not a valid pi session: {}",
                file.display()
            )));
        }
        let header = &raw_entries[0];
        if header.get("type").and_then(Value::as_str) != Some("session")
            || header.get("id").and_then(Value::as_str).is_none()
        {
            return Err(SessionError::InvalidSession(format!(
                "Session file is not a valid pi session: {}",
                file.display()
            )));
        }
        let version = header.get("version").and_then(Value::as_u64).unwrap_or(1) as u32;
        // 迁移会原地修改 raw_entries（含 header 的 version 字段），先取出不变的字段。
        let session_id = header
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let header_cwd = header
            .get("cwd")
            .and_then(Value::as_str)
            .map(str::to_string);
        if version < CURRENT_SESSION_VERSION {
            migrate_entries(&mut raw_entries, version);
            rewrite_file(&file, &raw_entries)?;
        }
        let cwd = header_cwd
            .map(|cwd| normalize_abs_path(Path::new(&cwd)))
            .transpose()?
            .unwrap_or(std::env::current_dir()?);
        let mut entries = Vec::new();
        let mut by_id = HashMap::new();
        let mut leaf_id: Option<String> = None;
        for raw in raw_entries.into_iter().skip(1) {
            // Pi 的 getEntries() 过滤 header；中段 session 行同样不属于树。
            if raw.get("type").and_then(Value::as_str) == Some("session") {
                continue;
            }
            let Ok(entry) = SessionEntry::from_value(&raw) else {
                // 迁移后仍无法解析的行跳过（Pi 对垃圾行同样容忍）。
                continue;
            };
            let id = entry.id.clone();
            by_id.insert(id.clone(), entries.len());
            leaf_id = Some(id);
            entries.push(entry);
        }
        Ok(Self {
            file,
            cwd,
            entries,
            by_id,
            leaf_id,
            session_id,
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
        Ok(Self {
            file: file.to_path_buf(),
            cwd,
            entries: Vec::new(),
            by_id: HashMap::new(),
            leaf_id: None,
            session_id,
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

    fn append_entry(&mut self, entry_type: SessionEntryType) -> Result<String> {
        let id = generate_id(|candidate| self.by_id.contains_key(candidate));
        let entry = SessionEntry {
            id: id.clone(),
            parent_id: self.leaf_id.clone().unwrap_or_default(),
            timestamp: Some(now_iso()),
            entry_type,
        };
        // Pi appendFileSync 语义：append + write + flush，无事务、无锁。
        let mut handle = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.file)?;
        writeln!(handle, "{}", serde_json::to_string(&entry)?)?;
        handle.flush()?;
        self.by_id.insert(id.clone(), self.entries.len());
        self.entries.push(entry);
        self.leaf_id = Some(id.clone());
        Ok(id)
    }

    /// 将 leaf 指针移动到指定条目；下次追加将成为该条目的子条目（Pi `branch`）。
    /// 已有条目不被修改或删除。
    pub fn branch(&mut self, at_entry_id: &str) -> Result<()> {
        if !self.by_id.contains_key(at_entry_id) {
            return Err(SessionError::EntryNotFound(at_entry_id.to_string()));
        }
        self.leaf_id = Some(at_entry_id.to_string());
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
/// `convertToLlm` 的 Phase 2a 简化）。
fn entry_to_llm_messages(entry: &SessionEntry) -> Vec<LlmMessage> {
    match &entry.entry_type {
        SessionEntryType::Message(message) => match message.role {
            AgentMessageRole::User => {
                vec![ModelMessage::text(ModelRole::User, message.content.clone())]
            }
            AgentMessageRole::Assistant => {
                // 带工具调用的 assistant 消息必须重放 tool_calls（对齐 Pi convertToLlm：
                // assistant 消息原样进入 LLM 上下文）。真实 provider 拒绝孤立 tool_call_id
                // 的 tool 消息（无对应 assistant tool_calls 的历史，实测 HTTP 400）。
                // raw_arguments 由 args 重新序列化（Phase 2a 会话 schema 不存原始文本）。
                let (Some(tool_name), Some(args)) = (&message.tool_name, &message.args) else {
                    return vec![ModelMessage::text(
                        ModelRole::Assistant,
                        message.content.clone(),
                    )];
                };
                let Some(tool_call_id) = &message.tool_call_id else {
                    // 缺 call id 无法构造合法工具调用历史；退化为纯文本，不伪造 id。
                    return vec![ModelMessage::text(
                        ModelRole::Assistant,
                        message.content.clone(),
                    )];
                };
                let mut llm = ModelMessage::assistant_tool_calls(vec![ModelToolCall {
                    tool_call_id: tool_call_id.clone(),
                    tool_name: tool_name.clone(),
                    arguments: args.clone(),
                    raw_arguments: serde_json::to_string(args).unwrap_or_default(),
                    parse_status: ModelToolParseStatus::Valid,
                    validation_errors: Vec::new(),
                }]);
                llm.content = message.content.clone();
                vec![llm]
            }
            AgentMessageRole::ToolResult => {
                let mut llm = ModelMessage::text(ModelRole::Tool, message.content.clone());
                llm.tool_call_id = message.tool_call_id.clone();
                vec![llm]
            }
            // Pi 用 bashExecutionToText 拼接 command/output；Phase 2a 消息模型只有
            // 单个 content 字符串，直接作为 user 文本。
            AgentMessageRole::BashExecution | AgentMessageRole::Custom => {
                vec![ModelMessage::text(ModelRole::User, message.content.clone())]
            }
            AgentMessageRole::BranchSummary => vec![ModelMessage::text(
                ModelRole::User,
                format!(
                    "{BRANCH_SUMMARY_PREFIX}{}{BRANCH_SUMMARY_SUFFIX}",
                    message.content
                ),
            )],
            AgentMessageRole::CompactionSummary => vec![ModelMessage::text(
                ModelRole::User,
                format!(
                    "{COMPACTION_SUMMARY_PREFIX}{}{COMPACTION_SUMMARY_SUFFIX}",
                    message.content
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
        | SessionEntryType::ModelChange { .. }
        | SessionEntryType::ThinkingLevelChange { .. }
        | SessionEntryType::Other(_) => Vec::new(),
    }
}

/// 逐行解析：跳过空行、JSON 解析失败与非对象行（Pi `parseSessionEntryLine`）。
fn parse_lines(content: &str) -> Vec<Value> {
    content
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(Value::is_object)
        .collect()
}

/// v1/v2→v3 迁移（Pi `migrateV1ToV2`/`migrateV2ToV3`，原地修改，仅 version < 3 时调用）。
fn migrate_entries(entries: &mut [Value], version: u32) {
    if version < 2 {
        migrate_v1_to_v2(entries);
    }
    if version < 3 {
        migrate_v2_to_v3(entries);
    }
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

/// 迁移后重写整个文件（Pi `_rewriteFile`）。
fn rewrite_file(file: &Path, entries: &[Value]) -> Result<()> {
    let mut handle = OpenOptions::new().write(true).truncate(true).open(file)?;
    for entry in entries {
        writeln!(handle, "{}", serde_json::to_string(entry)?)?;
    }
    handle.flush()?;
    Ok(())
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
    use singularity_model::ModelRole;

    fn user(text: &str) -> AgentMessage {
        AgentMessage {
            role: AgentMessageRole::User,
            content: text.to_string(),
            tool_call_id: None,
            tool_name: None,
            args: None,
            timestamp: Some(1_700_000_000_000),
        }
    }

    fn assistant(text: &str) -> AgentMessage {
        AgentMessage {
            role: AgentMessageRole::Assistant,
            content: text.to_string(),
            tool_call_id: None,
            tool_name: None,
            args: None,
            timestamp: Some(1_700_000_000_001),
        }
    }

    fn tool_result(call_id: &str, text: &str) -> AgentMessage {
        AgentMessage {
            role: AgentMessageRole::ToolResult,
            content: text.to_string(),
            tool_call_id: Some(call_id.to_string()),
            tool_name: Some("bash".to_string()),
            args: None,
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
        assert_eq!(first_line["version"], 3);
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
            SessionEntryType::Message(m) if m.role == AgentMessageRole::User && m.content == "hello"));
        assert!(matches!(&entries[1].entry_type,
            SessionEntryType::Message(m) if m.role == AgentMessageRole::Assistant && m.content == "hi there"));
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
    fn open_migrates_v1_file_to_v3() {
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
            SessionEntryType::Message(m) if m.role == AgentMessageRole::User && m.content == "a"));
        assert!(matches!(&entries[2].entry_type,
            SessionEntryType::Message(m) if m.role == AgentMessageRole::Assistant && m.content == "b"));
        assert!(matches!(&entries[3].entry_type,
            SessionEntryType::Message(m) if m.role == AgentMessageRole::User && m.content == "c"));
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
        // 文件已重写为 v3：header version 3，条目带 id/parentId，索引已转 id。
        let rewritten: Vec<Value> = std::fs::read_to_string(&file)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect();
        assert_eq!(rewritten.len(), 6);
        assert_eq!(rewritten[0]["version"], 3);
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
    fn open_migrates_v2_hook_message_role() {
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
            SessionEntryType::Message(m) if m.role == AgentMessageRole::Custom && m.content == "injected"));
        // 文件已重写为 v3，hookMessage → custom。
        let rewritten: Vec<Value> = std::fs::read_to_string(&file)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect();
        assert_eq!(rewritten[0]["version"], 3);
        assert_eq!(rewritten[2]["message"]["role"], "custom");
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
                content: String::new(),
                tool_call_id: Some("call_1".to_string()),
                tool_name: Some("write".to_string()),
                args: Some(serde_json::json!({
                    "path": "hello.txt",
                    "content": "hello",
                })),
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
                content: "text".to_string(),
                tool_call_id: None,
                tool_name: Some("write".to_string()),
                args: Some(serde_json::json!({})),
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
}
