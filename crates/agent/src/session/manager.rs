//! 可变会话生命周期与追加管理器。

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::json;
use uuid::Uuid;

use crate::message::AgentMessage;

use super::file::{
    AppendLimits, DEFAULT_APPEND_LIMITS, SessionFileState, TailRepair, generate_id,
    normalize_cwd_string, now_iso, parse_session_lines, rewrite_file, validate_append_limits,
};
use super::format::{
    CURRENT_SESSION_VERSION, CompactionEntry, Result, SessionEntry, SessionError, SessionMetadata,
    validate_entries, validate_header,
};
use super::writer_lock::{WriterLockCoordinator, WriterLockGuard};

/// JSONL 会话管理器。会话是严格的线性序列，`entries` 的物理顺序即事实源顺序；
/// 会话由单个写者在整轮 turn 内独占持有（由 OS 文件锁跨进程强制执行），因此
/// append 不需要跨写者协调——同一会话同一时刻至多一个存活写者。
pub struct SessionManager {
    pub(super) file: PathBuf,
    pub(super) cwd: PathBuf,
    pub(super) entries: Vec<SessionEntry>,
    pub(super) session_id: String,
    pub(super) header_timestamp: String,
    pub(super) file_state: SessionFileState,
    /// 写者锁守卫：随实例释放并清理锁文件；`None` 表示只读打开。
    _writer_lock: Option<WriterLockGuard>,
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
        let coordinator = Arc::new(WriterLockCoordinator::new(sessions_dir));
        Self::create_with_file(
            cwd,
            sessions_dir,
            format!("{session_id}.jsonl"),
            session_id,
            timestamp,
            &coordinator,
        )
    }

    /// 新建会话：文件名与 header id 都是调用方指定的 UUID。
    pub fn create_with_id(cwd: &Path, sessions_dir: &Path, session_id: &str) -> Result<Self> {
        let coordinator = Arc::new(WriterLockCoordinator::new(sessions_dir));
        Self::create_with_id_with_coordinator(cwd, sessions_dir, session_id, &coordinator)
    }

    /// 与 [`create_with_id`] 相同，但使用调用方持有的长驻协调器（进程级
    /// stale 清理只发生一次；见 [`open_existing_with_coordinator`]）。
    pub fn create_with_id_with_coordinator(
        cwd: &Path,
        sessions_dir: &Path,
        session_id: &str,
        coordinator: &Arc<WriterLockCoordinator>,
    ) -> Result<Self> {
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
            coordinator,
        )
    }

    /// 打开必须已存在的会话文件；缺失或损坏直接报错，不静默创建新会话。
    /// 打开时获取该会话的 OS 写者锁（文件名 stem 为锁键），解锁前其他写者
    /// 被拒绝。修复重写与后续 append 全程持锁。
    pub fn open_existing(path: &Path) -> Result<Self> {
        let sessions_dir = path.parent().ok_or_else(|| {
            SessionError::InvalidSession(format!(
                "session file has no parent directory: {}",
                path.display()
            ))
        })?;
        let coordinator = Arc::new(WriterLockCoordinator::new(sessions_dir));
        Self::open_existing_with_coordinator(path, &coordinator)
    }

    /// 与 [`open_existing`] 相同，但使用调用方持有的长驻协调器。
    ///
    /// 协调器承担进程级的一次性 stale 清理；进程内应只存在一个实例
    /// （runtime 的 TurnRunner 持有），避免每个打开路径各自触发清理。
    pub fn open_existing_with_coordinator(
        path: &Path,
        coordinator: &Arc<WriterLockCoordinator>,
    ) -> Result<Self> {
        if !path.is_file() {
            return Err(SessionError::InvalidSession(format!(
                "session file does not exist: {}",
                path.display()
            )));
        }
        let file = path.to_path_buf();
        let lock_key = path.file_stem().and_then(|s| s.to_str()).ok_or_else(|| {
            SessionError::InvalidSession(format!(
                "session file name is not valid UTF-8: {}",
                path.display()
            ))
        })?;
        let writer_lock = coordinator.acquire(lock_key)?;
        let opened = Self::open_parsed(&file, TailPolicy::RepairAndRewrite)
            .map(|opened| opened.with_lock(writer_lock))?;
        Ok(opened)
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
        Self::open_parsed(path, TailPolicy::RejectOnRepair)
    }

    /// 共用的打开路径：解析、结构校验与状态捕获。修复策略只影响 torn
    /// tail 的处理（重写或拒绝），其余语义在两条路径间保持一致。
    fn open_parsed(path: &Path, tail_policy: TailPolicy) -> Result<Self> {
        let file = path.to_path_buf();
        let metadata = std::fs::symlink_metadata(&file)?;
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
            match tail_policy {
                TailPolicy::RepairAndRewrite => rewrite_file(&file, &parsed.entries)?,
                TailPolicy::RejectOnRepair => {
                    return Err(SessionError::InvalidSession(
                        "read-only session scan rejected a rollout requiring tail repair"
                            .to_string(),
                    ));
                }
            }
        }
        let cwd = header_cwd
            .map(|cwd| std::path::absolute(Path::new(&cwd)))
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
            _writer_lock: None,
        })
    }

    /// 为已解析会话挂上写者锁守卫（只读路径不调用）。
    fn with_lock(mut self, writer_lock: WriterLockGuard) -> Self {
        self._writer_lock = Some(writer_lock);
        self
    }

    /// 共用的新建会话实现：先取写者锁，再写入 header 并打开新文件。
    fn create_with_file(
        cwd: &Path,
        sessions_dir: &Path,
        file_name: String,
        session_id: String,
        timestamp: String,
        coordinator: &Arc<WriterLockCoordinator>,
    ) -> Result<Self> {
        let cwd = std::path::absolute(cwd)?;
        std::fs::create_dir_all(sessions_dir)?;
        // 锁先于文件：会话文件一旦出现就受单写者保护。
        let writer_lock = coordinator.acquire(&session_id)?;
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
            _writer_lock: Some(writer_lock),
        })
    }

    /// 追加消息为当前 leaf 的子条目并推进 leaf，立即写盘。返回新条目 id。
    pub fn append_message(&mut self, message: AgentMessage) -> Result<String> {
        self.append_entry(SessionEntry::Message {
            id: generate_id(|candidate| self.entries.iter().any(|entry| entry.id() == candidate)),
            timestamp: Some(now_iso()),
            message,
        })
    }

    /// 追加 compaction 条目为当前 leaf 的子条目并推进 leaf，立即写盘。返回新条目 id。
    pub fn append_compaction(&mut self, entry: CompactionEntry) -> Result<String> {
        self.append_entry(SessionEntry::Compaction {
            id: generate_id(|candidate| self.entries.iter().any(|entry| entry.id() == candidate)),
            timestamp: Some(now_iso()),
            compaction: entry,
        })
    }

    /// 追加不进入模型上下文的 metadata。
    pub fn append_metadata(&mut self, metadata: SessionMetadata) -> Result<String> {
        let metadata = metadata.validate()?;
        self.append_entry(SessionEntry::Metadata {
            id: generate_id(|candidate| self.entries.iter().any(|entry| entry.id() == candidate)),
            timestamp: Some(now_iso()),
            metadata,
        })
    }

    pub(super) fn append_entry(&mut self, entry: SessionEntry) -> Result<String> {
        self.append_entry_with_limits(entry, DEFAULT_APPEND_LIMITS)
    }

    pub(super) fn append_entry_with_limits(
        &mut self,
        entry: SessionEntry,
        limits: AppendLimits,
    ) -> Result<String> {
        let id = entry.id().to_string();
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

    /// 校验会话头部 id 与请求一致；不一致属于损坏状态。
    pub fn verify_session_id(&self, expected: &str) -> Result<()> {
        let actual = self.session_id();
        if actual == expected {
            Ok(())
        } else {
            Err(SessionError::InvalidHeader(format!(
                "rollout header id {actual} does not match expected id {expected}"
            )))
        }
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

/// 尾部修复策略：正常打开修复重写，只读扫描拒绝。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TailPolicy {
    RepairAndRewrite,
    RejectOnRepair,
}
