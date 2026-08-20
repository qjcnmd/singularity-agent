//! Session context projection and LLM message conversion.

use std::collections::HashSet;

use singularity_model::{ModelMessage, ModelRole, ModelToolCall, ModelToolParseStatus};

use crate::message::{
    AgentMessageRole, COMPACTION_SUMMARY_PREFIX, COMPACTION_SUMMARY_SUFFIX, ContentBlock,
    LlmMessage,
};

use super::format::{Result, SessionEntry, SessionEntryType, SessionMetadataKind};
use super::manager::SessionManager;
/// 会话上下文的 LLM 消息序列与恢复所需的模型设置。
#[derive(Debug, Clone, PartialEq)]
pub struct SessionContext {
    pub messages: Vec<LlmMessage>,
    pub model: Option<String>,
    pub thinking_level: Option<String>,
}

impl SessionManager {
    /// 构建活跃的、compaction 感知的条目列表。
    pub fn build_context_entries(&self) -> Result<Vec<SessionEntry>> {
        let path = self.session_path();
        let mut compaction_index = None;
        for (index, &entry_index) in path.iter().enumerate() {
            if matches!(
                self.entries[entry_index].entry_type,
                SessionEntryType::Compaction(_)
            ) {
                compaction_index = Some(index);
            }
        }
        let Some(compaction_index) = compaction_index else {
            return Ok(path
                .iter()
                .map(|&index| self.entries[index].clone())
                .collect());
        };
        let compaction = &self.entries[path[compaction_index]];
        let first_kept = match &compaction.entry_type {
            SessionEntryType::Compaction(entry) => entry.first_kept_entry_id.clone(),
            _ => None,
        };
        let mut context = vec![compaction.clone()];
        let mut found_first_kept = false;
        for &entry_index in &path[..compaction_index] {
            let entry = &self.entries[entry_index];
            if Some(entry.id.as_str()) == first_kept.as_deref() {
                found_first_kept = true;
            }
            if found_first_kept {
                context.push(entry.clone());
            }
        }
        context.extend(
            path[compaction_index + 1..]
                .iter()
                .map(|&entry_index| self.entries[entry_index].clone()),
        );
        Ok(context)
    }

    /// 构建发送给 LLM 的会话上下文。
    pub fn build_session_context(&self) -> Result<SessionContext> {
        let mut model = None;
        for entry_index in self.session_path() {
            if let SessionEntryType::Metadata(metadata) = &self.entries[entry_index].entry_type
                && metadata.kind() == SessionMetadataKind::ThreadSettings
                && let (Some(provider), Some(model_id)) = (
                    metadata.field_string("provider"),
                    metadata.field_string("model"),
                )
            {
                model = Some(if provider.is_empty() {
                    model_id.to_string()
                } else {
                    format!("{provider}/{model_id}")
                });
            }
        }
        let messages = self
            .build_context_entries()?
            .iter()
            .flat_map(entry_to_llm_messages)
            .collect();
        Ok(SessionContext {
            messages,
            model,
            thinking_level: None,
        })
    }

    pub(super) fn session_path(&self) -> Vec<usize> {
        let Some(leaf_id) = &self.leaf_id else {
            return Vec::new();
        };
        let current = self
            .by_id
            .get(leaf_id)
            .copied()
            .or_else(|| self.entries.len().checked_sub(1));
        let Some(mut current) = current else {
            return Vec::new();
        };
        let mut path = Vec::new();
        let mut visited = HashSet::new();
        loop {
            if !visited.insert(current) {
                break;
            }
            path.push(current);
            let parent = &self.entries[current].parent_id;
            current = match self.by_id.get(parent) {
                Some(&next) => next,
                None => break,
            };
        }
        path.reverse();
        path
    }
}

pub(super) fn entry_to_llm_messages(entry: &SessionEntry) -> Vec<LlmMessage> {
    match &entry.entry_type {
        SessionEntryType::Message(message) => match message.role {
            AgentMessageRole::User => {
                vec![ModelMessage::text(ModelRole::User, message.content_text())]
            }
            AgentMessageRole::Assistant => {
                let tool_calls = message
                    .tool_calls()
                    .into_iter()
                    .filter_map(|block| match block {
                        ContentBlock::ToolCall { id, name, args } => {
                            if id.trim().is_empty() || name.trim().is_empty() {
                                return None;
                            }
                            Some(ModelToolCall {
                                tool_call_id: id.clone(),
                                tool_name: name.clone(),
                                arguments: args.clone(),
                                raw_arguments: serde_json::to_string(args).unwrap_or_default(),
                                parse_status: ModelToolParseStatus::Valid,
                                validation_errors: Vec::new(),
                            })
                        }
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                if tool_calls.is_empty() {
                    return vec![ModelMessage::text(
                        ModelRole::Assistant,
                        message.content_text(),
                    )];
                }
                let mut llm = ModelMessage::assistant_tool_calls(tool_calls);
                llm.content = message.content_text();
                vec![llm]
            }
            AgentMessageRole::ToolResult => {
                let mut llm = ModelMessage::text(ModelRole::Tool, message.content_text());
                llm.tool_call_id = message.tool_call_id.clone();
                vec![llm]
            }
        },
        SessionEntryType::Compaction(compaction) => vec![ModelMessage::text(
            ModelRole::User,
            format!(
                "{COMPACTION_SUMMARY_PREFIX}{}{COMPACTION_SUMMARY_SUFFIX}",
                compaction.summary
            ),
        )],
        SessionEntryType::Metadata(_) => Vec::new(),
    }
}
