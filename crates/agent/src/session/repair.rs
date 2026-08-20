//! Crash recovery and orphan-tool-result repair.

use std::collections::HashSet;

use crate::message::{AgentMessage, AgentMessageRole, ContentBlock};

pub use super::format::SessionError;
use super::format::{Result, SessionEntryType, SessionMetadata, SessionMetadataKind};
pub use super::manager::SessionManager;

impl SessionManager {
    /// 重开时把当前 leaf 上没有终态的 turn 标记为 synthetic interrupted。
    pub fn repair_interrupted_turns(&mut self) -> Result<usize> {
        let path = self.session_path();
        let mut started = HashSet::new();
        let mut terminal = HashSet::new();
        for &entry_index in &path {
            let SessionEntryType::Metadata(metadata) = &self.entries[entry_index].entry_type else {
                continue;
            };
            let Some(turn_id) = metadata.turn_id() else {
                continue;
            };
            match metadata.kind() {
                SessionMetadataKind::TurnStarted => {
                    started.insert(turn_id.to_string());
                }
                SessionMetadataKind::TurnCompleted
                | SessionMetadataKind::TurnFailed
                | SessionMetadataKind::TurnInterrupted => {
                    terminal.insert(turn_id.to_string());
                }
                _ => {}
            }
        }
        let mut repaired = 0;
        for turn_id in started {
            if terminal.contains(&turn_id) {
                continue;
            }
            self.append_metadata(SessionMetadata::turn_interrupted(
                turn_id,
                "session reopened with an incomplete turn",
                true,
            ))?;
            repaired += 1;
        }
        Ok(repaired)
    }

    /// 返回当前 leaf 路径上的 metadata。
    pub fn metadata_entries(&self) -> Vec<SessionMetadata> {
        self.session_path()
            .into_iter()
            .filter_map(|index| match &self.entries[index].entry_type {
                SessionEntryType::Metadata(metadata) => Some(metadata.clone()),
                _ => None,
            })
            .collect()
    }
}

impl SessionManager {
    /// 修复活动路径中崩溃遗留的孤立 assistant tool call。
    pub fn repair_orphaned_tool_calls(&mut self) -> Result<usize> {
        let path = self.session_path();
        let mut repaired = 0usize;
        for &entry_index in &path {
            let tool_call_ids: Vec<String> = match &self.entries[entry_index].entry_type {
                SessionEntryType::Message(message)
                    if message.role == AgentMessageRole::Assistant =>
                {
                    message
                        .tool_calls()
                        .into_iter()
                        .filter_map(|block| match block {
                            ContentBlock::ToolCall { id, .. } => Some(id.clone()),
                            _ => None,
                        })
                        .collect()
                }
                _ => Vec::new(),
            };
            for tool_call_id in tool_call_ids {
                let paired = path[path
                    .iter()
                    .position(|&index| index == entry_index)
                    .expect("path index")
                    + 1..]
                    .iter()
                    .any(|&later_index| {
                        matches!(
                            &self.entries[later_index].entry_type,
                            SessionEntryType::Message(message)
                                if message.role == AgentMessageRole::ToolResult
                                    && message.tool_call_id.as_deref()
                                        == Some(tool_call_id.as_str())
                        )
                    });
                if paired {
                    continue;
                }
                self.append_entry(SessionEntryType::Message(AgentMessage {
                    role: AgentMessageRole::ToolResult,
                    content: vec![ContentBlock::Text {
                        text: "[previous execution outcome unknown; do not retry]".to_string(),
                    }],
                    provider_reasoning_replay: None,
                    tool_call_id: Some(tool_call_id),
                    tool_name: None,
                    is_error: Some(true),
                    timestamp: None,
                }))?;
                repaired += 1;
            }
        }
        Ok(repaired)
    }
}
