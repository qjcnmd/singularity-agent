use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use app_core::now_iso;

use model::Message;

use super::file::{parse_session_file, repair_tail, scan_session_file};
use super::format::{
    CompactionEntry, LedgerRecord, MessageDisplay, Result, SessionEntry, SessionError, SessionHeader,
    SessionMetadata,
};

#[derive(Debug)]
pub struct SessionManager {
    pub(super) data: SessionData,
    append_error: Option<Arc<std::io::Error>>,
}

#[derive(Debug)]
pub struct SessionData {
    pub(super) file: PathBuf,
    pub(super) cwd: PathBuf,
    pub(super) entries: Vec<SessionEntry>,
    pub(super) session_id: String,
    pub(super) header_timestamp: String,
}

impl SessionManager {
    pub fn data(&self) -> &SessionData {
        &self.data
    }
    pub fn create_with_id(
        cwd: &Path,
        sessions_dir: &Path,
        session_id: uuid::Uuid,
        selection: protocol::ModelSelection,
        disabled_modules: Vec<String>,
    ) -> Result<Self> {
        let session_id = session_id.to_string();
        let cwd = app_core::canonicalize_workspace(cwd).map_err(SessionError::InvalidSession)?;
        let cwd_display = app_core::display_path(&cwd);
        std::fs::create_dir_all(sessions_dir)?;
        let file = sessions_dir.join(super::session_file_name(&session_id));
        let header = SessionHeader::new(session_id, cwd_display, now_iso());
        let mut header_bytes = serde_json::to_vec(&header)?;
        header_bytes.push(b'\n');
        let metadata = [SessionMetadata::ThreadSettings { selection }].into_iter().chain(
            (!disabled_modules.is_empty())
                .then_some(SessionMetadata::DisabledModules { ids: disabled_modules }),
        );
        let entries: Vec<_> = metadata
            .map(|metadata| SessionEntry::Metadata {
                id: super::new_entry_id(),
                timestamp: header.timestamp.clone(),
                metadata,
            })
            .collect();
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
                cwd,
                entries,
                session_id: header.id,
                header_timestamp: header.timestamp,
            },
            append_error: None,
        })
    }

    pub fn open_existing(path: &Path, expected_id: &str) -> Result<Self> {
        let (data, repair) = parse_session_file(path, Some(expected_id))?;
        if let Some(offset) = repair {
            repair_tail(path, offset)?;
        }
        Ok(Self { data, append_error: None })
    }
}

impl SessionData {
    pub(crate) fn image_directory(&self) -> PathBuf {
        let parent = self.file.parent().expect("session has a parent directory");
        super::session_images_directory(parent, &self.session_id)
    }

    pub(crate) fn output_directory(&self) -> PathBuf {
        super::session_outputs_directory(
            self.file.parent().expect("session has a parent directory"),
            &self.session_id,
        )
    }

    pub fn image_attachment(&self, image_id: &str) -> Result<crate::image::SavedImage> {
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
        Ok(crate::image::SavedImage {
            directory: self.image_directory(),
            attachment: attachment.clone(),
        })
    }

    pub fn image_data(&self, image_id: &str) -> Result<String> {
        Ok(self.image_attachment(image_id)?.data_url()?)
    }

    pub fn open(path: &Path, expected_id: Option<&str>) -> Result<Self> {
        parse_session_file(path, expected_id).map(|(data, _)| data)
    }
}

pub fn scan_session_entries(
    path: &Path,
    expected_id: &str,
    visit: impl FnMut(SessionEntry),
) -> Result<(PathBuf, String)> {
    let (header, cwd, _) = scan_session_file(path, Some(expected_id), visit)?;
    Ok((cwd, header.timestamp))
}

fn latest_settings(entries: &[SessionEntry]) -> Option<&protocol::ModelSelection> {
    entries.iter().rev().find_map(|entry| match entry {
        SessionEntry::Metadata {
            metadata: SessionMetadata::ThreadSettings { selection },
            ..
        } => Some(selection),
        _ => None,
    })
}

fn latest_name(entries: &[SessionEntry]) -> Option<&str> {
    entries.iter().rev().find_map(|entry| match entry {
        SessionEntry::Metadata {
            metadata: SessionMetadata::ThreadName { name },
            ..
        } => Some(name.as_str()),
        _ => None,
    })
}

impl SessionManager {
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

    pub fn append_metadata(&mut self, metadata: SessionMetadata) -> Result<String> {
        self.append_entry(SessionEntry::Metadata {
            id: super::new_entry_id(),
            timestamp: now_iso(),
            metadata,
        })
    }

    pub fn append_record(&mut self, record: LedgerRecord) -> Result<String> {
        self.append_record_with_id(super::new_entry_id(), record)
    }

    pub(crate) fn append_record_with_id(&mut self, id: String, record: LedgerRecord) -> Result<String> {
        self.append_entry(SessionEntry::Record { id, timestamp: now_iso(), record })
    }

    pub fn append_operation_started(&mut self, turn_id: String) -> Result<String> {
        self.append_record_with_id(turn_id, LedgerRecord::OperationStarted)
    }

    pub fn rewind_before_message(&mut self, message_id: &str) -> Result<()> {
        let target_entry_id = message_id.strip_suffix(":text:0").unwrap_or(message_id);
        let msg_idx = self
            .data
            .entries
            .iter()
            .position(|entry| {
                matches!(
                    entry,
                    SessionEntry::Message {
                        id,
                        message: Message::User { .. },
                        ..
                    } if id == target_entry_id
                )
            })
            .ok_or_else(|| SessionError::InvalidSession(format!("未找到目标消息：{message_id}")))?;

        let open_turn_start = self.data.entries[..msg_idx]
            .iter()
            .rposition(|entry| {
                matches!(
                    entry,
                    SessionEntry::Record {
                        record: LedgerRecord::OperationStarted | LedgerRecord::OperationFinished { .. },
                        ..
                    }
                )
            })
            .filter(|&pos| {
                matches!(
                    self.data.entries[pos],
                    SessionEntry::Record {
                        record: LedgerRecord::OperationStarted,
                        ..
                    }
                )
            });

        let (cut_idx, close_turn_id) = match open_turn_start {
            Some(start) => {
                let has_prior_content = self.data.entries[start + 1..msg_idx].iter().any(|entry| {
                    matches!(entry, SessionEntry::Message { .. } | SessionEntry::Compaction { .. })
                });
                if has_prior_content {
                    (msg_idx, Some(self.data.entries[start].id().to_owned()))
                } else {
                    (start, None)
                }
            }
            None => (msg_idx, None),
        };

        let (kept, dropped) = self.data.entries.split_at(cut_idx);
        let mut preserved = Vec::new();
        if let Some(turn_id) = close_turn_id {
            let timestamp = kept.last().map_or_else(now_iso, |entry| entry.timestamp().to_owned());
            preserved.push(SessionEntry::Record {
                id: super::new_entry_id(),
                timestamp,
                record: LedgerRecord::OperationFinished {
                    turn_id,
                    outcome: protocol::TurnStatus::Completed,
                    error: None,
                    user_stopped: false,
                },
            });
        }
        if let Some(selection) = latest_settings(dropped)
            && latest_settings(kept) != Some(selection)
        {
            preserved.push(SessionEntry::Metadata {
                id: super::new_entry_id(),
                timestamp: now_iso(),
                metadata: SessionMetadata::ThreadSettings { selection: selection.clone() },
            });
        }
        if let Some(name) = latest_name(dropped)
            && latest_name(kept) != Some(name)
        {
            preserved.push(SessionEntry::Metadata {
                id: super::new_entry_id(),
                timestamp: now_iso(),
                metadata: SessionMetadata::ThreadName { name: name.to_owned() },
            });
        }

        let header = SessionHeader::new(
            self.data.session_id.clone(),
            self.data.cwd_string(),
            self.data.header_timestamp.clone(),
        );
        let mut bytes = serde_json::to_vec(&header)?;
        bytes.push(b'\n');
        for entry in kept.iter().chain(&preserved) {
            serde_json::to_writer(&mut bytes, entry)?;
            bytes.push(b'\n');
        }
        app_core::write_bytes_atomic(&self.data.file, &bytes)?;

        self.data.entries.truncate(cut_idx);
        self.data.entries.extend(preserved);
        self.append_error = None;
        Ok(())
    }

    pub(crate) fn append_message_with_id(
        &mut self,
        id: &str,
        message: Message,
        observation: Option<protocol::RequestObservation>,
        display: Option<MessageDisplay>,
    ) -> Result<String> {
        self.append_entry(SessionEntry::Message {
            id: id.to_string(),
            timestamp: now_iso(),
            message,
            observation,
            display,
        })
    }

    fn append_entry(&mut self, entry: SessionEntry) -> Result<String> {
        if let Some(error) = &self.append_error {
            return Err(SessionError::Io(std::io::Error::new(
                error.kind(),
                format!("previous session append failed; reopen the writer to repair its tail: {error}"),
            )));
        }
        let timestamp = entry.timestamp().to_string();
        let mut bytes = serde_json::to_vec(&entry)?;
        bytes.push(b'\n');
        let mut handle = OpenOptions::new().append(true).open(&self.data.file)?;
        handle.write_all(&bytes).map_err(|error| {
            let error = Arc::new(error);
            self.append_error = Some(Arc::clone(&error));
            SessionError::Io(std::io::Error::new(error.kind(), error))
        })?;
        self.data.entries.push(entry);
        Ok(timestamp)
    }
}

impl SessionData {
    pub fn path(&self) -> &Path {
        &self.file
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn created_at(&self) -> &str {
        &self.header_timestamp
    }

    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    pub fn cwd_string(&self) -> String {
        app_core::display_path(&self.cwd)
    }

    pub fn entries(&self) -> &[SessionEntry] {
        &self.entries
    }

    pub fn disabled_modules(&self) -> Vec<String> {
        self.entries
            .iter()
            .find_map(|entry| match entry {
                SessionEntry::Metadata {
                    metadata: SessionMetadata::DisabledModules { ids },
                    ..
                } => Some(ids.clone()),
                _ => None,
            })
            .unwrap_or_default()
    }
}
