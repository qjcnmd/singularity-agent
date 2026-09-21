//! 会话消息与内容块的数据模型。
//!
//! 内容块覆盖正文、思考链、工具调用与工具结果，一次模型交互的完整语义（推理过程、
//! 多个工具调用）都能原样落盘并在协议重放时还原；序列化 wire 形状与历史平铺格式
//! 逐字节一致，由 session 层的 JSONL 字节夹具钉住这个契约。

use singularity_model::{
    ModelMessage, ModelStopReason, ModelToolCall, ModelTurnResponse, ProviderReasoningReplay,
};

use crate::tools::ToolExecution;

/// 公开内容的投影范围：工具的生命周期由工具自己的 start/end 事件表达，而历史归约
/// 需要工具项才能和结果配对，所以同一条消息的两类消费方需要的块并不一样。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemScope {
    /// 完整的公开历史：正文、思考和工具调用都包含在内。
    History,
    /// 实时 assistant 完成事件：只含正文和思考。
    Completion,
}

/// 消息体内部的结构化内容块；一条消息的内容就是若干块组成的有序列表。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ContentBlock {
    /// 纯文本内容块。
    Text { text: String },
    /// 只放对外公开的思考文本；provider 私有的续接材料由 Assistant 的
    /// provider_reasoning_replay 单独保存，不混进内容块。
    Thinking { thinking: String },
    /// 载荷直接复用模型层的 `ModelToolCall`，不重复定义同义字段。
    ToolCall(ModelToolCall),
}

/// 核心会话消息结构：以角色为标签的枚举，每个角色只携带自己合法的字段。
///
/// 序列化结果形如 {"content":...,"role":"user"} / {"role":"assistant",...,
/// "stopReason":...} / {"role":"toolResult":...,"toolCallId":...,"isError":...}。
/// deny_unknown_fields 让消息里出现未知字段时直接拒绝。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "role", rename_all = "camelCase", deny_unknown_fields)]
pub enum AgentMessage {
    #[serde(rename_all = "camelCase")]
    User { content: Vec<ContentBlock> },
    #[serde(rename_all = "camelCase")]
    Assistant {
        content: Vec<ContentBlock>,
        /// Provider 报告的 assistant 停止原因。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stop_reason: Option<ModelStopReason>,
        /// 模型提供方的私有推理状态，用于 Responses 这类协议重放推理的连续性。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_reasoning_replay: Option<ProviderReasoningReplay>,
    },
    #[serde(rename_all = "camelCase")]
    ToolResult {
        content: Vec<ContentBlock>,
        /// 对应的工具调用 ID；名称和参数由原始 ToolCall 记录提供，这里不重复。
        tool_call_id: String,
        /// 工具执行是否失败。
        is_error: bool,
        /// 观测到的工具执行耗时；结果未知或没有执行时缺省。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
        /// 用于展示的文件改动；不会包含在发给模型的内容里。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        diff: Option<String>,
        /// read 实际读取到的源文件范围；只有 read 的成功结果才带这个字段。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        read_source: Option<singularity_protocol::ReadSource>,
    },
}

impl AgentMessage {
    /// 用户和助手消息的公开内容，历史与完成事件共用；私有续接材料不进入投影，
    /// 工具结果必须由历史归约去绑定对应的调用，不在这里投影。
    ///
    /// 筛选在块映射之前完成，所以被排除的块不会产生任何临时对象。
    pub fn public_items(
        &self,
        entry_id: &str,
        scope: ItemScope,
    ) -> Vec<singularity_protocol::HistoryItem> {
        use singularity_protocol::HistoryItem;
        let role = match self {
            Self::User { .. } => "user",
            Self::Assistant { .. } => "assistant",
            Self::ToolResult { .. } => return Vec::new(),
        };
        let include_tool_calls = matches!(scope, ItemScope::History);
        let (mut text_index, mut thinking_index, mut call_index) = (0, 0, 0);
        self.content()
            .iter()
            .filter_map(|block| {
                Some(match block {
                    ContentBlock::Text { text } if !text.is_empty() => {
                        let id = crate::session::text_item_id(entry_id, text_index);
                        text_index += 1;
                        HistoryItem::Message {
                            id,
                            role: role.into(),
                            text: text.clone(),
                        }
                    }
                    ContentBlock::Thinking { thinking } if !thinking.is_empty() => {
                        let id = crate::session::thinking_item_id(entry_id, thinking_index);
                        thinking_index += 1;
                        HistoryItem::Thinking {
                            id,
                            text: thinking.clone(),
                        }
                    }
                    ContentBlock::ToolCall(_) if !include_tool_calls => return None,
                    ContentBlock::ToolCall(call) => {
                        let id = crate::session::tool_item_id(entry_id, call_index);
                        call_index += 1;
                        HistoryItem::ToolCall {
                            id,
                            name: call.tool_name.clone(),
                            args: call.arguments.clone(),
                        }
                    }
                    _ => return None,
                })
            })
            .collect()
    }

    pub fn content(&self) -> &[ContentBlock] {
        match self {
            Self::User { content }
            | Self::Assistant { content, .. }
            | Self::ToolResult { content, .. } => content,
        }
    }

    pub fn content_text(&self) -> String {
        content_text(self.content())
    }

    /// 消息里的工具调用载荷；类型已经收窄，调用方不用再自己解包其他内容块。
    pub fn tool_calls(&self) -> impl Iterator<Item = &ModelToolCall> {
        self.content().iter().filter_map(|block| match block {
            ContentBlock::ToolCall(call) => Some(call),
            _ => None,
        })
    }

    /// 对应的工具调用 ID；返回 Option 只是因为有的角色不带调用身份，ToolResult 本身一定有调用 ID。
    pub fn tool_call_id(&self) -> Option<&str> {
        match self {
            Self::ToolResult { tool_call_id, .. } => Some(tool_call_id.as_str()),
            _ => None,
        }
    }

    /// provider 的推理重放数据；只有 assistant 消息会带。
    pub fn provider_reasoning_replay(&self) -> Option<&ProviderReasoningReplay> {
        match self {
            Self::Assistant {
                provider_reasoning_replay,
                ..
            } => provider_reasoning_replay.as_ref(),
            _ => None,
        }
    }
}

/// 压缩摘要节点进入模型上下文时加在前面的说明文字。
pub const COMPACTION_SUMMARY_PREFIX: &str = "This checkpoint summarizes earlier conversation history. Treat it as established background and continue directly from the messages that follow without acknowledging the checkpoint.\n\n<compacted-summary>\n";
/// 压缩摘要节点进入模型上下文时加在后面的闭合标记。
pub const COMPACTION_SUMMARY_SUFFIX: &str = "\n</compacted-summary>";

pub(crate) fn user_message(text: &str) -> AgentMessage {
    AgentMessage::User {
        content: vec![ContentBlock::Text {
            text: text.to_string(),
        }],
    }
}

/// 构造公开可见内容块的唯一规则：空思考和空正文各自跳过，顺序固定为
/// Thinking → Text；正常响应和失败时的可见部分都走这条规则，tool_calls、
/// stop_reason 和私有续接材料才是两条路径的差别。按值接收让正常路径直接移动模型
/// 响应里的字符串，失败路径只在确实要持久化时才构造拥有所有权的值。
pub(crate) fn public_thinking_text_blocks(thinking: String, text: String) -> Vec<ContentBlock> {
    let mut content = Vec::with_capacity(2);
    if !thinking.is_empty() {
        content.push(ContentBlock::Thinking { thinking });
    }
    if !text.is_empty() {
        content.push(ContentBlock::Text { text });
    }
    content
}

/// 把一次模型响应投影成一条 assistant 消息：公开 Thinking → 公开 Text →
/// 全部 tool_call 块。
///
/// 响应按值交接，交接过程不再复制。provider 的 stop_reason 随 Assistant 一起保存；
/// usage 由请求观测和 operation 终态统计链记录，不算会话内容。
pub(crate) fn assistant_response_message(response: ModelTurnResponse) -> AgentMessage {
    let ModelTurnResponse {
        assistant_message,
        thinking,
        stop_reason,
        ..
    } = response;
    let ModelMessage {
        content: text,
        tool_calls,
        provider_reasoning_replay,
        ..
    } = assistant_message;
    let mut content = public_thinking_text_blocks(thinking, text);
    content.extend(tool_calls.into_iter().map(ContentBlock::ToolCall));
    AgentMessage::Assistant {
        content,
        stop_reason,
        provider_reasoning_replay,
    }
}

pub(crate) fn tool_result_text(
    tool_call_id: &str,
    text: impl Into<String>,
    is_error: bool,
) -> AgentMessage {
    AgentMessage::ToolResult {
        content: vec![ContentBlock::Text { text: text.into() }],
        tool_call_id: tool_call_id.to_string(),
        is_error,
        duration_ms: None,
        diff: None,
        read_source: None,
    }
}

pub(crate) fn tool_result_message(tool_call_id: &str, execution: &ToolExecution) -> AgentMessage {
    AgentMessage::ToolResult {
        content: vec![ContentBlock::Text {
            text: execution.content.clone(),
        }],
        tool_call_id: tool_call_id.to_string(),
        is_error: execution.is_error,
        duration_ms: execution.duration_ms,
        diff: execution.diff.clone(),
        read_source: execution.read_source,
    }
}

/// 拼接模型可见的文本块；空块与换行保持原有语义。
pub(crate) fn content_text(content: &[ContentBlock]) -> String {
    let mut text = String::new();
    for block in content {
        if let ContentBlock::Text { text: part } = block {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(part);
        }
    }
    text
}
