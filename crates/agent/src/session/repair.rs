//! 崩溃恢复与孤立工具结果修复。

use std::collections::{BTreeSet, HashSet};

use crate::message::{AgentMessage, AgentMessageRole, ContentBlock};

use super::format::{Result, SessionEntry, SessionMetadata, SessionMetadataKind};
use super::manager::SessionManager;
use singularity_protocol::{TurnModelUsage, TurnStatus};

impl SessionManager {
    /// 重开时把当前 leaf 上没有终态的 turn 标记为 synthetic interrupted。
    pub fn repair_interrupted_turns(&mut self) -> Result<usize> {
        // BTreeSet 保证 turn_terminal 追加顺序确定（同输入同输出）。
        let mut started = BTreeSet::new();
        let mut terminal = HashSet::new();
        for entry in &self.entries {
            let SessionEntry::Metadata { metadata, .. } = entry else {
                continue;
            };
            let Some(turn_id) = metadata.turn_id() else {
                continue;
            };
            match metadata.kind() {
                SessionMetadataKind::TurnStarted => {
                    started.insert(turn_id.to_string());
                }
                SessionMetadataKind::TurnTerminal => {
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
            self.append_metadata(SessionMetadata::turn_terminal(
                turn_id,
                TurnStatus::Interrupted,
                TurnModelUsage::default(),
            ))?;
            repaired += 1;
        }
        Ok(repaired)
    }

    /// 返回当前 leaf 路径上的 metadata。
    pub fn metadata_entries(&self) -> Vec<SessionMetadata> {
        self.entries
            .iter()
            .filter_map(|entry| match entry {
                SessionEntry::Metadata { metadata, .. } => Some(metadata.clone()),
                _ => None,
            })
            .collect()
    }
}

impl SessionManager {
    /// 修复活动路径中崩溃遗留的孤立 assistant tool call。单遍扫描：先收集
    /// 全部已配对的 tool_result id，再按 assistant source order 补 synthetic
    /// failed 结果（tool_call_id 在会话内全局唯一，全局配对与后缀配对等价）。
    pub fn repair_orphaned_tool_calls(&mut self) -> Result<usize> {
        let mut paired_tool_results: HashSet<String> = HashSet::new();
        let mut assistant_tool_calls: Vec<Vec<String>> = Vec::new();
        for entry in &self.entries {
            match entry {
                SessionEntry::Message { message, .. }
                    if message.role() == AgentMessageRole::Assistant =>
                {
                    let tool_call_ids: Vec<String> = message
                        .tool_calls()
                        .filter_map(|block| match block {
                            ContentBlock::ToolCall { id, .. } => Some(id.clone()),
                            _ => None,
                        })
                        .collect();
                    if !tool_call_ids.is_empty() {
                        assistant_tool_calls.push(tool_call_ids);
                    }
                }
                SessionEntry::Message { message, .. }
                    if message.role() == AgentMessageRole::ToolResult =>
                {
                    if let Some(tool_call_id) = message.tool_call_id() {
                        paired_tool_results.insert(tool_call_id.clone());
                    }
                }
                _ => {}
            }
        }
        let mut repaired = 0usize;
        for tool_call_ids in assistant_tool_calls {
            for tool_call_id in tool_call_ids {
                if paired_tool_results.contains(&tool_call_id) {
                    continue;
                }
                self.append_message(AgentMessage::ToolResult {
                    content: vec![ContentBlock::Text {
                        text: "[previous execution outcome unknown; do not retry]".to_string(),
                    }],
                    tool_call_id: Some(tool_call_id),
                    tool_name: None,
                    is_error: Some(true),
                })?;
                repaired += 1;
            }
        }
        Ok(repaired)
    }
}
