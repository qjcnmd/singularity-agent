//! 会话上下文投影与 LLM 消息转换。

use singularity_model::{ModelMessage, ModelRole};

use crate::message::{AgentMessageRole, COMPACTION_SUMMARY_PREFIX, COMPACTION_SUMMARY_SUFFIX};

use super::format::{Result, SessionEntry, SessionEntryType};
use super::manager::SessionManager;

impl SessionManager {
    /// 构建活跃的、compaction 感知的条目列表。
    pub fn build_context_entries(&self) -> Result<Vec<SessionEntry>> {
        let mut compaction_index = None;
        for (index, entry) in self.entries.iter().enumerate() {
            if matches!(entry.entry_type, SessionEntryType::Compaction(_)) {
                compaction_index = Some(index);
            }
        }
        let Some(compaction_index) = compaction_index else {
            return Ok(self.entries.clone());
        };
        let compaction = &self.entries[compaction_index];
        let first_kept = match &compaction.entry_type {
            SessionEntryType::Compaction(entry) => entry.first_kept_entry_id.clone(),
            _ => None,
        };
        let mut context = vec![compaction.clone()];
        let mut found_first_kept = false;
        for entry in &self.entries[..compaction_index] {
            if Some(entry.id.as_str()) == first_kept.as_deref() {
                found_first_kept = true;
            }
            if found_first_kept {
                context.push(entry.clone());
            }
        }
        context.extend_from_slice(&self.entries[compaction_index + 1..]);
        Ok(context)
    }
}

pub(crate) fn entry_to_llm_messages(entry: &SessionEntry) -> Vec<ModelMessage> {
    match &entry.entry_type {
        SessionEntryType::Message(message) => match message.role {
            AgentMessageRole::User => {
                vec![ModelMessage::text(ModelRole::User, message.content_text())]
            }
            AgentMessageRole::Assistant => {
                let tool_calls = message
                    .tool_calls()
                    .into_iter()
                    .filter_map(|block| block.to_model_tool_call())
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
