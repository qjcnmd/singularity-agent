//! Pure context and history projection.
//!
//! This module selects safe public history and renders it into model-facing messages without
//! owning AgentLoop orchestration or mutable execution state.

use std::collections::HashSet;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use singularity_model::{ModelMessage, ModelRole};
use singularity_tools::approximate_token_count;

use super::{
    ASSISTANT_MESSAGE_ROLE, AgentLoopInput, ContextCompactionOutcome, DEFAULT_MAX_CONTEXT_TOKENS,
    USER_MESSAGE_ROLE,
};

/// 为模型提供方请求选择公开上下文时使用的优先级。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum AgentContextItemPriority {
    System,
    CurrentTurn,
    Evidence,
    History,
}

impl AgentContextItemPriority {
    fn rank(&self) -> u8 {
        match self {
            Self::CurrentTurn => 0,
            Self::System => 1,
            Self::Evidence => 2,
            Self::History => 3,
        }
    }
}

/// 在可见性允许时可以投影到模型历史中的上下文条目。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AgentContextItem {
    pub item_id: String,
    pub role: String,
    pub content: String,
    pub priority: AgentContextItemPriority,
    pub token_count: u32,
    pub public: bool,
    pub evaluator_only: bool,
}

impl AgentContextItem {
    /// 构造用户上下文项。
    pub fn user(item_id: impl Into<String>, content: impl Into<String>) -> Self {
        let content = content.into();
        Self {
            item_id: item_id.into(),
            role: USER_MESSAGE_ROLE.to_string(),
            token_count: approximate_token_count(&content),
            content,
            priority: AgentContextItemPriority::CurrentTurn,
            public: true,
            evaluator_only: false,
        }
    }

    /// 构造历史用户消息上下文项。
    pub fn history_user(item_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self::history(item_id, content, USER_MESSAGE_ROLE)
    }

    /// 构造历史 assistant 消息上下文项。
    pub fn history_assistant(item_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self::history(item_id, content, ASSISTANT_MESSAGE_ROLE)
    }

    fn history(item_id: impl Into<String>, content: impl Into<String>, role: &'static str) -> Self {
        let item_id = item_id.into();
        let content = content.into();
        Self {
            item_id,
            role: role.to_string(),
            token_count: approximate_token_count(&content),
            content,
            priority: AgentContextItemPriority::History,
            public: true,
            evaluator_only: false,
        }
    }

    pub(super) fn into_safe_history(self) -> Option<Self> {
        if self.priority != AgentContextItemPriority::History || !self.public || self.evaluator_only
        {
            return None;
        }
        match self.role.as_str() {
            USER_MESSAGE_ROLE => Some(Self::history_user(self.item_id, self.content)),
            ASSISTANT_MESSAGE_ROLE => Some(Self::history_assistant(self.item_id, self.content)),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct ContextBudget {
    pub(super) model_context_window: Option<u32>,
    pub(super) reserved_output_tokens: u32,
    pub(super) fixed_overhead_tokens: u32,
    pub(super) developer_instruction_tokens: u32,
    pub(super) tool_tokens: u32,
    pub(super) message_framing_tokens: u32,
    pub(super) input_token_budget: Option<u32>,
}

impl ContextBudget {
    fn reserved_request_tokens(&self) -> u32 {
        self.reserved_output_tokens
            .saturating_add(self.fixed_overhead_tokens)
            .saturating_add(self.developer_instruction_tokens)
            .saturating_add(self.tool_tokens)
            .saturating_add(self.message_framing_tokens)
    }

    fn metadata(&self, message_tokens: u32) -> Value {
        json!({
            "model_context_window": self.model_context_window,
            "input_token_budget": self.input_token_budget,
            "reserved_output_tokens": self.reserved_output_tokens,
            "fixed_overhead_tokens": self.fixed_overhead_tokens,
            "developer_instruction_tokens": self.developer_instruction_tokens,
            "tool_tokens": self.tool_tokens,
            "message_framing_tokens": self.message_framing_tokens,
            "reserved_request_tokens": self.reserved_request_tokens(),
            "message_tokens": message_tokens,
        })
    }

    pub(super) fn for_public_assembly(max_tokens: u32) -> Self {
        Self {
            model_context_window: Some(DEFAULT_MAX_CONTEXT_TOKENS),
            reserved_output_tokens: 0,
            fixed_overhead_tokens: 0,
            developer_instruction_tokens: 0,
            tool_tokens: 0,
            message_framing_tokens: 0,
            input_token_budget: Some(max_tokens),
        }
    }
}
/// 选择符合请求令牌预算的公开当前 turn 条目和最新历史。
pub fn assemble_context_items(items: &[AgentContextItem], max_tokens: u32) -> ContextBundle {
    let budget = ContextBudget::for_public_assembly(max_tokens);
    assemble_context_items_with_budget(items, &budget)
}

pub(super) fn assemble_context_items_with_budget(
    items: &[AgentContextItem],
    budget: &ContextBudget,
) -> ContextBundle {
    let max_tokens = budget.input_token_budget;
    let mut candidates: Vec<(usize, &AgentContextItem)> = items
        .iter()
        .enumerate()
        .filter(|(_, item)| {
            item.public
                && !item.evaluator_only
                && item.priority != AgentContextItemPriority::History
        })
        .collect();
    candidates.sort_by_key(|(index, item)| (item.priority.rank(), *index));

    let mut used_tokens = 0;
    let mut included_indices = HashSet::new();
    for (index, item) in candidates {
        let item_tokens = context_item_token_count(item);
        if max_tokens.is_some_and(|max_tokens| item_tokens > max_tokens.saturating_sub(used_tokens))
        {
            continue;
        }
        used_tokens = used_tokens.saturating_add(item_tokens);
        included_indices.insert(index);
    }

    let history_indices: Vec<usize> = items
        .iter()
        .enumerate()
        .filter(|(_, item)| {
            item.public
                && !item.evaluator_only
                && item.priority == AgentContextItemPriority::History
        })
        .map(|(index, _)| index)
        .collect();
    let mut history_end = history_indices.len();
    while history_end > 0 {
        let mut history_start = history_end - 1;
        let newest = &items[history_indices[history_start]];
        if newest.role == ASSISTANT_MESSAGE_ROLE
            && history_start > 0
            && items[history_indices[history_start - 1]].role == USER_MESSAGE_ROLE
        {
            history_start -= 1;
        }
        let group_tokens =
            history_indices[history_start..history_end]
                .iter()
                .fold(0u32, |total, index| {
                    total.saturating_add(context_item_token_count(
                        items.get(*index).expect("history item index remains bound"),
                    ))
                });
        if max_tokens
            .is_some_and(|max_tokens| group_tokens > max_tokens.saturating_sub(used_tokens))
        {
            break;
        }
        used_tokens = used_tokens.saturating_add(group_tokens);
        included_indices.extend(history_indices[history_start..history_end].iter().copied());
        history_end = history_start;
    }

    let mut included_item_ids = Vec::new();
    let mut excluded_item_ids = Vec::new();
    let mut messages = Vec::new();
    for (index, item) in items.iter().enumerate() {
        if !included_indices.contains(&index) {
            excluded_item_ids.push(item.item_id.clone());
            continue;
        }
        included_item_ids.push(item.item_id.clone());
        messages.push(json!({
            "role": item.role,
            "content": item.content,
        }));
    }

    ContextBundle {
        messages,
        included_item_ids,
        excluded_item_ids,
        budget: budget.metadata(used_tokens),
    }
}

fn context_item_token_count(item: &AgentContextItem) -> u32 {
    item.token_count
}

pub(super) fn current_turn_excluded(input: &AgentLoopInput, context: &ContextBundle) -> bool {
    input.input.iter().any(|item| {
        item.priority == AgentContextItemPriority::CurrentTurn
            && item.public
            && !item.evaluator_only
            && !context.included_item_ids.contains(&item.item_id)
    })
}
/// 可直接交给模型提供方的上下文消息，以及用于追踪和诊断的纳入元数据。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ContextBundle {
    pub messages: Vec<Value>,
    pub included_item_ids: Vec<String>,
    pub excluded_item_ids: Vec<String>,
    pub budget: Value,
}

/// 随运行持久化的追踪安全上下文选择和压缩计数器。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct AgentContextTrace {
    pub included_item_ids: Vec<String>,
    pub excluded_item_ids: Vec<String>,
    pub budget: Value,
    #[serde(default)]
    pub compaction_count: u32,
    #[serde(default)]
    pub compacted_message_count: u32,
    #[serde(default)]
    pub last_compaction_before_tokens: Option<u32>,
    #[serde(default)]
    pub last_compaction_after_tokens: Option<u32>,
}

impl From<&ContextBundle> for AgentContextTrace {
    fn from(context: &ContextBundle) -> Self {
        Self {
            included_item_ids: context.included_item_ids.clone(),
            excluded_item_ids: context.excluded_item_ids.clone(),
            budget: context.budget.clone(),
            compaction_count: 0,
            compacted_message_count: 0,
            last_compaction_before_tokens: None,
            last_compaction_after_tokens: None,
        }
    }
}

impl AgentContextTrace {
    pub(super) fn refresh_context(&mut self, context: &ContextBundle) {
        self.included_item_ids = context.included_item_ids.clone();
        self.excluded_item_ids = context.excluded_item_ids.clone();
        self.budget = context.budget.clone();
    }

    pub(super) fn record_compaction(&mut self, outcome: &ContextCompactionOutcome) {
        self.compaction_count = self.compaction_count.saturating_add(1);
        self.compacted_message_count = self
            .compacted_message_count
            .saturating_add(outcome.compacted_message_count);
        self.last_compaction_before_tokens = Some(outcome.before_tokens);
        self.last_compaction_after_tokens = Some(outcome.after_tokens);
    }
}
pub(super) fn model_messages_from_context(context: &ContextBundle) -> Vec<ModelMessage> {
    context
        .messages
        .iter()
        .flat_map(|message| {
            let role = match message.get("role").and_then(Value::as_str) {
                Some("system") => ModelRole::System,
                Some("developer") => ModelRole::Developer,
                Some("assistant") => ModelRole::Assistant,
                Some("tool") => ModelRole::Tool,
                _ => ModelRole::User,
            };
            message
                .get("content")
                .and_then(Value::as_str)
                .map(|content| ModelMessage::text(role, content))
        })
        .collect()
}
