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
    CompactionEntry, LedgerRecord, Result, SessionEntry, SessionError, SessionHeader, SessionMetadata,
};

/// JSONL 会话管理器。会话是严格的线性序列，entries 的物理顺序就是事实来源的顺序；
/// 执行入口负责交接单个写者，turn 内通过 SessionWriter 串行追加。
pub struct SessionManager {
    pub(super) data: SessionData,
    append_error: Option<Arc<std::io::Error>>,
}

/// 已解析出来的会话事实。只读扫描与写者共用同一套解析、索引和投影，写入
/// 能力只属于 SessionManager；只读打开已有会话不会修改文件。
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
        f.debug_struct("SessionManager").field("data", &self.data).finish()
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
    /// 新建会话：文件名与 header id 都用调用方指定的 UUID，创建时间在写入文件头时生成。
    pub fn create_with_id(cwd: &Path, sessions_dir: &Path, session_id: &str) -> Result<Self> {
        let cwd = singularity_core::canonicalize_workspace(cwd).map_err(SessionError::InvalidSession)?;
        let cwd_display = cwd.display().to_string();
        std::fs::create_dir_all(sessions_dir)?;
        let file = sessions_dir.join(super::session_file_name(session_id));
        let header = SessionHeader::new(session_id.to_string(), cwd_display, now_iso());
        let mut header_bytes = serde_json::to_vec(&header)?;
        header_bytes.push(b'\n');
        singularity_core::atomic_create_bytes(&file, &header_bytes)?;
        Ok(Self {
            data: SessionData {
                file,
                cwd: cwd.as_path().to_path_buf(),
                entries: Vec::new(),
                session_id: header.id,
                header_timestamp: header.timestamp,
                definitions: std::collections::HashMap::new(),
            },
            append_error: None,
        })
    }

    /// 打开既有会话，校验头部身份并清除未完成的尾行；完整记录保持原样。
    /// 调用方须在执行入口持有该会话的写入所有权。
    pub fn open_existing(path: &Path, expected_id: &str) -> Result<Self> {
        let file = path.to_path_buf();
        let data = SessionData::open_parsed(&file, TailPolicy::RepairAndRewrite, Some(expected_id))?;
        Ok(Self { data, append_error: None })
    }
}

impl SessionData {
    /// 图片快照与任务身份绑定，归档后仍使用同一个目录。
    pub fn image_directory(&self) -> PathBuf {
        let parent = self.file.parent().expect("session has a parent directory");
        let root = if parent.file_name().is_some_and(|name| name == "archived") {
            parent.parent().expect("archive belongs to sessions directory")
        } else {
            parent
        };
        root.join("images").join(&self.session_id)
    }

    /// 只允许读取这份历史实际引用的图片，不接受任意文件路径。
    pub fn image_data(&self, image_id: &str) -> Result<String> {
        let attachment = self
            .entries
            .iter()
            .filter_map(|entry| match entry {
                SessionEntry::Message { message, .. } => Some(message),
                _ => None,
            })
            .flat_map(AgentMessage::images)
            .find(|image| image.id == image_id)
            .ok_or_else(|| SessionError::InvalidSession("图片不在该任务历史中。".into()))?;
        Ok(crate::image::load_image(&self.image_directory(), attachment)?)
    }

    /// 为只读扫描（列表、摘要、分页投影）打开既有会话文件。
    ///
    /// 只读取以换行符结束的完整记录，不写入。执行侧重开写者时
    /// 修复未完成的尾行，模型上下文由 `ContextView::derive()` 派生。
    pub fn open(path: &Path) -> Result<Self> {
        Self::open_parsed(path, TailPolicy::CompleteLines, None)
    }

    /// 两条打开路径共用解析与索引；写打开时修复尾行。
    fn open_parsed(path: &Path, tail_policy: TailPolicy, expected_id: Option<&str>) -> Result<Self> {
        let file = path.to_path_buf();
        let ParsedSession { header, cwd, entries, needs_repair } = parse_session_file(&file, tail_policy)?;
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
    /// 类别：操作与请求观测服务查看，指令与工具剪枝记录改变模型视图。
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
        request: Option<(super::RequestDefinitions, singularity_protocol::RequestPreferences)>,
    ) -> Result<Option<Box<singularity_protocol::ModelRequestSnapshot>>> {
        let (context, head) = if let Some((definitions, model_preferences)) = request {
            let id = match self.latest_request_definitions() {
                Some((id, previous)) if previous == &definitions => id.to_owned(),
                _ => {
                    self.append_record(LedgerRecord::RequestDefinitions { definitions: definitions.clone() })?
                }
            };
            let head = definitions.snapshot(&id, &model_preferences);
            (
                Some(Box::new(super::request::RequestContext { definitions: id, model_preferences })),
                Some(head),
            )
        } else {
            (None, None)
        };
        self.append_record(LedgerRecord::ModelRequest { observation, context })?;
        Ok(head)
    }

    /// 追加执行器为本次回答或工具结果预分配身份的消息。
    pub(crate) fn append_message_with_id(&mut self, id: &str, message: AgentMessage) -> Result<String> {
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
                format!("previous session append failed; reopen the writer to repair its tail: {error}"),
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
    /// 读取测试需要断言的持久记录；不包含消息、摘要和元数据。
    #[cfg(any(test, feature = "test-support"))]
    pub fn ledger_records(&self) -> Vec<LedgerRecord> {
        self.entries
            .iter()
            .filter_map(|entry| match entry {
                SessionEntry::Record { record, .. } => Some(record.clone()),
                _ => None,
            })
            .collect()
    }

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
