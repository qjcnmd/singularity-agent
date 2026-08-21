//! Session context projection and LLM message conversion.

use singularity_model::{ModelMessage, ModelRole, ModelToolCall, ModelToolParseStatus};

use crate::message::{
    AgentMessageRole, COMPACTION_SUMMARY_PREFIX, COMPACTION_SUMMARY_SUFFIX, ContentBlock,
    LlmMessage,
};

use super::format::{Result, SessionEntry, SessionEntryType};
use super::manager::SessionManager;

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

    /// 构建发送给 LLM 的会话上下文消息序列。
    pub fn build_session_context(&self) -> Result<Vec<LlmMessage>> {
        Ok(self
            .build_context_entries()?
            .iter()
            .flat_map(entry_to_llm_messages)
            .collect())
    }

    pub(super) fn session_path(&self) -> Vec<usize> {
        // 会话是严格的线性序列：事实源 `entries` 的物理顺序就是路径顺序，
        // 不存在回溯/分叉，直接从 0 到末尾。
        (0..self.entries.len()).collect()
    }
}

pub(crate) fn entry_to_llm_messages(entry: &SessionEntry) -> Vec<LlmMessage> {
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
