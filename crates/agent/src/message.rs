use crate::session::MessageDisplay;
use crate::tools::ToolExecution;
use model::{ContentBlock, Message};

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

pub fn public_items(
    message: &Message,
    display: Option<&MessageDisplay>,
    entry_id: &str,
    status: protocol::TurnStatus,
    tool_status: protocol::ToolStatus,
    saved_at: &str,
) -> Vec<protocol::HistoryItem> {
    use protocol::HistoryItem;
    let role = match message {
        Message::User { .. } => protocol::HistoryMessageRole::User,
        Message::Assistant { .. } => protocol::HistoryMessageRole::Assistant,
        Message::Developer { .. } | Message::ToolResult { .. } | Message::Notification { .. } => {
            return Vec::new();
        }
    };
    let mut images = message.images().cloned().collect::<Vec<_>>();
    let (mut files, mut annotations) = match display {
        Some(MessageDisplay::User { files, annotations, .. }) => (files.clone(), annotations.clone()),
        _ => (Vec::new(), Vec::new()),
    };
    let (mut text_index, mut thinking_index, mut call_index) = (0, 0, 0);
    message
        .content()
        .iter()
        .enumerate()
        .filter_map(|(index, block)| {
            Some(match block {
                ContentBlock::Text { text, .. } => {
                    let text = user_text_for_display(display, index, text);
                    let empty = match role {
                        protocol::HistoryMessageRole::Assistant => text.trim().is_empty(),
                        protocol::HistoryMessageRole::User => text.is_empty(),
                    };
                    if empty
                        && (text_index != 0
                            || (images.is_empty() && files.is_empty() && annotations.is_empty()))
                    {
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
                        annotations: std::mem::take(&mut annotations),
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

pub fn attach_usage(items: &mut [protocol::HistoryItem], usage: &protocol::SessionModelUsage) {
    if let Some(protocol::HistoryItem::Message {
        stats,
        role: protocol::HistoryMessageRole::Assistant,
        ..
    }) = items.iter_mut().rev().find(|item| matches!(item, protocol::HistoryItem::Message { .. }))
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
        || execution.read_source.is_some()
        || execution.file_created.is_some())
    .then_some(MessageDisplay::Tool {
        output,
        background_task_id,
        duration_ms: execution.duration_ms,
        read_source: execution.read_source,
        file_created: execution.file_created,
    });
    let message = Message::ToolResult {
        content,
        tool_call_id: tool_call_id.to_string(),
        is_error: execution.is_error,
    };
    (message, display)
}

pub fn public_tool_result(
    message: &Message,
    display: Option<&MessageDisplay>,
) -> Option<protocol::ToolResult> {
    let Message::ToolResult { is_error, .. } = message else {
        return None;
    };
    let (output, background_task_id, duration_ms, read_source, file_created) = match display {
        Some(MessageDisplay::Tool {
            output,
            background_task_id,
            duration_ms,
            read_source,
            file_created,
        }) => (
            output.clone().unwrap_or_else(|| message.content_text()),
            background_task_id.clone(),
            *duration_ms,
            *read_source,
            *file_created,
        ),
        _ => (message.content_text(), None, None, None, None),
    };
    Some(protocol::ToolResult {
        output,
        background_task_id,
        images: message.images().cloned().collect(),
        is_error: *is_error,
        duration_ms,
        read_source,
        file_created,
    })
}
