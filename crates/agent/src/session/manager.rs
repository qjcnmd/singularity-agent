//! 可变会话生命周期与追加管理器。

use std::fs::OpenOptions;
use std::io::Write;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::json;
use uuid::Uuid;

use crate::message::AgentMessage;

use super::file::{
    AppendLimits, DEFAULT_APPEND_LIMITS, generate_id, now_iso, parse_session_lines, rewrite_file,
    validate_append_limits,
};
use super::format::{
    CURRENT_SESSION_VERSION, CompactionEntry, LedgerRecord, Result, SessionEntry, SessionError,
    SessionMetadata, validate_entries, validate_header,
};
use super::writer_lock::{WriterLockCoordinator, WriterLockGuard};

/// 既有会话的打开意图：调用方必须声明打开后要做什么，锁语义与修复行为
/// 由此单点决定，而不是散布在调用方的后续编排里。
pub enum SessionAccess {
    /// 持锁打开，校验头部 id 一致性并修复中断 turn 与孤立工具调用
    /// （turn 执行与 resume 前的写修复路径）。
    RepairWrite,
    /// 持锁打开并校验头部 id 一致性，随后追加或移动，不做修复重写。
    Append,
}

/// JSONL 会话管理器。会话是严格的线性序列，entries 的物理顺序即事实源顺序；
/// 会话由单个写者在整轮 turn 内独占持有（由 OS 文件锁跨进程强制执行），因此
/// append 不需要跨写者协调——同一会话同一时刻至多一个存活写者。
pub struct SessionManager {
    pub(super) data: SessionData,
    writer_lock: WriterLockGuard,
}

/// 已解析的会话事实。只读扫描与持锁写者共用解析、索引和投影，写入能力仅属于
/// SessionManager；读取已有会话不会获取写者锁或修复文件。
///
/// ```compile_fail
/// use singularity_agent::session::{SessionData, SessionMetadata};
/// fn append_without_a_writer(mut session: SessionData) {
///     session.append_metadata(SessionMetadata::thread_name("renamed")).unwrap();
/// }
/// ```
pub struct SessionData {
    pub(super) file: PathBuf,
    pub(super) cwd: PathBuf,
    pub(super) cwd_display: String,
    pub(super) entries: Vec<SessionEntry>,
    pub(super) session_id: String,
    pub(super) header_timestamp: String,
    /// 解析或最后一次追加时的文件长度。
    pub(super) file_len: u64,
    request_index: super::request::RequestIndex,
}

impl std::fmt::Debug for SessionManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionManager")
            .field("data", &self.data)
            .finish()
    }
}

impl Deref for SessionManager {
    type Target = SessionData;

    fn deref(&self) -> &Self::Target {
        &self.data
    }
}

impl std::fmt::Debug for SessionData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionData")
            .field("file", &self.file)
            .field("cwd", &self.cwd)
            .field("session_id", &self.session_id)
            .field("header_timestamp", &self.header_timestamp)
            .field("entries_count", &self.entries.len())
            .finish()
    }
}

impl SessionManager {
    /// 测试便利构造器共用的协调器构造方式；并行测试仍各自持 per-tempdir
    /// 协调器，共享的是构造方式而不是实例。
    #[cfg(any(test, feature = "test-support"))]
    fn coordinator_for_tests(sessions_dir: &Path) -> Arc<WriterLockCoordinator> {
        Arc::new(WriterLockCoordinator::new(sessions_dir))
    }

    /// 新建会话：生成 UUID 并创建文件（测试便利入口）。
    #[cfg(any(test, feature = "test-support"))]
    pub fn create(cwd: &Path, sessions_dir: &Path) -> Result<Self> {
        let session_id = Uuid::now_v7().to_string();
        let timestamp = now_iso();
        Self::create_with_file(
            cwd,
            sessions_dir,
            format!("{session_id}.jsonl"),
            session_id,
            timestamp,
            &Self::coordinator_for_tests(sessions_dir),
        )
    }

    /// 测试便利入口：自带临时协调器 Self::create_with_id_with_coordinator。
    #[cfg(any(test, feature = "test-support"))]
    pub fn create_with_id(cwd: &Path, sessions_dir: &Path, session_id: &str) -> Result<Self> {
        Self::create_with_id_with_coordinator(
            cwd,
            sessions_dir,
            session_id,
            &Self::coordinator_for_tests(sessions_dir),
        )
    }

    /// 新建会话：文件名与 header id 都是调用方指定的 UUID，写者锁走调用方
    /// 持有的长驻协调器，统一锁目录与本进程活动回合投影。
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
    /// 被拒绝。修复重写与后续 append 全程持锁（测试便利入口）。
    #[cfg(any(test, feature = "test-support"))]
    pub fn open_existing(path: &Path) -> Result<Self> {
        let sessions_dir = path.parent().ok_or_else(|| {
            SessionError::InvalidSession(format!(
                "session file has no parent directory: {}",
                path.display()
            ))
        })?;
        Self::open_existing_with_coordinator(path, &Self::coordinator_for_tests(sessions_dir))
    }

    /// 按声明意图打开既有会话并使用调用方持有的长驻协调器。
    ///
    /// 两条路径都先校验文件头部 id 与 expected_id 一致（不一致属于损坏
    /// 状态）；协调器由 runtime 的 TurnRunner 持有，共享本进程活动回合投影。
    pub fn open_existing_with_access(
        path: &Path,
        coordinator: &Arc<WriterLockCoordinator>,
        expected_id: &str,
        access: SessionAccess,
    ) -> Result<Self> {
        let mut session = Self::open_existing_with_coordinator(path, coordinator)?;
        session.verify_session_id(expected_id)?;
        if matches!(access, SessionAccess::RepairWrite) {
            session.repair_interrupted_operations()?;
        }
        super::context::ContextView::validate(&session)?;
        Ok(session)
    }

    /// 打开既有会话并使用调用方持有的长驻协调器。
    ///
    /// 协调器由 runtime 的 TurnRunner 持有，共享本进程活动回合投影。
    fn open_existing_with_coordinator(
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
        let data = SessionData::open_parsed(&file, TailPolicy::RepairAndRewrite)?;
        Ok(Self { data, writer_lock })
    }
}

impl SessionData {
    /// 为只读扫描（列表、摘要、分页投影）打开既有会话文件。
    ///
    /// 此接缝不获取写者锁、不做任何写入：仅校验完整文件，需要正常
    /// 重开修复路径的文件被拒绝。
    pub fn open(path: &Path) -> Result<Self> {
        if !path.is_file() {
            return Err(SessionError::InvalidSession(format!(
                "session file does not exist: {}",
                path.display()
            )));
        }
        let session = Self::open_parsed(path, TailPolicy::RejectOnRepair)?;
        super::context::ContextView::validate(&session)?;
        Ok(session)
    }

    /// 共用的打开路径：解析、结构校验与状态捕获。修复策略只影响 torn
    /// tail 的处理（重写或拒绝），其余语义在两条路径间保持一致。
    fn open_parsed(path: &Path, tail_policy: TailPolicy) -> Result<Self> {
        let file = path.to_path_buf();
        let parsed = parse_session_lines(&file)?;
        if parsed.entries.is_empty() {
            return Err(SessionError::InvalidSession(format!(
                "Session file is not a valid session: {}",
                file.display()
            )));
        }
        let header = &parsed.entries[0];
        let (session_id, version, header_cwd, header_timestamp) = validate_header(header)?;
        let entries = validate_entries(&parsed.entries, &parsed.lines)?;
        if parsed.needs_repair && matches!(tail_policy, TailPolicy::RejectOnRepair) {
            return Err(SessionError::InvalidSession(
                "read-only session scan rejected a rollout requiring tail repair".into(),
            ));
        }
        let entries = if version == 5 {
            super::request::normalize_legacy(entries)?
        } else {
            entries
        };
        let request_index = super::request::RequestIndex::from_entries(&entries);
        if matches!(tail_policy, TailPolicy::RepairAndRewrite)
            && (parsed.needs_repair || version != CURRENT_SESSION_VERSION)
        {
            let mut values = Vec::with_capacity(entries.len() + 1);
            let mut header = header.clone();
            header["version"] = json!(CURRENT_SESSION_VERSION);
            values.push(header);
            for entry in &entries {
                values.push(serde_json::to_value(entry)?);
            }
            rewrite_file(&file, &values)?;
        }
        let cwd = PathBuf::from(&header_cwd);
        let cwd_display = header_cwd;
        let file_len = std::fs::metadata(&file)?.len();
        Ok(Self {
            file,
            cwd,
            cwd_display,
            entries,
            session_id,
            header_timestamp,
            file_len,
            request_index,
        })
    }
}

impl SessionManager {
    /// 共用的新建会话实现：先取写者锁，再写入 header 并打开新文件。
    fn create_with_file(
        cwd: &Path,
        sessions_dir: &Path,
        file_name: String,
        session_id: String,
        timestamp: String,
        coordinator: &Arc<WriterLockCoordinator>,
    ) -> Result<Self> {
        let cwd = singularity_core::canonicalize_workspace(cwd)
            .map_err(|error| SessionError::InvalidSession(error.to_string()))?;
        let cwd_display = cwd.display().to_string();
        std::fs::create_dir_all(sessions_dir)?;
        // 锁先于文件：会话文件一旦出现就受单写者保护。
        let writer_lock = coordinator.acquire(&session_id)?;
        let file = sessions_dir.join(file_name);
        let header = json!({
            "type": "session",
            "version": CURRENT_SESSION_VERSION,
            "id": session_id,
            "timestamp": timestamp,
            "cwd": &cwd_display,
        });
        let mut handle = singularity_core::create_owner_only_file(&file)?;
        writeln!(handle, "{}", serde_json::to_string(&header)?)?;
        handle.flush()?;
        let file_len = std::fs::metadata(&file)?.len();
        Ok(Self {
            data: SessionData {
                file,
                cwd: cwd.as_path().to_path_buf(),
                cwd_display,
                entries: Vec::new(),
                session_id,
                header_timestamp: timestamp,
                file_len,
                request_index: super::request::RequestIndex::default(),
            },
            writer_lock,
        })
    }

    /// 在既有条目集合内去重生成新条目 id；三个 append 入口与外部预分配共用。
    pub(crate) fn new_entry_id(&self) -> String {
        generate_id(|candidate| self.entries.iter().any(|entry| entry.id() == candidate))
    }

    /// 追加消息为当前 leaf 的子条目并推进 leaf，立即写盘。返回新条目 id。
    pub fn append_message(&mut self, message: AgentMessage) -> Result<String> {
        self.append_entry(SessionEntry::Message {
            id: self.new_entry_id(),
            timestamp: now_iso(),
            message,
        })
    }

    /// 追加 compaction 条目（预分配 id：compaction step attempt 的
    /// result_entry_id 指向它），立即写盘。返回新条目 id。
    pub fn append_compaction_with_id(
        &mut self,
        id: &str,
        compaction: CompactionEntry,
    ) -> Result<String> {
        if self.entries.iter().any(|entry| entry.id() == id) {
            return Err(SessionError::DuplicateId(id.to_string()));
        }
        self.append_entry(SessionEntry::Compaction {
            id: id.to_string(),
            timestamp: now_iso(),
            compaction,
        })
    }

    /// 追加不进入模型上下文的 metadata。
    pub fn append_metadata(&mut self, metadata: SessionMetadata) -> Result<String> {
        let metadata = metadata.validate()?;
        self.append_entry(SessionEntry::Metadata {
            id: self.new_entry_id(),
            timestamp: now_iso(),
            metadata,
        })
    }

    /// 追加一条 operation ledger 记录（不进入模型上下文）。
    pub fn append_record(&mut self, mut record: LedgerRecord) -> Result<String> {
        if let LedgerRecord::ModelRequest {
            observation,
            context,
        } = &mut record
            && let Some(request) = observation.request.take()
        {
            if context.is_some() {
                return Err(SessionError::InvalidStructure(
                    "request has both inline and referenced context".into(),
                ));
            }
            *context = Some(
                self.index_request(&serde_json::from_value(request)?)?
                    .into(),
            );
        }
        if let LedgerRecord::ModelRequest {
            context: Some(context),
            ..
        } = &record
        {
            self.request_index.validate(&self.entries, context)?;
        }
        let live_run = match &record {
            LedgerRecord::OperationStarted {
                operation_id,
                kind: super::format::OperationKind::Run,
                ..
            } => Some((operation_id.clone(), true)),
            LedgerRecord::OperationFinished {
                operation_id,
                turn_id: Some(_),
                ..
            } => Some((operation_id.clone(), false)),
            _ => None,
        };
        let id = self.append_entry(SessionEntry::Record {
            id: self.new_entry_id(),
            timestamp: now_iso(),
            record,
        })?;
        if let Some((operation_id, started)) = live_run {
            self.writer_lock.observe_run(&operation_id, started);
        }
        Ok(id)
    }

    /// 保存一次 provider 观测，直接索引已构造的模型请求。
    pub(crate) fn append_model_request(
        &mut self,
        observation: singularity_protocol::RequestObservation,
        request: Option<&singularity_model::ModelTurnRequest>,
    ) -> Result<String> {
        let context = request
            .map(|request| self.index_request(request))
            .transpose()?;
        self.append_record(LedgerRecord::ModelRequest {
            observation,
            context: context.map(Box::new),
        })
    }

    fn index_request(
        &mut self,
        request: &singularity_model::ModelTurnRequest,
    ) -> Result<super::request::RequestContext> {
        super::request::encode_request(request, |value| {
            if let Some(id) = self.request_index.find(&self.entries, &value) {
                return Ok(id);
            }
            self.append_record(LedgerRecord::RequestContent { value })
        })
    }

    /// 以预分配 id 追加消息；id 已存在时拒绝（单写者下只会因编程错误发生）。
    pub fn append_message_with_id(&mut self, id: &str, message: AgentMessage) -> Result<String> {
        if self.entries.iter().any(|entry| entry.id() == id) {
            return Err(SessionError::DuplicateId(id.to_string()));
        }
        self.append_entry(SessionEntry::Message {
            id: id.to_string(),
            timestamp: now_iso(),
            message,
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
        // 单写者语义：内存 entries 与 file_len 是唯一权威，append 前无需再
        // 读盘核对；limits 直接基于内存态的长度/条数判定。
        validate_append_limits(self.file_len, self.entries.len(), serialized.len(), limits)?;
        let mut handle = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.file)?;
        let bytes_to_write = serialized.as_bytes();
        let total_written = (bytes_to_write.len() + 1) as u64;
        handle.write_all(bytes_to_write)?;
        handle.write_all(b"\n")?;
        handle.flush()?;
        self.data.file_len += total_written;
        self.data
            .request_index
            .observe(&entry, self.data.entries.len());
        self.data.entries.push(entry);
        Ok(id)
    }
}

impl SessionData {
    /// 还原公开请求详情；仅消费已验证的不可变内容引用。
    pub fn request_snapshot(
        &self,
        context: &super::request::RequestContext,
    ) -> Result<serde_json::Value> {
        self.request_index.resolve(&self.entries, context)
    }

    /// Resolve a request by its durable lookup key, including legacy observation IDs.
    pub fn request_details(&self, id: &str) -> Result<serde_json::Value> {
        self.request_snapshot(self.request_index.lookup(&self.entries, id)?)
    }

    /// Project prompt and tool definitions without expanding conversation history.
    pub fn request_head(&self, id: &str) -> Result<serde_json::Value> {
        self.request_index
            .head(&self.entries, self.request_index.lookup(&self.entries, id)?)
    }

    /// 会话头部声明的稳定身份。
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

    /// 此快照的 JSONL 来源路径。
    pub fn path(&self) -> &Path {
        &self.file
    }

    /// 会话头部声明的规范工作目录。
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// 会话工作目录对外呈现的唯一形状：与会话头记录的字面值一致（正斜杠绝对
    /// 路径），供 Thread 投影、摘要与系统提示词共用，使同一事实在内存、磁盘与
    /// 模型可见文本中只有一个写法。
    pub fn cwd_string(&self) -> String {
        self.cwd_display.clone()
    }

    /// 按落盘顺序排列的已解析条目。
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
