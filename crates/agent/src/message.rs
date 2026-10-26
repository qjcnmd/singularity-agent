//! 把消息转换成工作台历史条目，以及构造工具结果消息。

use crate::tools::ToolExecution;
use singularity_model::{ContentBlock, Message};

/// 用户和助手消息中可显示的条目，历史读取和完成事件共用。提供方私有字段不转换；
/// 工具结果由历史读取时挂到对应调用上，这里不处理。
/// `saved_at` 是消息写入日志的时间。助手回复的累计用量由调用方挂到最后一段正文上。
pub fn public_items(
    message: &Message,
    entry_id: &str,
    status: singularity_protocol::TurnStatus,
    tool_status: singularity_protocol::ToolStatus,
    saved_at: &str,
) -> Vec<singularity_protocol::HistoryItem> {
    use singularity_protocol::HistoryItem;
    let role = match message {
        Message::User { .. } => singularity_protocol::HistoryMessageRole::User,
        Message::Assistant { .. } => singularity_protocol::HistoryMessageRole::Assistant,
        Message::Developer { .. } | Message::ToolResult { .. } | Message::Notification { .. } => {
            return Vec::new();
        }
    };
    let mut images = message.images().cloned().collect::<Vec<_>>();
    let mut files = message.files().to_vec();
    let (mut text_index, mut thinking_index, mut call_index) = (0, 0, 0);
    message
        .content()
        .iter()
        .enumerate()
        .filter_map(|(index, block)| {
            Some(match block {
                ContentBlock::Text { text, .. } => {
                    let text = message.text_for_display(index, text);
                    if text.is_empty() && (text_index != 0 || (images.is_empty() && files.is_empty())) {
                        return None;
                    }
                    let id = crate::session::text_item_id(entry_id, text_index);
                    text_index += 1;
                    HistoryItem::Message {
                        status,
                        id,
                        role,
                        text: text.to_string(),
                        at: Some(saved_at.to_owned()),
                        stats: None,
                        images: std::mem::take(&mut images),
                        files: std::mem::take(&mut files),
                    }
                }
                ContentBlock::Thinking { thinking, .. } if !thinking.is_empty() => {
                    let id = crate::session::thinking_item_id(entry_id, thinking_index);
                    thinking_index += 1;
                    HistoryItem::Thinking { id, text: thinking.clone(), status }
                }
                ContentBlock::ToolCall(call) => {
                    let id = crate::session::tool_item_id(entry_id, call_index);
                    call_index += 1;
                    HistoryItem::Tool {
                        status: tool_status,
                        id,
                        name: call.tool_name.clone(),
                        args: call.arguments.clone(),
                        result: None,
                    }
                }
                _ => return None,
            })
        })
        .collect()
}

/// 把会话累计用量挂到一条助手消息的最后一段正文上；没有正文时不显示。
pub fn attach_usage(
    items: &mut [singularity_protocol::HistoryItem],
    usage: &singularity_protocol::SessionModelUsage,
) {
    if let Some(singularity_protocol::HistoryItem::Message {
        stats,
        role: singularity_protocol::HistoryMessageRole::Assistant,
        ..
    }) = items
        .iter_mut()
        .rev()
        .find(|item| matches!(item, singularity_protocol::HistoryItem::Message { .. }))
    {
        *stats = Some(usage.clone());
    }
}

/// 摘要放进模型上下文时加在前面的说明。
pub const COMPACTION_SUMMARY_PREFIX: &str = "This checkpoint summarizes earlier conversation history. Treat it as established background and continue directly from the messages that follow without acknowledging the checkpoint.\n\n<compacted-summary>\n";
/// 摘要放进模型上下文时加在后面的结束标签。
pub const COMPACTION_SUMMARY_SUFFIX: &str = "\n</compacted-summary>";

pub(crate) fn tool_result_message(tool_call_id: &str, execution: &ToolExecution) -> Message {
    let mut content = vec![ContentBlock::text(execution.content.clone())];
    content.extend(execution.images.iter().map(|image| ContentBlock::Image(image.attachment.clone())));
    Message::ToolResult {
        content,
        tool_call_id: tool_call_id.to_string(),
        is_error: execution.is_error,
        duration_ms: execution.duration_ms,
        read_source: execution.read_source,
    }
}

/// 实时完成和历史读取共用工具结果的显示内容。
pub fn public_tool_result(message: &Message) -> Option<singularity_protocol::ToolResult> {
    let Message::ToolResult { is_error, duration_ms, read_source, .. } = message else {
        return None;
    };
    Some(singularity_protocol::ToolResult {
        output: message.content_text(),
        images: message.images().cloned().collect(),
        is_error: *is_error,
        duration_ms: *duration_ms,
        read_source: *read_source,
    })
}
