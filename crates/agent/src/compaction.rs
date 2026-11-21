//! 纯内存消息压缩与切块变换：头尾安全截断、超窗降级压缩与原子调用组切分。
//!
//! 本模块不持有锁也不做 IO，供会话微压缩（`session::context`）和超窗轮换降级
//! （`agent::context_rotation`）共用，保证工具调用与结果配对在切块后保持完整。

use std::path::Path;

use model::{ContentBlock, Message};

use crate::session::context::{image_captions, message_token_estimate};

const DEGRADED_TOOL_HEAD_BYTES: usize = 400;
const DEGRADED_TOOL_TAIL_BYTES: usize = 200;
const DEGRADED_ASSISTANT_HEAD_BYTES: usize = 600;
const DEGRADED_ASSISTANT_TAIL_BYTES: usize = 200;

/// 估算一组消息的总 Token 数。
pub(crate) fn messages_tokens(messages: &[Message]) -> u64 {
    messages.iter().map(message_token_estimate).sum()
}

/// 将长文本按 UTF-8 字符边界折叠为头尾片段；若正文包含 `Full output:` 溢写路径行则保留该行。
pub(crate) fn compact_text(text: &str, head_bytes: usize, tail_bytes: usize, note: &str) -> String {
    if text.len() <= head_bytes.saturating_add(tail_bytes).saturating_add(256) {
        return text.to_string();
    }
    let head_end = text.floor_char_boundary(head_bytes.min(text.len()));
    let tail_start = text.ceil_char_boundary(text.len().saturating_sub(tail_bytes));
    let head = &text[..head_end];
    let tail = (tail_bytes > 0 && tail_start > head_end).then(|| &text[tail_start..]);
    let full_output_line = text
        .lines()
        .rev()
        .find(|line| line.starts_with("Full output: "))
        .filter(|line| !head.contains(line) && tail.is_none_or(|t| !t.contains(line)))
        .map(|line| format!("\n{line}"))
        .unwrap_or_default();
    match tail {
        Some(tail) => format!("{head}\n\n[... {note} ...]\n\n{tail}{full_output_line}"),
        None => format!("{head}\n\n[... {note} ...]{full_output_line}"),
    }
}

/// 剥离消息中的图片二进制块并保留文本路径说明（用于超窗轮换降级）。
pub(crate) fn strip_images_to_captions(messages: &mut [Message], directory: &Path) {
    for message in messages {
        let captions = image_captions(message, directory);
        if captions.is_empty() {
            continue;
        }
        let content = message_content_mut(message);
        content.retain(|block| !matches!(block, ContentBlock::Image(_)));
        content.push(ContentBlock::text(format!(
            "{captions}\n[image omitted for context compaction; saved copy path preserved]"
        )));
    }
}

/// 阶梯 1 降级：将历史工具结果正文折叠为紧凑头尾摘要。
pub(crate) fn compact_tool_results(messages: &mut [Message]) {
    for message in messages {
        if let Message::ToolResult { content, .. } = message {
            for block in content {
                if let ContentBlock::Text { text, .. } = block {
                    *text = compact_text(
                        text,
                        DEGRADED_TOOL_HEAD_BYTES,
                        DEGRADED_TOOL_TAIL_BYTES,
                        "tool output compacted for context rotation",
                    );
                }
            }
        }
    }
}

/// 阶梯 2 降级：剥离助手思考，折叠超长助手正文与大参数工具调用，完整保留用户发言。
pub(crate) fn compact_assistant_messages(messages: &mut [Message]) {
    for message in messages {
        if let Message::Assistant { content, provider_metadata } = message {
            *provider_metadata = None;
            content.retain(|block| !matches!(block, ContentBlock::Thinking { .. }));
            for block in content.iter_mut() {
                match block {
                    ContentBlock::Text { text, metadata } => {
                        *metadata = None;
                        *text = compact_text(
                            text,
                            DEGRADED_ASSISTANT_HEAD_BYTES,
                            DEGRADED_ASSISTANT_TAIL_BYTES,
                            "assistant output compacted for context rotation",
                        );
                    }
                    ContentBlock::ToolCall(call) => {
                        call.metadata = None;
                        let serialized = call.arguments.to_string();
                        if serialized.len() > 512 {
                            call.arguments = serde_json::json!({
                                "_compacted": format!("{} bytes omitted for context rotation", serialized.len())
                            });
                        }
                    }
                    ContentBlock::Thinking { .. } | ContentBlock::Image(_) => {}
                }
            }
        }
    }
}

/// 将前置 Developer 接手提示与后续消息、以及 Assistant 工具调用与其对应的 ToolResult 绑定在同一原子组内，
/// 避免切块后产生孤立工具结果或仅含 Developer 消息的空 `messages` 批次。
pub(crate) fn atomic_groups(messages: Vec<Message>) -> Vec<Vec<Message>> {
    let mut groups: Vec<Vec<Message>> = Vec::new();
    let mut iter = messages.into_iter().peekable();
    while let Some(message) = iter.next() {
        let mut group = vec![message];
        while matches!(group.last(), Some(Message::Developer { .. })) && iter.peek().is_some() {
            group.push(iter.next().expect("peeked message following developer exists"));
        }
        let has_calls = group.last().is_some_and(|last| {
            matches!(last, Message::Assistant { .. }) && last.tool_calls().next().is_some()
        });
        if has_calls {
            while matches!(iter.peek(), Some(Message::ToolResult { .. })) {
                group.push(iter.next().expect("peeked tool result exists"));
            }
        }
        if group.iter().all(|m| matches!(m, Message::Developer { .. }))
            && let Some(previous) = groups.last_mut()
        {
            previous.extend(group);
        } else {
            groups.push(group);
        }
    }
    groups
}

/// 按原子组与单批预算将消息列表切分为多个批次；超过预算的单个原子组先逐次折半压缩。
pub(crate) fn partition_into_chunks(messages: Vec<Message>, chunk_budget: u64) -> Vec<Vec<Message>> {
    let groups = atomic_groups(messages);
    let mut chunks: Vec<Vec<Message>> = Vec::new();
    let mut current: Vec<Message> = Vec::new();
    let mut current_tokens = 0u64;
    for mut group in groups {
        let mut group_tokens = messages_tokens(&group);
        while group_tokens > chunk_budget {
            let previous = group_tokens;
            shrink_messages_by_half(&mut group);
            group_tokens = messages_tokens(&group);
            if group_tokens >= previous {
                break;
            }
        }
        if !current.is_empty() && current_tokens.saturating_add(group_tokens) > chunk_budget {
            chunks.push(std::mem::take(&mut current));
            current_tokens = 0;
        }
        current_tokens = current_tokens.saturating_add(group_tokens);
        current.extend(group);
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

/// 将超长文本块按头 2/3、尾 1/3 折半截短，用于极端单条消息仍超出窗口时的兜底压缩。
pub(crate) fn shrink_messages_by_half(messages: &mut [Message]) {
    for message in messages {
        for block in message_content_mut(message) {
            if let ContentBlock::Text { text, .. } = block
                && text.len() > 512
            {
                let target = (text.len() / 2).max(256);
                let head = target * 2 / 3;
                let tail = target - head;
                *text = compact_text(text, head, tail, "truncated to fit context rotation window");
            }
        }
    }
}

fn message_content_mut(message: &mut Message) -> &mut Vec<ContentBlock> {
    match message {
        Message::Developer { content }
        | Message::Notification { content }
        | Message::User { content }
        | Message::Assistant { content, .. }
        | Message::ToolResult { content, .. } => content,
    }
}
