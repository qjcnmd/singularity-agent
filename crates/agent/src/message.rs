//! Pi 式 Agent 消息类型（语义基线：`@earendil-works/pi-coding-agent` v0.84.1 的
//! `packages/ai/src/types.ts` 与 `dist/core/session-manager.js`）。
//!
//! v4 会话格式：assistant 消息的 `content` 是 content block 数组
//! （Text/Thinking/ToolCall），一次模型响应 = 一条 assistant 消息（对齐 Pi
//! `AssistantMessage.content: (TextContent|ThinkingContent|ToolCall)[]`）；
//! 工具结果仍按 `toolCallId` 关联的独立 `toolResult` 消息回写（对齐 Pi
//! `ToolResultMessage`）。thinking 块随会话持久化，续接时投影为 provider
//! reasoning replay（N2 裁决）。

/// 可直接交给模型提供方的消息形态，复用 `singularity_model::ModelMessage`，
/// 避免与旧 AgentLoop 形成第二套 LLM 消息表示。
pub type LlmMessage = singularity_model::ModelMessage;

use serde_json::Value;
use singularity_model::{ModelTurnResponse, ProviderReasoningReplay};

use crate::tools::ToolExecution;

/// Pi AgentMessage 的 role 枚举。
///
/// 序列化为 Pi JSON 使用的 camelCase 小写 role（`user`/`assistant`/`toolResult`/
/// `bashExecution`/`custom`/`branchSummary`/`compactionSummary`）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AgentMessageRole {
    User,
    Assistant,
    ToolResult,
    BashExecution,
    Custom,
    BranchSummary,
    CompactionSummary,
}

/// 消息 content 的内容块（对齐 Pi `AssistantMessage.content` 联合类型）。
///
/// JSON 判别字段为 `type`：`text` / `thinking` / `tool_call`。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    /// 纯文本块（`{"type":"text","text":...}`）。
    Text { text: String },
    /// 推理文本块（`{"type":"thinking","thinking":...,"signature":...}`；
    /// 对齐 Pi `ThinkingContent{type:"thinking", thinking, signature}`）。
    Thinking {
        thinking: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    /// 工具调用块（`{"type":"tool_call","id":...,"name":...,"args":...}`；
    /// 对齐 Pi `ToolCall{id, name, args}`）。
    ToolCall {
        id: String,
        name: String,
        args: serde_json::Value,
    },
}

/// Pi 式会话消息 payload。
///
/// JSON 字段名对齐 Pi：`role`/`content`/`toolCallId`/`toolName`/`timestamp`。
/// v4 起 `content` 为 content block 数组；`toolCallId`/`toolName` 仅 toolResult
/// 消息使用（关联其对应的 assistant tool call）。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentMessage {
    pub role: AgentMessageRole,
    pub content: Vec<ContentBlock>,
    /// Provider-private continuation captured with an assistant tool-call response.
    ///
    /// This field is durable session state, not a user-visible content block.  The
    /// provider adapter owns its wire interpretation; the agent only carries it
    /// across reopen and request construction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_reasoning_replay: Option<ProviderReasoningReplay>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    /// unix 毫秒时间戳，对齐 Pi 消息 timestamp。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<u64>,
}

impl AgentMessage {
    /// 构造纯文本消息（v4 内容块形态）。
    pub fn text(role: AgentMessageRole, content: impl Into<String>) -> Self {
        Self {
            role,
            content: vec![ContentBlock::Text {
                text: content.into(),
            }],
            provider_reasoning_replay: None,
            tool_call_id: None,
            tool_name: None,
            timestamp: None,
        }
    }

    /// 拼接全部文本块的纯文本视图（摘要/估算/投影共用）。
    pub fn content_text(&self) -> String {
        let mut text = String::new();
        for block in &self.content {
            if let ContentBlock::Text { text: part } = block {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(part);
            }
        }
        text
    }

    /// 工具调用块列表（assistant 消息；一次响应多条调用都在同一消息内）。
    pub fn tool_calls(&self) -> Vec<&ContentBlock> {
        self.content
            .iter()
            .filter(|block| matches!(block, ContentBlock::ToolCall { .. }))
            .collect()
    }

    /// 是否有任何工具调用块。
    pub fn has_tool_calls(&self) -> bool {
        self.content
            .iter()
            .any(|block| matches!(block, ContentBlock::ToolCall { .. }))
    }

    /// thinking 块列表（N2：续接时投影 provider reasoning replay）。
    pub fn thinking_blocks(&self) -> Vec<&ContentBlock> {
        self.content
            .iter()
            .filter(|block| matches!(block, ContentBlock::Thinking { .. }))
            .collect()
    }
}

/// Pi `COMPACTION_SUMMARY_PREFIX`（messages.js）：compaction 摘要进入 LLM 上下文时的固定前缀。
pub const COMPACTION_SUMMARY_PREFIX: &str = "The conversation history before this point was compacted into the following summary:\n\n<summary>\n";
/// Pi `COMPACTION_SUMMARY_SUFFIX`（messages.js）。
pub const COMPACTION_SUMMARY_SUFFIX: &str = "\n</summary>";
/// Pi `BRANCH_SUMMARY_PREFIX`（messages.js）。
pub const BRANCH_SUMMARY_PREFIX: &str =
    "The following is a summary of a branch that this conversation came back from:\n\n<summary>\n";
/// Pi `BRANCH_SUMMARY_SUFFIX`（messages.js）。
pub const BRANCH_SUMMARY_SUFFIX: &str = "</summary>";

pub(crate) fn user_message(text: &str) -> AgentMessage {
    AgentMessage {
        role: AgentMessageRole::User,
        content: vec![ContentBlock::Text {
            text: text.to_string(),
        }],
        provider_reasoning_replay: None,
        tool_call_id: None,
        tool_name: None,
        timestamp: None,
    }
}

/// 一次模型响应投影为一条 assistant 消息（v4 内容块）：
/// thinking 块（N2，随会话持久化）→ 文本块 → 全部 tool_call 块。
pub(crate) fn assistant_response_message(response: &ModelTurnResponse) -> AgentMessage {
    let mut content = Vec::new();
    if let Some(thinking) = reasoning_text_from_replay(&response.provider_reasoning_history) {
        content.push(ContentBlock::Thinking {
            thinking,
            signature: None,
        });
    }
    let assistant_text = response
        .assistant_message
        .as_ref()
        .map(|message| message.content.clone())
        .unwrap_or_default();
    if !assistant_text.is_empty() {
        content.push(ContentBlock::Text {
            text: assistant_text,
        });
    }
    for call in &response.tool_calls {
        content.push(ContentBlock::ToolCall {
            id: call.tool_call_id.clone(),
            name: call.tool_name.clone(),
            args: call.arguments.clone(),
        });
    }
    AgentMessage {
        role: AgentMessageRole::Assistant,
        content,
        provider_reasoning_replay: response.provider_reasoning_history.first().cloned(),
        tool_call_id: None,
        tool_name: None,
        timestamp: None,
    }
}

/// 从响应携带的 provider reasoning replay 提取可展示的推理文本：
/// Chat replay 直接取 `reasoning_content`；Responses replay 从 reasoning item 的
/// `summary` 段尽力提取（OpenAI `{"type":"reasoning","summary":[{"type":"summary_text",...}]}`）。
pub(crate) fn reasoning_text_from_replay(replay: &[ProviderReasoningReplay]) -> Option<String> {
    let first = replay.first()?;
    match first {
        ProviderReasoningReplay::Chat {
            reasoning_content, ..
        } if !reasoning_content.is_empty() => Some(reasoning_content.clone()),
        ProviderReasoningReplay::Responses { items, .. } => items.iter().find_map(|item| {
            if item.get("type").and_then(Value::as_str) != Some("reasoning") {
                return None;
            }
            let summary = item
                .get("summary")
                .and_then(Value::as_array)
                .map(|parts| {
                    parts
                        .iter()
                        .filter_map(|part| part.get("text").and_then(Value::as_str))
                        .collect::<Vec<_>>()
                        .join("")
                })
                .unwrap_or_default();
            (!summary.is_empty()).then_some(summary)
        }),
        _ => None,
    }
}

pub(crate) fn tool_result_message(
    tool_call_id: &str,
    tool_name: &str,
    execution: &ToolExecution,
) -> AgentMessage {
    AgentMessage {
        role: AgentMessageRole::ToolResult,
        content: vec![ContentBlock::Text {
            text: execution.content.clone(),
        }],
        provider_reasoning_replay: None,
        tool_call_id: Some(tool_call_id.to_string()),
        tool_name: Some(tool_name.to_string()),
        timestamp: None,
    }
}
