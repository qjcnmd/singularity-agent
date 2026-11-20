//! 把消息转换成工作台历史条目，以及构造工具结果消息和独立展示元数据。

use crate::session::MessageDisplay;
use crate::tools::ToolExecution;
use singularity_model::{ContentBlock, Message};

/// 取出用户消息首段文本的界面展示文案：有原始输入时显示原文，模型消费含技能正文与附件路径的 `content`。
pub fn user_text_for_display<'a>(
    display: Option<&'a MessageDisplay>,
    index: usize,
    text: &'a str,
) -> &'a str {
    match display {
        Some(MessageDisplay::User { display_text: Some(display), .. }) if index == 0 => display,
        _ => text,
    }
}

fn user_files(display: Option<&MessageDisplay>) -> Vec<String> {
    match display {
        Some(MessageDisplay::User { files, .. }) => files.clone(),
        _ => Vec::new(),
    }
}

/// 用户和助手消息中可显示的条目，历史读取和完成事件共用。提供方私有字段不转换；
/// 工具结果由历史读取时挂到对应调用上，这里不处理。
/// `saved_at` 是消息写入日志的时间。助手回复的累计用量由调用方挂到最后一段正文上。
pub fn public_items(
    message: &Message,
    display: Option<&MessageDisplay>,
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
    let mut files = user_files(display);
    let (mut text_index, mut thinking_index, mut call_index) = (0, 0, 0);
    message
        .content()
        .iter()
        .enumerate()
        .filter_map(|(index, block)| {
            Some(match block {
                ContentBlock::Text { text, .. } => {
                    let text = user_text_for_display(display, index, text);
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

pub(crate) fn tool_result_message(
    tool_call_id: &str,
    execution: &ToolExecution,
) -> (Message, Option<MessageDisplay>) {
    let mut content = vec![ContentBlock::text(execution.content.clone())];
    content.extend(execution.images.iter().map(|image| ContentBlock::Image(image.attachment.clone())));
    let output = execution.display.as_ref().map(|display| display.output.clone());
    let background_task_id =
        execution.display.as_ref().and_then(|display| display.background_task_id.clone());
    let display = (output.is_some()
        || background_task_id.is_some()
        || execution.duration_ms.is_some()
        || execution.read_source.is_some())
    .then_some(MessageDisplay::Tool {
        output,
        background_task_id,
        duration_ms: execution.duration_ms,
        read_source: execution.read_source,
    });
    let message = Message::ToolResult {
        content,
        tool_call_id: tool_call_id.to_string(),
        is_error: execution.is_error,
    };
    (message, display)
}

/// 实时完成和历史读取共用工具结果的显示内容。
pub fn public_tool_result(
    message: &Message,
    display: Option<&MessageDisplay>,
) -> Option<singularity_protocol::ToolResult> {
    let Message::ToolResult { is_error, .. } = message else {
        return None;
    };
    let (output, background_task_id, duration_ms, read_source) = match display {
        Some(MessageDisplay::Tool {
            output,
            background_task_id,
            duration_ms,
            read_source,
        }) => (
            output.clone().unwrap_or_else(|| message.content_text()),
            background_task_id.clone(),
            *duration_ms,
            *read_source,
        ),
        _ => (message.content_text(), None, None, None),
    };
    Some(singularity_protocol::ToolResult {
        output,
        background_task_id,
        images: message.images().cloned().collect(),
        is_error: *is_error,
        duration_ms,
        read_source,
    })
}
