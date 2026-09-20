//! 可变会话生命周期与追加管理器。

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

/// 既有会话的打开意图：调用方必须声明打开后要做什么，锁语义与修复行为
/// 由此单点决定，而不是散布在调用方的后续编排里。
pub enum SessionAccess {
    /// 持锁打开，校验头部 id 一致性并修复中断 turn 与孤立工具调用
    /// （turn 执行与 resume 前的写修复路径）。
    RepairWrite,
    /// 持锁打开并校验头部 id 一致性，修复撕裂尾部；不修复未完成 operation。
    Append,
}

/// 调用方打开既有会话时声明的期望身份。
///
/// 声明部分在解析出头部之后、尾部重写与未完成 operation 修复之前逐项校验，
/// 因此身份不符时本次打开不修改目标文件。`cwd` 表达工作区归属：会话头部记录
/// 的 cwd 必须与声明的目录指向同一位置。比较规则由
/// [`singularity_core::saved_directory_matches`] 提供，与工作台 scope 校验同源。
#[derive(Debug, Clone, Copy)]
pub struct ExpectedSession<'a> {
    pub id: &'a str,
    pub cwd: Option<&'a str>,
}

/// JSONL 会话管理器。会话是严格的线性序列，entries 的物理顺序即事实源顺序；
/// 会话由单个写者在整轮 turn 内独占持有（由共享进程内协调器强制执行），因此
/// append 不需要跨写者协调——同一会话同一时刻至多一个存活写者。
pub struct SessionManager {
    pub(super) data: SessionData,
    writer_lock: WriterLockGuard,
    append_error: Option<Arc<std::io::Error>>,
}

/// 已解析的会话事实。只读扫描与持锁写者共用解析、索引和投影，写入能力仅属于
/// SessionManager；读取已有会话不会获取写者锁或修复文件。
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
    pub(super) cwd_display: String,
    pub(super) entries: Vec<SessionEntry>,
    pub(super) session_id: String,
    pub(super) header_timestamp: String,
    /// 解析或最后一次追加时的文件长度。
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
    /// 测试便利构造器共用的协调器构造方式；并行测试仍各自持 per-tempdir
    /// 协调器，共享的是构造方式而不是实例。
    #[cfg(any(test, feature = "test-support"))]
    fn coordinator_for_tests() -> Arc<WriterLockCoordinator> {
        Arc::new(WriterLockCoordinator::default())
    }

    /// 新建会话：生成 UUID 并创建文件（测试便利入口）。
    #[cfg(any(test, feature = "test-support"))]
    pub fn create(cwd: &Path, sessions_dir: &Path) -> Result<Self> {
        Self::create_with_id_with_coordinator(
            cwd,
            sessions_dir,
            &Uuid::now_v7().to_string(),
            &Self::coordinator_for_tests(),
        )
    }

    /// 新建会话：文件名与 header id 都是调用方指定的 UUID，写者锁走调用方
    /// 持有的长驻协调器，统一锁目录与本进程活动回合投影。文件名与 header 时间
    /// 在这一条实现里从 session id 一次派生，不再作为参数向下传递。
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
        // 锁先于文件：会话文件一旦出现就受单写者保护。
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
                cwd_display: header.cwd,
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

    /// 打开必须已存在的会话文件；缺失或损坏直接报错，不静默创建新会话。
    /// 打开时向进程内写者协调器登记该会话（文件名 stem 为键），登记期间本进程
    /// 的其他写者被拒绝；跨进程独占由 CLI 数据目录层的锁负责。修复重写与
    /// 后续 append 全程持锁（测试便利入口）。
    ///
    /// 该入口不声明期望身份，因此不校验头部 id；需要身份校验的调用方一律使用
    /// [`Self::open_existing_with_access`]。
    #[cfg(any(test, feature = "test-support"))]
    pub fn open_existing(path: &Path) -> Result<Self> {
        Self::open_existing_with_coordinator(path, &Self::coordinator_for_tests(), None)
            .map(|(session, _)| session)
    }

    /// 按声明意图打开既有会话并使用调用方持有的长驻协调器。
    ///
    /// 期望身份在解析出头部之后、任何重写或修复之前校验，因此身份不符时本次
    /// 打开不修改目标文件。协调器由 runtime 的 TurnRunner 持有，共享本进程
    /// 活动回合投影。
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

    /// 打开既有会话并使用调用方持有的长驻协调器。
    ///
    /// 写者锁覆盖读取、身份校验与尾部修复，三者之间不放开独占。协调器由
    /// runtime 的 TurnRunner 持有，共享本进程活动回合投影。
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
    /// 此接缝不获取写者锁、不做任何写入：仅校验完整文件，需要正常
    /// 重开修复路径的文件被拒绝。模型上下文的有效性与计量在
    /// `ContextView::derive()` 中派生，由执行侧（`Agent::new()`）承担。
    pub fn open(path: &Path) -> Result<Self> {
        verify_session_file(path)?;
        let (session, _) = Self::open_parsed(path, TailPolicy::RejectOnRepair, None)?;
        Ok(session)
    }

    /// 共用的打开路径：解析、结构校验与状态捕获。修复策略只影响 torn
    /// tail 的处理（重写或拒绝），其余语义在两条路径间保持一致。
    ///
    /// `expected_id` 的校验紧跟在解析之后、尾部重写之前：声明了期望身份的
    /// 调用方在任何文件变更发生前就已确认目标身份。
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
        let cwd_display = header_cwd;
        let file_len = std::fs::metadata(&file)?.len();
        let mut data = Self {
            file,
            cwd,
            cwd_display,
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
    /// 交还已校验的只读事实并释放本次写者锁，供恢复后的历史投影复用。
    pub fn into_data(self) -> SessionData {
        self.data
    }

    /// 追加消息到线性日志，写入成功后推进内存视图。返回新条目 id。
    pub fn append_message(&mut self, message: AgentMessage) -> Result<String> {
        self.append_entry(SessionEntry::Message {
            id: super::new_entry_id(),
            timestamp: now_iso(),
            message,
        })
    }

    /// 追加 compaction 条目（预分配 id：本次摘要 attempt 的
    /// result_entry_id 指向它），立即写盘。返回新条目 id。
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

    /// 追加不进入模型上下文的 metadata。
    pub fn append_metadata(&mut self, metadata: SessionMetadata) -> Result<String> {
        let metadata = metadata.validate()?;
        self.append_entry(SessionEntry::Metadata {
            id: super::new_entry_id(),
            timestamp: now_iso(),
            metadata,
        })
    }

    /// 追加一条 operation ledger 记录。记录本身是持久事实，是否参与模型上下文
    /// 取决于类别：操作与请求观测只服务恢复与查看；文件指令、skill 指令与工具
    /// 剪枝记录会改变模型视图，由投影另行读取（见 ContextView）。
    pub fn append_record(&mut self, record: LedgerRecord) -> Result<String> {
        if let LedgerRecord::ModelRequest {
            context: Some(context),
            ..
        } = &record
        {
            self.validate_request_context(context)?;
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
            id: super::new_entry_id(),
            timestamp: now_iso(),
            record,
        })?;
        if let Some((operation_id, started)) = live_run {
            self.writer_lock.observe_run(&operation_id, started);
        }
        Ok(id)
    }

    /// 保存一次 provider 观测；开始记录成功后返回可公开的请求头。
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

    /// 以预分配 id 追加消息；id 已存在时拒绝（单写者下只会因编程错误发生）。
    pub fn append_message_with_id(&mut self, id: &str, message: AgentMessage) -> Result<String> {
        self.reject_existing_entry_id(id)?;
        self.append_entry(SessionEntry::Message {
            id: id.to_string(),
            timestamp: now_iso(),
            message,
        })
    }

    /// 调用方给定 id 的两个入口共用的拒重规则；自动生成 id 的普通追加不扫描。
    fn reject_existing_entry_id(&self, id: &str) -> Result<()> {
        if self.entries.iter().any(|entry| entry.id() == id) {
            return Err(SessionError::DuplicateId(id.to_string()));
        }
        Ok(())
    }

    pub(super) fn append_entry(&mut self, entry: SessionEntry) -> Result<String> {
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
        // 单写者语义：内存 entries 与 file_len 是唯一权威，append 前无需再
        // 读盘核对；增长上限直接基于内存态的长度/条数判定。
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
        // 写入失败可能在尾部留下半行 JSONL。保留它交给
        // 现有的重开修复路径；绝不向其后追加任何记录。
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

#[cfg(test)]
mod append_tests {
    use super::*;

    #[test]
    #[allow(clippy::unwrap_used)]
    fn partial_write_blocks_later_appends_until_reopen_repairs_the_tail() {
        struct ShortWriter(std::fs::File, bool);
        impl Write for ShortWriter {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                if self.1 {
                    return Err(std::io::Error::other("injected disk failure"));
                }
                self.1 = true;
                self.0.write(&bytes[..bytes.len().min(8)])
            }
            fn flush(&mut self) -> std::io::Result<()> {
                self.0.flush()
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let mut session = SessionManager::create(dir.path(), &dir.path().join("sessions")).unwrap();
        let path = session.path().to_path_buf();
        let mut writer = ShortWriter(OpenOptions::new().append(true).open(&path).unwrap(), false);
        assert!(
            session
                .write_append(&mut writer, br#"{"type":"message","id":"broken"}"#)
                .is_err()
        );
        drop(writer);
        let torn = std::fs::read(&path).unwrap();
        let error = session
            .append_message(crate::message::user_message("must not be appended"))
            .unwrap_err();
        assert!(error.to_string().contains("injected disk failure"));
        assert_eq!(std::fs::read(&path).unwrap(), torn);
        assert!(session.entries().is_empty());
        drop(session);
        let mut reopened = SessionManager::open_existing(&path).unwrap();
        reopened
            .append_message(crate::message::user_message("after repair"))
            .unwrap();
        assert_eq!(SessionData::open(&path).unwrap().entries().len(), 1);
    }
}

impl SessionData {
    /// 会话头部声明的稳定身份。
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// 校验会话头部 id 与请求一致；不一致属于损坏状态。
    pub fn verify_session_id(&self, expected: &str) -> Result<()> {
        verify_header_id(self.session_id(), expected)
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

    /// 会话工作目录对外呈现的唯一形状：重开时取会话头字面值经规范化后的正斜杠
    /// 绝对路径，供 Thread 投影、摘要与系统提示词共用，使同一事实在内存与模型
    /// 可见文本中只有一个写法。磁盘头里的原始字面值另行保留（修复写回不改写已存
    /// 路径），只有新建会话才可能与本值逐字相同。
    pub fn cwd_string(&self) -> String {
        self.cwd_display.clone()
    }

    /// 按落盘顺序排列的已解析条目。
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

/// 尾部修复策略：正常打开修复重写，只读扫描拒绝。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TailPolicy {
    RepairAndRewrite,
    RejectOnRepair,
}

/// 头部身份与期望 id 的一致性规则；打开与只读校验共用同一处判定。
fn verify_header_id(actual: &str, expected: &str) -> Result<()> {
    if actual == expected {
        Ok(())
    } else {
        Err(SessionError::InvalidHeader(format!(
            "rollout header id {actual} does not match expected id {expected}"
        )))
    }
}

/// 头部 cwd 与期望目录的一致性规则：二者必须指向同一已保存目录。
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
