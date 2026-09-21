//! 可变会话的生命周期与追加管理。

use std::fs::OpenOptions;
use std::io::Write;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use singularity_core::now_iso;
use uuid::Uuid;

use crate::message::AgentMessage;

use super::file::{ParsedSession, parse_session_file, rewrite_file, validate_append_limits};
use super::format::{
    CompactionEntry, LedgerRecord, Result, SessionEntry, SessionError, SessionHeader,
    SessionMetadata,
};
use super::writer_lock::{WriterLockCoordinator, WriterLockGuard};

/// 打开既有会话的意图：锁语义和修复行为都由这个声明一处决定，不散落在调用方。
pub enum SessionAccess {
    /// 持锁打开，校验头部 id 是否一致，并修复被中断的 turn 与孤立的工具调用
    /// （turn 执行前和 resume 前的写修复走这条路径）。
    RepairWrite,
    /// 持锁打开并校验头部 id 是否一致，只修复撕裂的尾部；未完成的 operation 不动。
    Append,
}

/// 调用方打开既有会话时声明的期望身份。
///
/// 这些声明在解析出头部之后、尾部重写和未完成 operation 修复之前逐项校验，所以身份
/// 不符时本次打开不会改动目标文件。`cwd` 表达工作区归属，比较规则来自
/// [`singularity_core::saved_directory_matches`]，与工作台 scope 校验同源。
#[derive(Debug, Clone, Copy)]
pub struct ExpectedSession<'a> {
    pub id: &'a str,
    pub cwd: Option<&'a str>,
}

/// JSONL 会话管理器。会话是严格的线性序列，entries 的物理顺序就是事实来源的顺序；
/// 整个 turn 内由单个写者独占（进程内共享协调器强制），追加不需要跨写者协调。
pub struct SessionManager {
    pub(super) data: SessionData,
    writer_lock: WriterLockGuard,
    append_error: Option<Arc<std::io::Error>>,
}

/// 已解析出来的会话事实。只读扫描与持锁写者共用同一套解析、索引和投影，写入
/// 能力只属于 SessionManager；只读打开已有会话不会拿写者锁，也不会修复文件。
///
/// ```compile_fail
/// use singularity_agent::session::{SessionData, SessionMetadata};
/// fn append_without_a_writer(mut session: SessionData) {
///     session.append_metadata(SessionMetadata::ThreadName { name: "renamed".into() }).unwrap();
/// }
/// ```
pub struct SessionData {
    pub(super) file: PathBuf,
    pub(super) cwd: PathBuf,
    pub(super) entries: Vec<SessionEntry>,
    pub(super) session_id: String,
    pub(super) header_timestamp: String,
    /// 解析时、或最后一次追加之后的文件长度。
    pub(super) file_len: u64,
    pub(super) definitions: std::collections::HashMap<String, usize>,
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
    /// 测试便利构造器共用的协调器构造方式；并行测试各自持有自己的实例。
    #[cfg(any(test, feature = "test-support"))]
    fn coordinator_for_tests() -> Arc<WriterLockCoordinator> {
        Arc::new(WriterLockCoordinator::default())
    }

    /// 新建会话：生成 UUID 并创建文件（测试与 test-support 的便利入口）。
    #[cfg(any(test, feature = "test-support"))]
    pub fn create(cwd: &Path, sessions_dir: &Path) -> Result<Self> {
        Self::create_with_id_with_coordinator(
            cwd,
            sessions_dir,
            &Uuid::now_v7().to_string(),
            &Self::coordinator_for_tests(),
        )
    }

    /// 新建会话：文件名与 header id 都用调用方指定的 UUID；写者锁走调用方持有的
    /// 长驻协调器，以便统一锁目录和本进程的活动回合投影。文件名与 header 时间都
    /// 在这一处从 session id 派生出来，不再作为参数往下传。
    pub fn create_with_id_with_coordinator(
        cwd: &Path,
        sessions_dir: &Path,
        session_id: &str,
        coordinator: &Arc<WriterLockCoordinator>,
    ) -> Result<Self> {
        Uuid::parse_str(session_id).map_err(|_| {
            SessionError::InvalidSession(format!("session id is not a UUID: {session_id}"))
        })?;
        let cwd =
            singularity_core::canonicalize_workspace(cwd).map_err(SessionError::InvalidSession)?;
        let cwd_display = cwd.display().to_string();
        std::fs::create_dir_all(sessions_dir)?;
        // 先拿锁再建文件：会话文件一旦出现就已经处在单写者保护之下。
        let writer_lock = coordinator.acquire(session_id)?;
        let file = sessions_dir.join(super::session_file_name(session_id));
        let header = SessionHeader::new(session_id.to_string(), cwd_display, now_iso());
        let mut handle = singularity_core::create_new_file(&file)?;
        writeln!(handle, "{}", serde_json::to_string(&header)?)?;
        handle.flush()?;
        let file_len = std::fs::metadata(&file)?.len();
        Ok(Self {
            data: SessionData {
                file,
                cwd: cwd.as_path().to_path_buf(),
                entries: Vec::new(),
                session_id: header.id,
                header_timestamp: header.timestamp,
                file_len,
                definitions: std::collections::HashMap::new(),
            },
            writer_lock,
            append_error: None,
        })
    }

    /// 打开一个必须已存在的会话文件；缺失或损坏就直接报错，不会静默新建会话。打开时
    /// 按文件名 stem 向进程内写者协调器登记，登记期间本进程的其他写者被拒绝；跨进程
    /// 独占由 CLI 数据目录层的锁负责。修复重写和后续追加全程持锁。本入口不声明期望
    /// 身份，因此不校验头部 id；需要身份校验的调用方用 [`Self::open_existing_with_access`]。
    #[cfg(any(test, feature = "test-support"))]
    pub fn open_existing(path: &Path) -> Result<Self> {
        Self::open_existing_with_coordinator(path, &Self::coordinator_for_tests(), None)
            .map(|(session, _)| session)
    }

    /// 按声明的意图打开既有会话，并使用调用方持有的长驻协调器（runtime 的 TurnRunner
    /// 持有它，用来共享本进程的活动回合投影）。
    pub fn open_existing_with_access(
        path: &Path,
        coordinator: &Arc<WriterLockCoordinator>,
        expected: ExpectedSession<'_>,
        access: SessionAccess,
    ) -> Result<Self> {
        let (mut session, operation) =
            Self::open_existing_with_coordinator(path, coordinator, Some(expected))?;
        if matches!(access, SessionAccess::RepairWrite) {
            session.repair_interrupted_operation(operation)?;
        }
        Ok(session)
    }

    /// 打开既有会话，并使用调用方持有的长驻协调器。写者锁覆盖读取、身份校验和尾部
    /// 修复，这三步之间不放开独占。
    fn open_existing_with_coordinator(
        path: &Path,
        coordinator: &Arc<WriterLockCoordinator>,
        expected: Option<ExpectedSession<'_>>,
    ) -> Result<(Self, Option<super::operation::OperationState>)> {
        verify_session_file(path)?;
        let file = path.to_path_buf();
        let lock_key = path.file_stem().and_then(|s| s.to_str()).ok_or_else(|| {
            SessionError::InvalidSession(format!(
                "session file name is not valid UTF-8: {}",
                path.display()
            ))
        })?;
        let writer_lock = coordinator.acquire(lock_key)?;
        let (data, operation) =
            SessionData::open_parsed(&file, TailPolicy::RepairAndRewrite, expected)?;
        Ok((
            Self {
                data,
                writer_lock,
                append_error: None,
            },
            operation,
        ))
    }
}

impl SessionData {
    /// 为只读扫描（列表、摘要、分页投影）打开既有会话文件。
    ///
    /// 这条路径不获取写者锁、不做任何写入：只校验文件是否完整，需要走重开修复路径的
    /// 文件一律拒绝。模型上下文由执行侧的 `Agent::new()` 经 `ContextView::derive()` 派生。
    pub fn open(path: &Path) -> Result<Self> {
        verify_session_file(path)?;
        let (session, _) = Self::open_parsed(path, TailPolicy::RejectOnRepair, None)?;
        Ok(session)
    }

    /// 两条打开路径共用的实现：解析、结构校验与状态捕获。修复策略只影响撕裂尾部
    /// 怎么处理（重写还是拒绝），其余语义在两条路径间保持一致。
    fn open_parsed(
        path: &Path,
        tail_policy: TailPolicy,
        expected: Option<ExpectedSession<'_>>,
    ) -> Result<(Self, Option<super::operation::OperationState>)> {
        let file = path.to_path_buf();
        let ParsedSession {
            header,
            cwd: header_cwd,
            entries,
            needs_repair,
        } = parse_session_file(&file)?;
        if let Some(expected) = expected {
            verify_header_id(&header.id, expected.id)?;
            if let Some(cwd) = expected.cwd {
                verify_header_cwd(&header_cwd, cwd)?;
            }
        }
        let operation = super::operation::reduce_operations(&entries)?;
        if needs_repair && matches!(tail_policy, TailPolicy::RejectOnRepair) {
            return Err(SessionError::InvalidSession(
                "read-only session scan rejected a rollout requiring tail repair".into(),
            ));
        }
        if matches!(tail_policy, TailPolicy::RepairAndRewrite) && needs_repair {
            rewrite_file(&file, &header, &entries)?;
        }
        let cwd = PathBuf::from(&header_cwd);
        let file_len = std::fs::metadata(&file)?.len();
        let mut data = Self {
            file,
            cwd,
            entries,
            session_id: header.id,
            header_timestamp: header.timestamp,
            file_len,
            definitions: std::collections::HashMap::new(),
        };
        for position in 0..data.entries.len() {
            data.observe_definitions(position);
        }
        Ok((data, operation))
    }
}

impl SessionManager {
    /// 交还已校验的只读事实，同时释放本次写者锁；恢复后的历史投影复用它。
    pub fn into_data(self) -> SessionData {
        self.data
    }

    /// 往线性日志追加一条消息，写盘成功后再推进内存视图。返回新条目的 id。
    pub fn append_message(&mut self, message: AgentMessage) -> Result<String> {
        self.append_entry(SessionEntry::Message {
            id: super::new_entry_id(),
            timestamp: now_iso(),
            message,
        })
    }

    /// 追加一条 compaction 条目并立即写盘（id 是预分配的：本次摘要 attempt 的
    /// result_entry_id 就指向它）。返回新条目的 id。
    pub fn append_compaction_with_id(
        &mut self,
        id: &str,
        compaction: CompactionEntry,
    ) -> Result<String> {
        self.reject_existing_entry_id(id)?;
        self.append_entry(SessionEntry::Compaction {
            id: id.to_string(),
            timestamp: now_iso(),
            compaction,
        })
    }

    /// 追加一条不进入模型上下文的 metadata。
    pub fn append_metadata(&mut self, metadata: SessionMetadata) -> Result<String> {
        let metadata = metadata.validate()?;
        self.append_entry(SessionEntry::Metadata {
            id: super::new_entry_id(),
            timestamp: now_iso(),
            metadata,
        })
    }

    /// 追加一条 operation ledger 记录。记录本身就是持久事实；是否进入模型上下文看
    /// 类别：操作与请求观测只服务恢复和查看，指令与工具剪枝记录改变模型视图。
    pub fn append_record(&mut self, record: LedgerRecord) -> Result<String> {
        if let LedgerRecord::ModelRequest {
            context: Some(context),
            ..
        } = &record
        {
            self.validate_request_context(context)?;
        }
        // 只有绑定 turn 的 run 起止记录进入进程内活动回合投影。
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
            id: super::new_entry_id(),
            timestamp: now_iso(),
            record,
        })?;
        if let Some((operation_id, started)) = live_run {
            self.writer_lock.observe_run(operation_id, started);
        }
        Ok(id)
    }

    /// 保存一次 provider 观测；开始记录写成功后就返回可公开的请求头。
    pub(crate) fn append_model_request(
        &mut self,
        observation: singularity_protocol::RequestObservation,
        request: Option<&singularity_model::ModelTurnRequest>,
    ) -> Result<Option<Box<singularity_protocol::ModelRequestSnapshot>>> {
        let (context, head) = if let Some(request) = request {
            let definitions = super::request::RequestDefinitions::from_request(request);
            let head = definitions.snapshot(&request.model_preferences);
            let id = match self.find_definitions(&definitions) {
                Some(id) => id,
                None => self.append_record(LedgerRecord::RequestDefinitions { definitions })?,
            };
            (
                Some(Box::new(super::request::RequestContext::new(
                    id,
                    request.model_preferences.clone(),
                ))),
                Some(head),
            )
        } else {
            (None, None)
        };
        self.append_record(LedgerRecord::ModelRequest {
            observation,
            context,
        })?;
        Ok(head)
    }

    /// 用预分配的 id 追加消息；id 已存在就拒绝（单写者之下只可能是编程错误）。
    pub fn append_message_with_id(&mut self, id: &str, message: AgentMessage) -> Result<String> {
        self.reject_existing_entry_id(id)?;
        self.append_entry(SessionEntry::Message {
            id: id.to_string(),
            timestamp: now_iso(),
            message,
        })
    }

    /// 两个由调用方给定 id 的入口共用的拒重规则；自动生成 id 的普通追加不扫这一遍。
    fn reject_existing_entry_id(&self, id: &str) -> Result<()> {
        if self.entries.iter().any(|entry| entry.id() == id) {
            return Err(SessionError::DuplicateId(id.to_string()));
        }
        Ok(())
    }

    pub(super) fn append_entry(&mut self, entry: SessionEntry) -> Result<String> {
        // 上次追加已经失败：文件尾部可能残缺，重开修复前不再接受任何写入。
        if let Some(error) = &self.append_error {
            return Err(SessionError::Io(std::io::Error::new(
                error.kind(),
                format!(
                    "previous session append failed; reopen the writer to repair its tail: {error}"
                ),
            )));
        }
        let id = entry.id().to_string();
        let serialized = serde_json::to_string(&entry)?;
        // 单写者语义：内存里的 entries 与 file_len 就是唯一权威，追加前不必再读盘
        // 核对；增长上限也直接按内存中的长度和条数判定。
        validate_append_limits(self.file_len, self.entries.len(), serialized.len())?;
        let mut handle = OpenOptions::new().append(true).open(&self.file)?;
        let bytes_to_write = serialized.as_bytes();
        let total_written = (bytes_to_write.len() + 1) as u64;
        self.write_append(&mut handle, bytes_to_write)?;
        self.data.file_len += total_written;
        self.data.entries.push(entry);
        self.data.observe_definitions(self.data.entries.len() - 1);
        Ok(id)
    }

    fn write_append(&mut self, handle: &mut impl Write, bytes: &[u8]) -> Result<()> {
        // 写入失败可能在文件尾部留下半行 JSONL。这一行原样保留，交给现有的重开
        // 修复路径处理；绝不往它后面再追加任何记录。
        let result = handle
            .write_all(bytes)
            .and_then(|()| handle.write_all(b"\n"))
            .and_then(|()| handle.flush());
        result.map_err(|error| {
            let error = Arc::new(error);
            self.append_error = Some(Arc::clone(&error));
            SessionError::Io(std::io::Error::new(error.kind(), error))
        })
    }
}

impl SessionData {
    /// 会话头部声明的稳定身份，即会话 id。
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// 校验会话头部 id 与请求方声明的是否一致；不一致就属于损坏状态。
    pub fn verify_session_id(&self, expected: &str) -> Result<()> {
        verify_header_id(self.session_id(), expected)
    }

    /// header 里的时间戳，是重建索引时权威的创建时间。
    pub fn created_at(&self) -> &str {
        &self.header_timestamp
    }

    /// 这份快照对应的 JSONL 文件路径。
    pub fn path(&self) -> &Path {
        &self.file
    }

    /// 会话头部声明的规范工作目录（已归一化）。
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// 工作目录归一化后的显示路径，Thread 投影、摘要与系统提示词共用。
    /// 修复写回用的是解析时的原始 header，不受这里显示转换的影响。
    pub fn cwd_string(&self) -> String {
        singularity_core::display_path(&self.cwd)
    }

    /// 已解析的条目，按落盘顺序排列。
    pub fn entries(&self) -> &[SessionEntry] {
        &self.entries
    }
}

fn verify_session_file(path: &Path) -> Result<()> {
    if !std::fs::metadata(path)?.is_file() {
        return Err(SessionError::InvalidSession(format!(
            "session path is not a file: {}",
            path.display()
        )));
    }
    Ok(())
}

/// 尾部修复策略：正常打开时重写修复，只读扫描时直接拒绝。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TailPolicy {
    RepairAndRewrite,
    RejectOnRepair,
}

/// 头部身份与期望 id 是否一致的规则；打开和只读校验共用这一处判定。
fn verify_header_id(actual: &str, expected: &str) -> Result<()> {
    if actual == expected {
        Ok(())
    } else {
        Err(SessionError::InvalidHeader(format!(
            "rollout header id {actual} does not match expected id {expected}"
        )))
    }
}

/// 头部 cwd 与期望目录是否一致的规则：两者必须指向同一个已保存的目录。
fn verify_header_cwd(actual: &str, expected: &str) -> Result<()> {
    match singularity_core::saved_directory_matches(expected, actual) {
        Ok(true) => Ok(()),
        Ok(false) => Err(SessionError::ScopeMismatch {
            actual: actual.to_string(),
            expected: expected.to_string(),
        }),
        Err(message) => Err(SessionError::InvalidHeader(message)),
    }
}
