//! 会话消息与内容块数据模型。
//!
//! 支持富文本内容块（纯文本 Text、思考链 Thinking、工具调用 ToolCall）
//! 以及工具执行结果 ToolResult，确保单次模型交互的完整语义（含推理过程与多工具调用）
//! 能够精确持久化与协议重放。
//!
//! AgentMessage 以角色为标签的枚举承载消息体：每个角色只携带其合法字段，
//! 编译器拒绝「user 消息带 toolCallId」一类非法组合；序列化 wire 形状与历史
//! 平铺格式逐字节一致（tag = "role" + 变体级 camelCase，键序由 serde_json
//! Map 排序稳定），session 层的 JSONL 字节夹具固化该契约。

use singularity_model::{
    ModelMessage, ModelStopReason, ModelToolCall, ModelTurnResponse, ProviderReasoningReplay,
};

use crate::tools::ToolExecution;

/// 消息体内的结构化内容块。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ContentBlock {
    /// 纯文本内容块（{"type":"text","text":...}）。
    Text { text: String },
    /// 思考/推理链内容块（{"type":"thinking","thinking":...}）。
    /// 只承载公开思考文本；provider 私有的续接材料由 Assistant 的
    /// provider_reasoning_replay 单独保存，不混入内容块。
    Thinking { thinking: String },
    /// 工具调用描述块（{"type":"tool_call","id":...,"name":...,"args":...}）：
    /// 载荷直接复用模型层的 `ModelToolCall`，不再另存一份同义字段。
    ToolCall(ModelToolCall),
}

/// 核心会话消息数据结构：以角色为标签的枚举，每个角色只携带其合法字段。
///
/// wire 形状与历史平铺格式逐字节一致（role 为内部 tag）：序列化输出
/// {"content":...,"role":"user"} / {"role":"assistant",...,"stopReason":...}
/// / {"role":"toolResult",...,"toolCallId":...,"isError":...}。结果只经调用
/// ID 关联原始 ToolCall，名称不重复携带。deny_unknown_fields 使消息内未知
/// 字段写入即拒绝。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "role", rename_all = "camelCase", deny_unknown_fields)]
pub enum AgentMessage {
    #[serde(rename_all = "camelCase")]
    User { content: Vec<ContentBlock> },
    #[serde(rename_all = "camelCase")]
    Assistant {
        content: Vec<ContentBlock>,
        /// Provider 给出的 assistant 停止原因。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stop_reason: Option<ModelStopReason>,
        /// 模型提供方私有推理状态（用于支持 Responses 等协议的推理连续性重放）。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_reasoning_replay: Option<ProviderReasoningReplay>,
    },
    #[serde(rename_all = "camelCase")]
    ToolResult {
        content: Vec<ContentBlock>,
        /// 对应的工具调用 ID；名称与参数由原始 ToolCall 记录提供。
        tool_call_id: String,
        /// 工具执行是否失败标志。
        is_error: bool,
        /// 观测到的工具执行耗时；结果未知或未执行时缺省。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
        /// 供展示的文件改动；不包含在发送给模型的内容中。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        diff: Option<String>,
    },
}

impl AgentMessage {
    /// 用户和助手消息的公开内容，复用于历史和完成事件；私有续接材料不进入投影。
    /// 工具结果须由历史归约绑定对应调用，不在此投影。
    pub fn public_items(&self, entry_id: &str) -> Vec<singularity_protocol::HistoryItem> {
        use singularity_protocol::HistoryItem;
        let role = match self {
            Self::User { .. } => "user",
            Self::Assistant { .. } => "assistant",
            Self::ToolResult { .. } => return Vec::new(),
        };
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

    /// 消息内容的切片视图。
    pub fn content(&self) -> &[ContentBlock] {
        match self {
            Self::User { content }
            | Self::Assistant { content, .. }
            | Self::ToolResult { content, .. } => content,
        }
    }

    /// 提取并拼接消息内部所有纯文本块的内容视图。
    pub fn content_text(&self) -> String {
        content_text(self.content())
    }

    /// 消息包含的工具调用载荷；类型已收窄，调用方不再解包其他内容块。
    pub fn tool_calls(&self) -> impl Iterator<Item = &ModelToolCall> {
        self.content().iter().filter_map(|block| match block {
            ContentBlock::ToolCall(call) => Some(call),
            _ => None,
        })
    }

    /// 对应工具调用 ID；Option 只表示「该角色不携带调用身份」，ToolResult
    /// 本身必有调用 ID。
    pub fn tool_call_id(&self) -> Option<&str> {
        match self {
            Self::ToolResult { tool_call_id, .. } => Some(tool_call_id.as_str()),
            _ => None,
        }
    }

    /// provider 推理重放；仅 assistant 消息携带。
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

/// 压缩摘要节点进入模型上下文时的说明前缀。
pub const COMPACTION_SUMMARY_PREFIX: &str = "This checkpoint summarizes earlier conversation history. Treat it as established background and continue directly from the messages that follow without acknowledging the checkpoint.\n\n<compacted-summary>\n";
/// 压缩摘要节点进入模型上下文时的闭合后缀。
pub const COMPACTION_SUMMARY_SUFFIX: &str = "\n</compacted-summary>";

pub(crate) fn user_message(text: &str) -> AgentMessage {
    AgentMessage::User {
        content: vec![ContentBlock::Text {
            text: text.to_string(),
        }],
    }
}

/// 公开可见内容块的唯一构造规则：空思考与空正文各自跳过，顺序固定为
/// Thinking → Text。
///
/// 正常响应与失败时的可见部分共用这条规则；tool_calls、stop_reason 与私有
/// 续接材料属于两条路径的真实差异，仍由各自决定。按值接收：正常路径直接
/// 移动模型响应里已有的字符串，失败路径只在确需持久化时才构造拥有值。
/// 只在 crate 内复用，不进入公开消息 API。
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

/// 一次模型响应投影为一条 assistant 消息：公开 Thinking → 公开 Text →
/// 全部 tool_call 块。
///
/// 响应按值交接：正文、思考、调用与私有续接材料都从拥有的响应移动进内容块，
/// 不再为交接复制。provider 的 stop_reason 随 Assistant 一并保存；usage 由
/// 请求观测与 operation 终态统计链记录，不属于会话内容。
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

pub(crate) fn tool_result_message(tool_call_id: &str, execution: &ToolExecution) -> AgentMessage {
    AgentMessage::ToolResult {
        content: vec![ContentBlock::Text {
            text: execution.content.clone(),
        }],
        // 名称与参数由原始 ToolCall 拥有；结果只经调用 id 关联。
        tool_call_id: tool_call_id.to_string(),
        is_error: execution.is_error,
        duration_ms: execution.duration_ms,
        diff: execution.diff.clone(),
    }
}

/// 拼接模型可见的文本块，保持空块与换行的既有语义。
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 公开内容规则只有一处：空思考/空正文各自跳过，顺序固定 Thinking → Text。
    #[test]
    fn public_blocks_drop_empties_and_keep_thinking_before_text() {
        assert!(public_thinking_text_blocks(String::new(), String::new()).is_empty());
        assert!(matches!(
            public_thinking_text_blocks("why".into(), String::new()).as_slice(),
            [ContentBlock::Thinking { thinking }] if thinking == "why"
        ));
        assert!(matches!(
            public_thinking_text_blocks(String::new(), "answer".into()).as_slice(),
            [ContentBlock::Text { text }] if text == "answer"
        ));
        assert!(matches!(
            public_thinking_text_blocks("why".into(), "answer".into()).as_slice(),
            [ContentBlock::Thinking { thinking }, ContentBlock::Text { text }]
                if thinking == "why" && text == "answer"
        ));
    }

    #[test]
    fn completed_reply_keeps_displayed_thinking_without_replay() {
        let mut response = ModelTurnResponse::completed("answer");
        response.thinking = "visible thinking".into();
        let message = assistant_response_message(response);
        assert!(
            matches!(&message.content()[0], ContentBlock::Thinking { thinking } if thinking == "visible thinking")
        );
        assert!(matches!(&message.content()[1], ContentBlock::Text { text } if text == "answer"));
        assert!(message.provider_reasoning_replay().is_none());
    }
}
