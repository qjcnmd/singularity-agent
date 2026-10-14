//! 会话消息与内容块的数据模型。
//!
//! 内容块覆盖正文、思考链、工具调用与工具结果，一次模型交互的完整语义（推理过程、
//! 多个工具调用）都能原样落盘并在协议重放时还原。

use singularity_model::{ModelMessage, ModelToolCall, ModelTurnResponse, ProviderReasoningReplay};
use std::borrow::Cow;

use crate::tools::ToolExecution;

/// 公开内容的投影范围。工具的生命周期由工具自己的 start/end 事件表达，历史归约又要
/// 工具项才能和结果配对，同一条消息的两类消费方需要的块不一样。
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
    Text {
        text: String,
    },
    /// 图片像素保存在会话所属目录，消息只保留不可变快照描述。
    Image(singularity_protocol::ImageAttachment),
    /// 只放对外公开的思考文本；provider 私有的续接材料由 Assistant 的
    /// provider_reasoning_replay 单独保存，不混进内容块。
    Thinking {
        thinking: String,
    },
    ToolCall(ModelToolCall),
}

/// 核心会话消息结构：以角色为标签的枚举，每个角色只携带自己合法的字段。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "role", rename_all = "camelCase", deny_unknown_fields)]
pub enum AgentMessage {
    #[serde(rename_all = "camelCase")]
    User {
        content: Vec<ContentBlock>,
        /// 技能引用展开前的用户文字；模型消费 content，界面保持原始显示。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        display_text: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    Assistant {
        content: Vec<ContentBlock>,
        /// 模型提供方的私有推理状态，用于 Responses 这类协议重放推理的连续性。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_reasoning_replay: Option<ProviderReasoningReplay>,
    },
    #[serde(rename_all = "camelCase")]
    ToolResult {
        content: Vec<ContentBlock>,
        /// 对应的工具调用 ID；名称和参数由原始 ToolCall 记录提供，这里不重复。
        tool_call_id: String,
        is_error: bool,
        /// 观测到的工具执行耗时；结果未知或没有执行时缺省。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
        /// read 实际读取到的源文件范围；只有 read 的成功结果才带这个字段。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        read_source: Option<singularity_protocol::ReadSource>,
    },
}

impl AgentMessage {
    /// 用户和助手消息的公开内容，历史与完成事件共用；私有续接材料不进入投影，
    /// 工具结果必须由历史归约去绑定对应的调用，不在这里投影。
    pub fn public_items(&self, entry_id: &str, scope: ItemScope) -> Vec<singularity_protocol::HistoryItem> {
        use singularity_protocol::HistoryItem;
        let role = match self {
            Self::User { .. } => "user",
            Self::Assistant { .. } => "assistant",
            Self::ToolResult { .. } => return Vec::new(),
        };
        let include_tool_calls = matches!(scope, ItemScope::History);
        let mut images = self.images().cloned().collect::<Vec<_>>();
        let (mut text_index, mut thinking_index, mut call_index) = (0, 0, 0);
        self.display_content()
            .iter()
            .filter_map(|block| {
                Some(match block {
                    ContentBlock::Text { text }
                        if !text.is_empty() || (text_index == 0 && !images.is_empty()) =>
                    {
                        let id = crate::session::text_item_id(entry_id, text_index);
                        text_index += 1;
                        HistoryItem::Message {
                            id,
                            role: role.into(),
                            text: text.clone(),
                            images: std::mem::take(&mut images),
                        }
                    }
                    ContentBlock::Thinking { thinking } if !thinking.is_empty() => {
                        let id = crate::session::thinking_item_id(entry_id, thinking_index);
                        thinking_index += 1;
                        HistoryItem::Thinking { id, text: thinking.clone() }
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
            Self::User { content, .. }
            | Self::Assistant { content, .. }
            | Self::ToolResult { content, .. } => content,
        }
    }

    /// 用户消息和任务标题共用的展示内容；模型仍消费原始 content。
    pub fn display_content(&self) -> Cow<'_, [ContentBlock]> {
        match self {
            Self::User { content, display_text: Some(text) } => {
                // displayText 只随输入交付保存；该消息的首块就是用户文字。
                let mut displayed = content.clone();
                displayed[0] = ContentBlock::Text { text: text.clone() };
                Cow::Owned(displayed)
            }
            _ => Cow::Borrowed(self.content()),
        }
    }

    pub fn content_text(&self) -> String {
        content_text(self.content())
    }

    /// 消息中的图片快照，历史、预览和模型输入从同一份内容派生。
    pub fn images(&self) -> impl Iterator<Item = &singularity_protocol::ImageAttachment> {
        self.content().iter().filter_map(|block| match block {
            ContentBlock::Image(image) => Some(image),
            _ => None,
        })
    }

    /// 消息里的工具调用载荷。
    pub fn tool_calls(&self) -> impl Iterator<Item = &ModelToolCall> {
        self.content().iter().filter_map(|block| match block {
            ContentBlock::ToolCall(call) => Some(call),
            _ => None,
        })
    }

    /// 工具结果对应的调用 ID；其他角色为 None。
    pub fn tool_call_id(&self) -> Option<&str> {
        match self {
            Self::ToolResult { tool_call_id, .. } => Some(tool_call_id.as_str()),
            _ => None,
        }
    }

    /// provider 的推理重放数据；只有 assistant 消息会带。
    pub fn provider_reasoning_replay(&self) -> Option<&ProviderReasoningReplay> {
        match self {
            Self::Assistant { provider_reasoning_replay, .. } => provider_reasoning_replay.as_ref(),
            _ => None,
        }
    }
}

/// 压缩摘要节点进入模型上下文时加在前面的说明文字。
pub const COMPACTION_SUMMARY_PREFIX: &str = "This checkpoint summarizes earlier conversation history. Treat it as established background and continue directly from the messages that follow without acknowledging the checkpoint.\n\n<compacted-summary>\n";
/// 压缩摘要节点进入模型上下文时加在后面的闭合标记。
pub const COMPACTION_SUMMARY_SUFFIX: &str = "\n</compacted-summary>";

/// 公开可见内容块：空思考和空正文各自跳过，顺序固定为 Thinking → Text。
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
/// 全部 tool_call 块。停止原因和 usage 不属于会话内容。
pub(crate) fn assistant_response_message(response: ModelTurnResponse) -> AgentMessage {
    let ModelTurnResponse { assistant_message, thinking, .. } = response;
    let ModelMessage {
        content: text,
        tool_calls,
        provider_reasoning_replay,
        ..
    } = assistant_message;
    let mut content = public_thinking_text_blocks(thinking, text);
    content.extend(tool_calls.into_iter().map(ContentBlock::ToolCall));
    AgentMessage::Assistant { content, provider_reasoning_replay }
}

pub(crate) fn tool_result_message(tool_call_id: &str, execution: &ToolExecution) -> AgentMessage {
    let mut content = vec![ContentBlock::Text { text: execution.content.clone() }];
    content.extend(execution.images.iter().map(|image| ContentBlock::Image(image.attachment.clone())));
    AgentMessage::ToolResult {
        content,
        tool_call_id: tool_call_id.to_string(),
        is_error: execution.is_error,
        duration_ms: execution.duration_ms,
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
