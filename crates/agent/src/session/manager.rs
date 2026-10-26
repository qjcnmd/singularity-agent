//! 会话日志的创建、打开和追加写入。

use std::fs::OpenOptions;
use std::io::Write;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use singularity_core::now_iso;

use singularity_model::Message;

use super::file::{parse_session_file, repair_tail};
use super::format::{
    CompactionEntry, LedgerRecord, Result, SessionEntry, SessionError, SessionHeader, SessionMetadata,
};

/// 可写的会话日志。记录按写入顺序排列，这个顺序就是事情发生的顺序；
/// 一个会话只有一个 SessionManager，通过 SessionWriter 共享并按顺序追加。
pub struct SessionManager {
    pub(super) data: SessionData,
    /// 上一次追加失败的原因；之后拒绝写入，直到重新打开日志截掉残缺的末尾。
    append_error: Option<Arc<std::io::Error>>,
}

/// 解析后的会话内容。只读打开和写者共用这套解析结果；只读打开不会修改文件。
pub struct SessionData {
    pub(super) file: PathBuf,
    pub(super) cwd: PathBuf,
    pub(super) entries: Vec<SessionEntry>,
    pub(super) session_id: String,
    pub(super) header_timestamp: String,
}

impl Deref for SessionManager {
    type Target = SessionData;

    fn deref(&self) -> &Self::Target {
        &self.data
    }
}

impl SessionManager {
    /// 新建会话日志：文件名和文件头里的 ID 都用调用方给的 UUID；文件已存在时报错，
    /// 写入失败时删除文件。
    pub fn create_with_id(
        cwd: &Path,
        sessions_dir: &Path,
        session_id: &str,
        selection: singularity_protocol::ModelSelection,
    ) -> Result<Self> {
        let cwd = singularity_core::canonicalize_workspace(cwd).map_err(SessionError::InvalidSession)?;
        let cwd_display = cwd.display().to_string();
        std::fs::create_dir_all(sessions_dir)?;
        let file = sessions_dir.join(super::session_file_name(session_id));
        let header = SessionHeader::new(session_id.to_string(), cwd_display, now_iso());
        let mut header_bytes = serde_json::to_vec(&header)?;
        header_bytes.push(b'\n');
        let entries = vec![SessionEntry::Metadata {
            id: super::new_entry_id(),
            timestamp: header.timestamp.clone(),
            metadata: SessionMetadata::ThreadSettings { selection },
        }];
        for entry in &entries {
            serde_json::to_writer(&mut header_bytes, entry)?;
            header_bytes.push(b'\n');
        }
        let mut handle = std::fs::File::create_new(&file)?;
        if let Err(error) = handle.write_all(&header_bytes) {
            drop(handle);
            let _ = std::fs::remove_file(&file);
            return Err(error.into());
        }
        Ok(Self {
            data: SessionData {
                file,
                cwd: cwd.as_path().to_path_buf(),
                entries,
                session_id: header.id,
                header_timestamp: header.timestamp,
            },
            append_error: None,
        })
    }

    /// 以写入方式打开已有日志：检查文件头 ID，截掉没写完的最后一行，完整的记录不动。
    /// 同一个日志同时只能有一个写者。
    pub fn open_existing(path: &Path, expected_id: &str) -> Result<Self> {
        let (data, repair) = parse_session_file(path)?;
        data.verify_session_id(expected_id)?;
        if let Some(offset) = repair {
            repair_tail(path, offset)?;
        }
        Ok(Self { data, append_error: None })
    }
}

impl SessionData {
    /// 这个任务的图片目录：`sessions/images/<任务 ID>`。
    pub fn image_directory(&self) -> PathBuf {
        let parent = self.file.parent().expect("session has a parent directory");
        super::session_images_directory(parent, &self.session_id)
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
            .flat_map(Message::images)
            .find(|image| image.id == image_id)
            .ok_or_else(|| SessionError::InvalidSession("图片不在该任务历史中。".into()))?;
        Ok(crate::image::load_image(&self.image_directory(), attachment)?)
    }

    /// 只读打开日志，用于任务列表、预览和历史读取。
    ///
    /// 只读取以换行结束的完整记录，不修改文件；截掉残缺末尾由写者打开时负责。
    pub fn open(path: &Path) -> Result<Self> {
        parse_session_file(path).map(|(data, _)| data)
    }
}

impl SessionManager {
    /// 追加一条摘要记录，ID 使用压缩开始时分配给界面条目的 ID。返回保存时间。
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

    /// 追加一条任务设置（模型选择或名称），不进入模型上下文。返回保存时间。
    pub fn append_metadata(&mut self, metadata: SessionMetadata) -> Result<String> {
        self.append_entry(SessionEntry::Metadata {
            id: super::new_entry_id(),
            timestamp: now_iso(),
            metadata,
        })
    }

    /// 追加一条执行记录（回合开始与结束、请求、后台命令等），用于显示和统计，
    /// 不进入模型上下文。返回保存时间。
    pub fn append_record(&mut self, record: LedgerRecord) -> Result<String> {
        self.append_record_with_id(super::new_entry_id(), record)
    }

    /// 使用事先分配的 ID 追加执行记录，保存的内容和实时显示的条目对应同一个 ID。
    pub fn append_record_with_id(&mut self, id: String, record: LedgerRecord) -> Result<String> {
        self.append_entry(SessionEntry::Record { id, timestamp: now_iso(), record })
    }

    /// 使用事先分配的 ID 追加一条消息（回复、用户输入或工具结果）。
    pub(crate) fn append_message_with_id(
        &mut self,
        id: &str,
        message: Message,
        observation: Option<singularity_protocol::RequestObservation>,
    ) -> Result<String> {
        self.append_entry(SessionEntry::Message {
            id: id.to_string(),
            timestamp: now_iso(),
            message,
            observation,
        })
    }

    pub(super) fn append_entry(&mut self, entry: SessionEntry) -> Result<String> {
        // 上次写入失败可能留下残缺行，必须重新打开并截掉残缺末尾后才能继续追加。
        if let Some(error) = &self.append_error {
            return Err(SessionError::Io(std::io::Error::new(
                error.kind(),
                format!("previous session append failed; reopen the writer to repair its tail: {error}"),
            )));
        }
        let timestamp = entry.timestamp().to_string();
        let serialized = serde_json::to_string(&entry)?;
        let mut handle = OpenOptions::new().append(true).open(&self.file)?;
        // 记下写入错误，阻止后续追加把新记录拼在残缺行后面。
        handle
            .write_all(serialized.as_bytes())
            .and_then(|()| handle.write_all(b"\n"))
            .map_err(|error| {
                let error = Arc::new(error);
                self.append_error = Some(Arc::clone(&error));
                SessionError::Io(std::io::Error::new(error.kind(), error))
            })?;
        self.data.entries.push(entry);
        Ok(timestamp)
    }
}

impl SessionData {
    /// 会话日志路径，用于错误信息。
    pub fn path(&self) -> &Path {
        &self.file
    }

    /// 文件头里的会话 ID。
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// 检查文件头里的 ID 与文件名是否一致；不一致说明文件损坏或被改名。
    pub fn verify_session_id(&self, expected: &str) -> Result<()> {
        if self.session_id() == expected {
            Ok(())
        } else {
            Err(SessionError::InvalidHeader(format!(
                "rollout header id {} does not match expected id {expected}",
                self.session_id()
            )))
        }
    }

    /// 文件头里的创建时间。
    pub fn created_at(&self) -> &str {
        &self.header_timestamp
    }

    /// 任务的工作目录（规范化后的绝对路径）。
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// 工作目录的显示形式（去掉 Windows 长路径前缀），任务信息、列表和提示词共用。
    pub fn cwd_string(&self) -> String {
        singularity_core::display_path(&self.cwd)
    }

    /// 解析出的记录，按写入顺序排列。
    pub fn entries(&self) -> &[SessionEntry] {
        &self.entries
    }
}
