//! 可变会话生命周期与追加管理器。

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::json;
use uuid::Uuid;

use crate::message::AgentMessage;

use super::file::{
    AppendLimits, DEFAULT_APPEND_LIMITS, SessionFileState, TailRepair, generate_id,
    normalize_abs_path, normalize_cwd_string, now_iso, parse_session_lines, rewrite_file,
    validate_append_limits,
};
use super::format::{
    CURRENT_SESSION_VERSION, CompactionEntry, Result, SessionEntry, SessionEntryType, SessionError,
    SessionMetadata, validate_entries, validate_header,
};
/// JSONL 会话管理器。会话是严格的线性序列，`entries` 的物理顺序即事实源顺序；
/// 会话由单个写者在整轮 turn 内独占持有，因此 append 不需要跨写者协调（同一
/// 会话同一时刻至多一个存活写者，由 AppServer 的 activate_turn 保证）。
pub struct SessionManager {
    pub(super) file: PathBuf,
    pub(super) cwd: PathBuf,
    pub(super) entries: Vec<SessionEntry>,
    pub(super) session_id: String,
    pub(super) header_timestamp: String,
    pub(super) file_state: SessionFileState,
}

impl std::fmt::Debug for SessionManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionManager")
            .field("file", &self.file)
            .field("cwd", &self.cwd)
            .field("session_id", &self.session_id)
            .field("header_timestamp", &self.header_timestamp)
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

    /// 为有界 discovery/index 重建打开既有 rollout。
    ///
    /// 此接缝在持有正常追加锁时校验完整文件，但绝不截断 torn tail 或
    /// 补末尾换行；需要正常重开修复路径的文件被拒绝。
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
        let file_state = SessionFileState::capture(&file)?;
        Ok(Self {
            file,
            cwd,
            entries: Vec::new(),
            session_id,
            header_timestamp: timestamp,
            file_state,
        })
    }

    /// 打开会话文件：严格逐行解析。
    pub fn open(path: &Path) -> Result<Self> {
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
        let file_state = SessionFileState::capture(&file)?;
        Ok(Self {
            file,
            cwd,
            entries,
            session_id,
            header_timestamp,
            file_state,
        })
    }

    fn open_read_only(path: &Path) -> Result<Self> {
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
        let file_state = SessionFileState::capture(&file)?;
        Ok(Self {
            file,
            cwd,
            entries,
            session_id,
            header_timestamp,
            file_state,
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
        let file_state = SessionFileState::capture(file)?;
        Ok(Self {
            file: file.to_path_buf(),
            cwd,
            entries: Vec::new(),
            session_id,
            header_timestamp: timestamp,
            file_state,
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
        let metadata = metadata.validate()?;
        self.append_entry(SessionEntryType::Metadata(metadata))
    }

    pub(super) fn append_entry(&mut self, entry_type: SessionEntryType) -> Result<String> {
        self.append_entry_with_limits(entry_type, DEFAULT_APPEND_LIMITS)
    }

    pub(super) fn append_entry_with_limits(
        &mut self,
        entry_type: SessionEntryType,
        limits: AppendLimits,
    ) -> Result<String> {
        let id = generate_id(|candidate| self.entries.iter().any(|entry| entry.id == candidate));
        let entry = SessionEntry {
            id: id.clone(),
            timestamp: Some(now_iso()),
            entry_type,
        };
        let serialized = serde_json::to_string(&entry)?;
        // 单写者语义：内存 entries 与 file_state 是唯一权威，append 前无需再
        // 读盘核对；limits 直接基于内存态的长度/条数判定。
        validate_append_limits(
            self.file_state.len,
            self.entries.len(),
            serialized.len(),
            limits,
        )?;
        let mut handle = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.file)?;
        let bytes_to_write = serialized.as_bytes();
        let total_written = (bytes_to_write.len() + 1) as u64;
        handle.write_all(bytes_to_write)?;
        handle.write_all(b"\n")?;
        handle.flush()?;
        self.file_state.len += total_written;
        self.entries.push(entry);
        Ok(id)
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// header 时间戳是索引重建的权威创建事实。
    pub fn created_at(&self) -> &str {
        &self.header_timestamp
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
}
