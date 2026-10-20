//! 模型使用的指令、工具定义，以及会话内可展开的注入记录。
use super::LedgerRecord;
use super::manager::SessionManager;
use singularity_model::ModelMessage;
use singularity_protocol::{RequestMessage, RequestTool};

/// 本轮模型使用的固定请求内容；模型投影与 Token 估价由这里负责。
#[derive(Debug, Clone)]
pub struct RequestDefinitions {
    pub(crate) messages: Vec<RequestMessage>,
    pub(crate) tools: Vec<RequestTool>,
}

impl RequestDefinitions {
    pub(crate) fn sections(&self) -> Vec<(String, String)> {
        let mut sections: Vec<_> = self
            .messages
            .iter()
            .map(|message| (message.name.clone(), message.content.clone()))
            .collect();
        sections.push((
            "工具定义".into(),
            serde_json::to_string_pretty(&self.tools).expect("tool schemas are serializable"),
        ));
        sections
    }

    /// 指令和工具定义共同占用的输入预算。
    pub(crate) fn estimated_tokens(&self) -> u64 {
        let instructions = self
            .messages
            .iter()
            .map(|message| super::context::estimate_tokens_of(&message.content) + 4)
            .sum::<u64>();
        let tools = if self.tools.is_empty() {
            0
        } else {
            super::context::estimate_tokens_of(
                &serde_json::to_string(&self.tools).expect("tool schemas are serializable"),
            ) + 4
        };
        instructions + tools
    }

    /// 生成本轮模型请求的指令前缀。
    pub(crate) fn model_messages(&self) -> Vec<ModelMessage> {
        self.messages
            .iter()
            .map(|message| ModelMessage::text(message.role, &message.content))
            .collect()
    }
}

impl SessionManager {
    /// 每轮保存实际注入的内容，直接供会话展示。
    pub(crate) fn record_context(
        &mut self,
        sections: Vec<(String, String)>,
    ) -> super::Result<Vec<singularity_protocol::HistoryItem>> {
        let mut items = Vec::new();
        for (name, content) in sections {
            let id = super::new_entry_id();
            self.append_record_with_id(
                id.clone(),
                LedgerRecord::ContextInjected {
                    name: name.clone(),
                    content: content.clone(),
                },
            )?;
            items.push(singularity_protocol::HistoryItem::Context { id, name, content });
        }
        Ok(items)
    }
}
