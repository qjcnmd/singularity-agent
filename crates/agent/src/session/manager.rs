//! 可变会话的生命周期与追加管理。

use std::fs::OpenOptions;
use std::io::Write;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use singularity_core::now_iso;

use crate::message::AgentMessage;

use super::file::{ParsedSession, TailPolicy, parse_session_file, rewrite_file};
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

/// JSONL 会话管理器。会话是严格的线性序列，entries 的物理顺序就是事实来源的顺序；
/// 整个 turn 内由单个写者独占（进程内共享协调器强制），追加不需要跨写者协调。
pub struct SessionManager {
    pub(super) data: SessionData,
    _writer_lock: WriterLockGuard,
    append_error: Option<Arc<std::io::Error>>,
}

/// 已解析出来的会话事实。只读扫描与持锁写者共用同一套解析、索引和投影，写入
/// 能力只属于 SessionManager；只读打开已有会话不会拿写者锁，也不会修复文件。
pub struct SessionData {
    pub(super) file: PathBuf,
    pub(super) cwd: PathBuf,
    pub(super) entries: Vec<SessionEntry>,
    pub(super) session_id: String,
    pub(super) header_timestamp: String,
    /// 请求定义 ID 对应的条目位置。
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

    /// 新建会话：文件名与 header id 都用调用方指定的 UUID；写者锁走调用方持有的
    /// 长驻协调器，以便统一锁目录和本进程的活动回合投影。文件名由
    /// session id 派生，创建时间在写入文件头时生成。
    pub fn create_with_id_with_coordinator(
        cwd: &Path,
        sessions_dir: &Path,
        session_id: &str,
        coordinator: &Arc<WriterLockCoordinator>,
    ) -> Result<Self> {
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
        Ok(Self {
            data: SessionData {
                file,
                cwd: cwd.as_path().to_path_buf(),
                entries: Vec::new(),
                session_id: header.id,
                header_timestamp: header.timestamp,
                definitions: std::collections::HashMap::new(),
            },
            _writer_lock: writer_lock,
            append_error: None,
        })
    }

    /// 打开一个必须已存在的会话文件；缺失或损坏就直接报错，不会静默新建会话。打开时
    /// 按文件名 stem 向进程内写者协调器登记，登记期间本进程的其他写者被拒绝；跨进程
    /// 独占由数据目录的 OS 级锁负责。修复重写和后续追加全程持锁。本入口不声明期望
    /// 身份，因此不校验头部 id；需要身份校验的调用方用 [`Self::open_existing_with_access`]。
    #[cfg(any(test, feature = "test-support"))]
    pub fn open_existing(path: &Path) -> Result<Self> {
        Self::open_existing_with_coordinator(path, &Self::coordinator_for_tests(), None)
    }

    /// 按声明的意图打开既有会话，并使用调用方持有的长驻协调器（runtime 的 TurnRunner
    /// 持有它，用来共享本进程的活动回合投影）。
    pub fn open_existing_with_access(
        path: &Path,
        coordinator: &Arc<WriterLockCoordinator>,
        expected_id: &str,
        access: SessionAccess,
    ) -> Result<Self> {
        let mut session =
            Self::open_existing_with_coordinator(path, coordinator, Some(expected_id))?;
        if matches!(access, SessionAccess::RepairWrite) {
            let operation = super::operation::reduce_operations(session.entries());
            session.repair_interrupted_operation(operation)?;
        }
        Ok(session)
    }

    /// 打开既有会话，并使用调用方持有的长驻协调器。写者锁覆盖读取、身份校验和尾部
    /// 修复，这三步之间不放开独占。
    fn open_existing_with_coordinator(
        path: &Path,
        coordinator: &Arc<WriterLockCoordinator>,
        expected_id: Option<&str>,
    ) -> Result<Self> {
        let file = path.to_path_buf();
        let lock_key = path
            .file_stem()
            .and_then(|name| name.to_str())
            .expect("session path has a UUID file name");
        let writer_lock = coordinator.acquire(lock_key)?;
        let data = SessionData::open_parsed(&file, TailPolicy::RepairAndRewrite, expected_id)?;
        Ok(Self {
            data,
            _writer_lock: writer_lock,
            append_error: None,
        })
    }
}

impl SessionData {
    /// 为只读扫描（列表、摘要、分页投影）打开既有会话文件。
    ///
    /// 只读取以换行符结束的完整记录，不获取写者锁、不写入。执行侧重开写者时
    /// 修复未完成的尾行，模型上下文由 `ContextView::derive()` 派生。
    pub fn open(path: &Path) -> Result<Self> {
        Self::open_parsed(path, TailPolicy::CompleteLines, None)
    }

    /// 两条打开路径共用解析与索引；写打开时修复尾行，执行恢复由写者入口负责。
    fn open_parsed(
        path: &Path,
        tail_policy: TailPolicy,
        expected_id: Option<&str>,
    ) -> Result<Self> {
        let file = path.to_path_buf();
        let ParsedSession {
            header,
            cwd,
            entries,
            needs_repair,
        } = parse_session_file(&file, tail_policy)?;
        if let Some(expected_id) = expected_id {
            verify_header_id(&header.id, expected_id)?;
        }
        if matches!(tail_policy, TailPolicy::RepairAndRewrite) && needs_repair {
            rewrite_file(&file, &header, &entries)?;
        }
        let mut data = Self {
            file,
            cwd,
            entries,
            session_id: header.id,
            header_timestamp: header.timestamp,
            definitions: std::collections::HashMap::new(),
        };
        for position in 0..data.entries.len() {
            data.observe_definitions(position);
        }
        Ok(data)
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
    pub(crate) fn append_compaction_with_id(
        &mut self,
        id: &str,
        compaction: CompactionEntry,
    ) -> Result<String> {
        self.append_entry(SessionEntry::Compaction {
            id: id.to_string(),
            timestamp: now_iso(),
            compaction,
        })
    }

    /// 追加一条不进入模型上下文的 metadata。
    pub fn append_metadata(&mut self, metadata: SessionMetadata) -> Result<String> {
        self.append_entry(SessionEntry::Metadata {
            id: super::new_entry_id(),
            timestamp: now_iso(),
            metadata,
        })
    }

    /// 追加一条 operation ledger 记录。记录本身就是持久事实；是否进入模型上下文看
    /// 类别：操作与请求观测只服务恢复和查看，指令与工具剪枝记录改变模型视图。
    pub fn append_record(&mut self, record: LedgerRecord) -> Result<String> {
        self.append_entry(SessionEntry::Record {
            id: super::new_entry_id(),
            timestamp: now_iso(),
            record,
        })
    }

    /// 保存一次 provider 观测；开始记录写成功后就返回可公开的请求头。
    pub(crate) fn append_model_request(
        &mut self,
        observation: singularity_protocol::RequestObservation,
        request: Option<(
            super::RequestDefinitions,
            singularity_protocol::RequestPreferences,
        )>,
    ) -> Result<Option<Box<singularity_protocol::ModelRequestSnapshot>>> {
        let (context, head) = if let Some((definitions, model_preferences)) = request {
            let id = match self.find_definitions(&definitions) {
                Some(id) => id,
                None => self.append_record(LedgerRecord::RequestDefinitions {
                    definitions: definitions.clone(),
                })?,
            };
            let head = definitions.snapshot(&id, &model_preferences);
            (
                Some(Box::new(super::request::RequestContext {
                    definitions: id,
                    model_preferences,
                })),
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

    /// 追加执行器为本次回答或工具结果预分配身份的消息。
    pub(crate) fn append_message_with_id(
        &mut self,
        id: &str,
        message: AgentMessage,
    ) -> Result<String> {
        self.append_entry(SessionEntry::Message {
            id: id.to_string(),
            timestamp: now_iso(),
            message,
        })
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
        let mut handle = OpenOptions::new().append(true).open(&self.file)?;
        let bytes_to_write = serialized.as_bytes();
        self.write_append(&mut handle, bytes_to_write)?;
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
