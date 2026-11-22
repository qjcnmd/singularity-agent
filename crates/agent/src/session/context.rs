//! 从会话日志投影模型上下文并估算 Token（日志本身只追加不修改）。

use std::fmt::Write as _;

use app_core::estimate_tokens_of;
use model::{ContentBlock, Message, ModelConfigurationSnapshot};

use super::format::{LedgerRecord, SessionEntry};
use super::manager::SessionData;

const SUMMARY_OPEN_TAG: &str = "<compacted-summary>";
const SUMMARY_CLOSE_TAG: &str = "</compacted-summary>";

const CHECKPOINT_PREAMBLE: &str = "This is an automatically generated checkpoint condensing an earlier span of the conversation to free up context. Treat the captured context as established background and build on it without restating it. Continue the task directly from the messages that follow, without acknowledging this checkpoint.";

pub(crate) fn frame_compacted_summary(summary: &str) -> Message {
    Message::user(format!("{CHECKPOINT_PREAMBLE}\n\n{SUMMARY_OPEN_TAG}\n{summary}\n{SUMMARY_CLOSE_TAG}"))
}

const UNKNOWN_TOOL_OUTCOME: &str = "[previous execution was interrupted; outcome unknown. Inspect the current state before deciding whether to repeat an action with side effects.]";
const MESSAGE_OVERHEAD_TOKENS: u64 = 4;
const CONTENT_BLOCK_OVERHEAD_TOKENS: u64 = 4;
const TOOL_RESULT_OVERHEAD_TOKENS: u64 = 4;

/// 把日志记录转换成模型消息；未配对工具调用补一条“结果未知”。
pub(crate) fn project_messages(entries: &[&SessionEntry]) -> Vec<Message> {
    let mut messages = Vec::new();
    let mut remaining = entries.iter().copied().peekable();
    while let Some(entry) = remaining.next() {
        messages.push(model_message(entry));
        let SessionEntry::Message { message, .. } = entry else {
            continue;
        };
        let mut calls = message.tool_calls().peekable();
        if calls.peek().is_none() {
            continue;
        }
        let mut results = std::collections::HashMap::new();
        while let Some(SessionEntry::Message {
            message: Message::ToolResult { tool_call_id, .. },
            ..
        }) = remaining.peek()
        {
            let entry = remaining.next().expect("peeked result exists");
            results.insert(tool_call_id.as_str(), entry);
        }
        for call in calls {
            messages.push(if let Some(entry) = results.remove(call.tool_call_id.as_str()) {
                model_message(entry)
            } else {
                Message::ToolResult {
                    content: vec![ContentBlock::text(UNKNOWN_TOOL_OUTCOME)],
                    tool_call_id: call.tool_call_id.clone(),
                    is_error: true,
                }
            });
        }
    }
    messages
}

fn context_token_estimate(entries: &[&SessionEntry], session: &SessionData) -> u64 {
    let mut tokens = 0u64;
    let mut calls = 0usize;
    let mut results = 0usize;
    for entry in entries {
        tokens = tokens.saturating_add(entry_token_estimate(entry, session));
        if let SessionEntry::Message { message, .. } = entry {
            calls = calls.saturating_add(message.tool_calls().count());
            if message.tool_call_id().is_some() {
                results = results.saturating_add(1);
            }
        }
    }
    let unknown_result_tokens = MESSAGE_OVERHEAD_TOKENS
        .saturating_add(text_block_token_estimate(UNKNOWN_TOOL_OUTCOME))
        .saturating_add(TOOL_RESULT_OVERHEAD_TOKENS);
    let missing_results = u64::try_from(calls.saturating_sub(results)).unwrap_or(u64::MAX);
    tokens.saturating_add(missing_results.saturating_mul(unknown_result_tokens))
}

enum ContextEntry<'a> {
    Message(&'a Message),
    Summary(&'a str),
}

fn context_entry(entry: &SessionEntry) -> Option<ContextEntry<'_>> {
    match entry {
        SessionEntry::Message { observation: Some(observation), .. }
            if observation.status != protocol::ProviderAttemptStatus::Ok =>
        {
            None
        }
        SessionEntry::Message { message, .. } => Some(ContextEntry::Message(message)),
        SessionEntry::Compaction { compaction, .. } => Some(ContextEntry::Summary(&compaction.summary)),
        SessionEntry::Metadata { .. } | SessionEntry::Record { .. } => None,
    }
}

fn entry_token_estimate(entry: &SessionEntry, session: &SessionData) -> u64 {
    match context_entry(entry) {
        Some(ContextEntry::Message(message)) => {
            let tokens = message_token_estimate(message);
            let captions = image_captions(message, &session.image_directory());
            if captions.is_empty() {
                tokens
            } else {
                tokens.saturating_add(text_block_token_estimate(&captions))
            }
        }
        Some(ContextEntry::Summary(summary)) => message_token_estimate(&frame_compacted_summary(summary)),
        None => 0,
    }
}

pub(crate) fn message_token_estimate(message: &Message) -> u64 {
    let mut tokens = MESSAGE_OVERHEAD_TOKENS;
    for block in message.content() {
        tokens = tokens.saturating_add(match block {
            ContentBlock::Text { text, .. } | ContentBlock::Thinking { thinking: text, .. } => {
                text_block_token_estimate(text)
            }
            ContentBlock::Image(image) => u64::from(image.width)
                .div_ceil(32)
                .saturating_mul(u64::from(image.height).div_ceil(32))
                .saturating_add(128),
            ContentBlock::ToolCall(call) => estimate_tokens_of(&call.tool_name)
                .saturating_add(estimate_tokens_of(&call.arguments.to_string()))
                .saturating_add(CONTENT_BLOCK_OVERHEAD_TOKENS),
        });
    }
    if matches!(message, Message::ToolResult { .. }) {
        tokens = tokens.saturating_add(TOOL_RESULT_OVERHEAD_TOKENS);
    }
    tokens
}

fn text_block_token_estimate(text: &str) -> u64 {
    estimate_tokens_of(text).saturating_add(CONTENT_BLOCK_OVERHEAD_TOKENS)
}

/// 本会话最近一次成功完成普通生成的模型选择（直接读取该回复记录的实际推理档位，不受运行中设置修改干扰）。
pub(crate) fn last_routed_model(session: &SessionData) -> Option<protocol::ModelSelection> {
    session.entries().iter().rev().find_map(|entry| match entry {
        SessionEntry::Message {
            message: Message::Assistant { provider_metadata, .. },
            observation: Some(observation),
            ..
        } if observation.status == protocol::ProviderAttemptStatus::Ok
            && observation.purpose == protocol::RequestPurpose::Generation =>
        {
            Some(protocol::ModelSelection {
                provider: observation.provider.clone(),
                model: observation.model.clone(),
                reasoning: provider_metadata.as_ref().and_then(|meta| meta.reasoning.clone()),
            })
        }
        _ => None,
    })
}

fn latest_reply_usage(
    session: &SessionData,
    entries: &[&SessionEntry],
) -> Option<(String, String, u64, u64)> {
    let latest = session
        .entries()
        .iter()
        .rev()
        .take_while(|entry| {
            !matches!(
                entry,
                SessionEntry::Compaction { .. }
                    | SessionEntry::Record {
                        record: LedgerRecord::OperationStarted,
                        ..
                    }
            )
        })
        .find(|entry| matches!(context_entry(entry), Some(ContextEntry::Message(Message::Assistant { .. }))));
    if let Some(SessionEntry::Message { id, observation: Some(observation), .. }) = latest
        && let Some(usage) = &observation.usage
    {
        let after = entries
            .iter()
            .position(|entry| entry.id() == id)
            .expect("latest assistant belongs to the active context")
            + 1;
        let tail_tokens = context_token_estimate(&entries[after..], session);
        return Some((
            observation.provider.clone(),
            observation.model.clone(),
            usage.total_tokens,
            tail_tokens,
        ));
    }
    None
}

fn input_tokens(
    session: &SessionData,
    entries: &[&SessionEntry],
    identity: (&str, &str),
    definition_tokens: u64,
) -> u64 {
    if let Some((provider, model, total_tokens, tail_tokens)) = latest_reply_usage(session, entries)
        && provider == identity.0
        && model == identity.1
    {
        return total_tokens.saturating_add(tail_tokens);
    }
    context_token_estimate(entries, session).saturating_add(definition_tokens)
}

pub(crate) fn estimated_input_tokens(
    session: &SessionData,
    identity: (&str, &str),
    definition_tokens: u64,
) -> u64 {
    let entries = resolve_context_entries(session);
    input_tokens(session, &entries, identity, definition_tokens)
}

pub(crate) fn prepare_context(
    session: &SessionData,
    identity: (&str, &str),
    definition_tokens: u64,
) -> (Vec<Message>, u64) {
    let entries = resolve_context_entries(session);
    (project_messages(&entries), input_tokens(session, &entries, identity, definition_tokens))
}

/// 扣除输出预留后的有效消息预算的 15%（至少 1 token）。
pub fn retention_budget_tokens(model: ModelConfigurationSnapshot) -> u64 {
    let message_budget = model.context_window().saturating_sub(u64::from(model.max_output_tokens));
    u64::try_from(u128::from(message_budget) * 15 / 100).unwrap_or(0).max(1)
}

/// 按工具调用/结果配对划分原子组，从末尾倒序保留至少 `retain_tokens`（即使为 0 也至少保留最后一个原子组）。
pub(crate) fn select_compactable_split(
    active: &[&SessionEntry],
    session: &SessionData,
    retain_tokens: u64,
) -> Option<(usize, String)> {
    let msg_start = usize::from(matches!(active.first(), Some(SessionEntry::Compaction { .. })));
    if msg_start >= active.len() {
        return None;
    }

    let mut units: Vec<std::ops::Range<usize>> = Vec::new();
    let mut i = msg_start;
    while i < active.len() {
        let start = i;
        let is_tool_result =
            matches!(context_entry(active[i]), Some(ContextEntry::Message(Message::ToolResult { .. })));
        if is_tool_result && let Some(prev) = units.last_mut() {
            prev.end = i + 1;
            i += 1;
            continue;
        }
        let has_calls = match active[i] {
            SessionEntry::Message { message, .. } => {
                matches!(message, Message::Assistant { .. }) && message.tool_calls().next().is_some()
            }
            _ => false,
        };
        i += 1;
        if has_calls {
            while i < active.len()
                && matches!(context_entry(active[i]), Some(ContextEntry::Message(Message::ToolResult { .. })))
            {
                i += 1;
            }
        }
        units.push(start..i);
    }

    let mut accumulated = 0u64;
    let mut keep_unit = units.len();
    for (idx, range) in units.iter().enumerate().rev() {
        let unit_tokens = context_token_estimate(&active[range.clone()], session);
        accumulated = accumulated.saturating_add(unit_tokens);
        keep_unit = idx;
        if accumulated >= retain_tokens {
            break;
        }
    }
    if keep_unit == 0 {
        return None;
    }
    let split_idx = units[keep_unit].start;
    Some((split_idx, active[split_idx].id().to_owned()))
}

#[derive(Debug)]
pub struct CompactionInput {
    pub(crate) messages: Vec<Message>,
    pub(crate) first_kept_entry_id: String,
    pub(crate) prefix_estimated_tokens: u64,
    pub(crate) total_estimated_tokens: u64,
    pub(crate) latest_usage: Option<(String, String, u64, u64)>,
    pub(crate) image_directory: std::path::PathBuf,
}

impl CompactionInput {
    /// 前缀为空或不可能小于空检查点框定时返回 `None`。
    pub async fn select(session: &super::SessionWriter, retain_tokens: u64) -> Option<Self> {
        let session = std::sync::Arc::clone(session);
        tokio::task::spawn_blocking(move || {
            super::with_writer(&session, |session| {
                let data = session.data();
                let active = resolve_context_entries(data);
                let (split_idx, first_kept_entry_id) =
                    select_compactable_split(&active, data, retain_tokens)?;
                let prefix_entries = &active[..split_idx];
                let prefix_estimated_tokens = context_token_estimate(prefix_entries, data);
                if prefix_estimated_tokens <= message_token_estimate(&frame_compacted_summary("")) {
                    return None;
                }
                let kept_estimated_tokens = context_token_estimate(&active[split_idx..], data);
                Some(Self {
                    messages: project_messages(prefix_entries),
                    first_kept_entry_id,
                    prefix_estimated_tokens,
                    total_estimated_tokens: prefix_estimated_tokens.saturating_add(kept_estimated_tokens),
                    latest_usage: latest_reply_usage(data, &active),
                    image_directory: data.image_directory(),
                })
            })
        })
        .await
        .expect("compaction selection completes while the runtime is running")
    }
}

fn model_message(entry: &SessionEntry) -> Message {
    match context_entry(entry).expect("context contains messages and compaction summaries") {
        ContextEntry::Message(message) => message.clone(),
        ContextEntry::Summary(summary) => frame_compacted_summary(summary),
    }
}

/// 请求附加的一个文本块；生成与预算都按整块处理，避免多张图片分别向上取整。
pub(crate) fn image_captions(message: &Message, directory: &std::path::Path) -> String {
    let mut captions = String::new();
    for (index, image) in message.images().enumerate() {
        let _ = write!(
            captions,
            "\n[Image #{}: {}; {} × {}; saved copy: {}]",
            index + 1,
            image.name,
            image.width,
            image.height,
            directory.join(&image.id).display()
        );
    }
    captions
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

/// 在写者锁外读取消息引用的图片，并在消息末尾附上图片的名称、尺寸和保存位置。
pub(crate) fn load_images(
    messages: &mut [Message],
    directory: &std::path::Path,
) -> std::io::Result<std::collections::HashMap<String, String>> {
    let mut images = std::collections::HashMap::new();
    for message in messages {
        for image in message.images() {
            images.insert(image.id.clone(), crate::image::load_image(directory, image)?);
        }
        let captions = image_captions(message, directory);
        if !captions.is_empty() {
            message_content_mut(message).push(ContentBlock::text(captions));
        }
    }
    Ok(images)
}

/// 最新压缩检查点放最前，随后保留 `firstKeptEntryId` 起及之后的原始消息（跳过夹在中间的旧检查点）。
pub(crate) fn resolve_context_entries(session: &SessionData) -> Vec<&SessionEntry> {
    let entries = session.entries();
    let latest_compact = entries.iter().rposition(|entry| matches!(entry, SessionEntry::Compaction { .. }));
    match latest_compact {
        None => entries.iter().filter(|entry| context_entry(entry).is_some()).collect(),
        Some(compact_idx) => {
            let mut active = vec![&entries[compact_idx]];
            if let SessionEntry::Compaction { compaction, .. } = &entries[compact_idx]
                && let Some(first_kept_id) = &compaction.first_kept_entry_id
                && let Some(kept_idx) = entries[..compact_idx].iter().rposition(|e| e.id() == first_kept_id)
            {
                active.extend(entries[kept_idx..compact_idx].iter().filter(|entry| {
                    matches!(entry, SessionEntry::Message { .. }) && context_entry(entry).is_some()
                }));
            }
            active.extend(entries[compact_idx + 1..].iter().filter(|entry| {
                matches!(entry, SessionEntry::Message { .. }) && context_entry(entry).is_some()
            }));
            active
        }
    }
}
