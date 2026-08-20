//! Mutable Session lifecycle and append manager.

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::json;
use uuid::Uuid;

use crate::message::AgentMessage;

use super::file::{
    AppendLimits, DEFAULT_APPEND_LIMITS, MAX_SESSION_FILE_BYTES, SessionFileState, TailRepair,
    append_lock_for, generate_id, lock_append, normalize_abs_path, normalize_cwd_string, now_iso,
    parse_session_lines, parse_session_tail, rewrite_file, validate_append_limits,
};
use super::format::{
    CURRENT_SESSION_VERSION, CompactionEntry, Result, SessionEntry, SessionEntryType, SessionError,
    SessionMetadata, validate_entries, validate_header,
};
/// JSONL 会话管理器。
pub struct SessionManager {
    pub(super) file: PathBuf,
    pub(super) cwd: PathBuf,
    pub(super) entries: Vec<SessionEntry>,
    pub(super) by_id: HashMap<String, usize>,
    pub(super) leaf_id: Option<String>,
    pub(super) session_id: String,
    pub(super) header_timestamp: String,
    pub(super) append_lock: Arc<Mutex<()>>,
    pub(super) file_state: SessionFileState,
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
        let file_state = SessionFileState::capture(&file)?;
        Ok(Self {
            file,
            cwd,
            entries: Vec::new(),
            by_id: HashMap::new(),
            leaf_id: None,
            session_id,
            header_timestamp: timestamp,
            append_lock,
            file_state,
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
        let file_state = SessionFileState::capture(&file)?;
        Ok(Self {
            file,
            cwd,
            entries,
            by_id,
            leaf_id,
            session_id,
            header_timestamp,
            append_lock,
            file_state,
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
        let file_state = SessionFileState::capture(&file)?;
        Ok(Self {
            file,
            cwd,
            entries,
            by_id,
            leaf_id,
            session_id,
            header_timestamp,
            append_lock,
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
        let append_lock = append_lock_for(file);
        let file_state = SessionFileState::capture(file)?;
        Ok(Self {
            file: file.to_path_buf(),
            cwd,
            entries: Vec::new(),
            by_id: HashMap::new(),
            leaf_id: None,
            session_id,
            header_timestamp: timestamp,
            append_lock,
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
        let (id, entry, file_state) = {
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
            let file_state = SessionFileState::capture(&self.file)?;
            (id, entry, file_state)
        };
        self.by_id.insert(id.clone(), self.entries.len());
        self.entries.push(entry);
        self.leaf_id = Some(id.clone());
        self.file_state = file_state;
        Ok(id)
    }

    fn refresh_from_disk_locked(&mut self) -> Result<()> {
        let current_state = SessionFileState::capture(&self.file)?;
        if current_state == self.file_state {
            return Ok(());
        }
        if current_state.len > self.file_state.len
            && current_state.len <= MAX_SESSION_FILE_BYTES as u64
            && current_state.identity == self.file_state.identity
            && current_state.header == self.file_state.header
            && let Ok(tail) =
                parse_session_tail(&self.file, self.file_state.len, self.entries.len())
        {
            let mut next_parent = self.leaf_id.as_deref().unwrap_or("").to_string();
            let valid = tail.iter().all(|entry| {
                let is_valid =
                    !self.by_id.contains_key(&entry.id) && entry.parent_id == next_parent;
                if is_valid {
                    next_parent = entry.id.clone();
                }
                is_valid
            });
            if valid {
                for entry in tail {
                    let index = self.entries.len();
                    self.by_id.insert(entry.id.clone(), index);
                    self.leaf_id = Some(entry.id.clone());
                    self.entries.push(entry);
                }
                self.file_state = current_state;
                return Ok(());
            }
        }
        let refreshed = Self::open_unlocked(&self.file)?;
        if refreshed.session_id != self.session_id {
            return Err(SessionError::InvalidSession(format!(
                "session header id {} does not match current session {}",
                refreshed.session_id, self.session_id
            )));
        }
        self.cwd = refreshed.cwd;
        self.header_timestamp = refreshed.header_timestamp;
        self.entries = refreshed.entries;
        self.by_id = refreshed.by_id;
        self.leaf_id = refreshed.leaf_id;
        self.file_state = refreshed.file_state;
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
}
