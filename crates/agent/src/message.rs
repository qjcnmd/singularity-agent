//! 把消息转换成工作台历史条目，以及构造工具结果消息。

use crate::tools::ToolExecution;
use singularity_model::{ContentBlock, Message};

/// 用户和助手消息中可显示的条目，历史读取和完成事件共用。提供方私有字段不转换；
/// 工具结果由历史读取时挂到对应调用上，这里不处理。
pub fn public_items(
    message: &Message,
    entry_id: &str,
    status: singularity_protocol::TurnStatus,
    tool_status: singularity_protocol::ToolStatus,
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
    let (mut text_index, mut thinking_index, mut call_index) = (0, 0, 0);
    message
        .content()
        .iter()
        .enumerate()
        .filter_map(|(index, block)| {
            Some(match block {
                ContentBlock::Text { text, .. } => {
                    let text = message.text_for_display(index, text);
                    if text.is_empty() && (text_index != 0 || images.is_empty()) {
                        return None;
                    }
                    let id = crate::session::text_item_id(entry_id, text_index);
                    text_index += 1;
                    HistoryItem::Message {
                        status,
                        id,
                        role,
                        text: text.to_string(),
                        images: std::mem::take(&mut images),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn published_items_keep_interleaved_tool_calls_in_message_order() {
        let message = Message::Assistant {
            content: vec![
                ContentBlock::text("before"),
                ContentBlock::ToolCall(singularity_model::ModelToolCall {
                    tool_call_id: "call".into(),
                    tool_name: "read".into(),
                    arguments: serde_json::json!({ "path": "README.md" }),
                    metadata: None,
                }),
                ContentBlock::text("after"),
            ],
            provider_metadata: None,
        };
        let items = public_items(
            &message,
            "reply",
            singularity_protocol::TurnStatus::Completed,
            singularity_protocol::ToolStatus::Pending,
        );
        assert!(matches!(
            items[1],
            singularity_protocol::HistoryItem::Tool {
                status: singularity_protocol::ToolStatus::Pending,
                ..
            }
        ));
        assert_eq!(
            items.iter().map(singularity_protocol::HistoryItem::id).collect::<Vec<_>>(),
            ["reply:text:0", "reply:tool:0", "reply:text:1"]
        );
    }
}
