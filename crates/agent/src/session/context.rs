//! 从会话日志生成发给模型的对话上下文，并估算 Token。
//!
//! 对话来自日志；指令和工具定义由 Agent 提供。普通请求、摘要请求、压缩判断和
//! 超限后的重发都用这里生成的上下文。日志本身不被修改。

use singularity_core::estimate_tokens_of;
use singularity_model::{ContentBlock, Message};

use crate::message::{COMPACTION_SUMMARY_PREFIX, COMPACTION_SUMMARY_SUFFIX};

use super::format::{LedgerRecord, SessionEntry};
use super::manager::SessionData;

fn compaction_summary(summary: &str) -> String {
    format!("{COMPACTION_SUMMARY_PREFIX}{summary}{COMPACTION_SUMMARY_SUFFIX}")
}

const UNKNOWN_TOOL_OUTCOME: &str = "[previous execution was interrupted; outcome unknown. Inspect the current state before deciding whether to repeat an action with side effects.]";
const MESSAGE_OVERHEAD_TOKENS: u64 = 4;
const CONTENT_BLOCK_OVERHEAD_TOKENS: u64 = 4;
const TOOL_RESULT_OVERHEAD_TOKENS: u64 = 4;

/// 把日志记录转换成模型消息；工具调用没有结果（如被中断）时补一条“结果未知”，
/// 让每个调用都有对应结果。
fn project_messages(entries: &[&SessionEntry]) -> Vec<Message> {
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
            results.insert(tool_call_id.as_str(), remaining.next().expect("peeked result exists"));
        }
        for call in calls {
            messages.push(if let Some(entry) = results.remove(call.tool_call_id.as_str()) {
                model_message(entry)
            } else {
                Message::ToolResult {
                    content: vec![ContentBlock::text(UNKNOWN_TOOL_OUTCOME)],
                    tool_call_id: call.tool_call_id.clone(),
                    display_output: None,
                    background_task_id: None,
                    is_error: true,
                    duration_ms: None,
                    read_source: None,
                }
            });
        }
    }
    messages
}

fn context_token_estimate(entries: &[&SessionEntry], image_directory: &std::path::Path) -> u64 {
    let mut tokens = 0;
    let mut calls = 0;
    let mut results = 0;
    for entry in entries {
        tokens += entry_token_estimate(entry, image_directory);
        if let SessionEntry::Message { message, .. } = entry {
            calls += message.tool_calls().count();
            if message.tool_call_id().is_some() {
                results += 1;
            }
        }
    }
    let unknown_result_tokens = MESSAGE_OVERHEAD_TOKENS
        + text_block_token_estimate(UNKNOWN_TOOL_OUTCOME)
        + TOOL_RESULT_OVERHEAD_TOKENS;
    tokens + calls.saturating_sub(results) as u64 * unknown_result_tokens
}

enum ContextEntry<'a> {
    Message(&'a Message),
    Summary(&'a str),
}

/// 判断一条记录是否进入模型上下文；Token 估算、压缩切点和生成请求共用这一判断。
fn context_entry(entry: &SessionEntry) -> Option<ContextEntry<'_>> {
    match entry {
        SessionEntry::Message { observation: Some(observation), .. }
            if observation.status != singularity_protocol::ProviderAttemptStatus::Ok =>
        {
            None
        }
        SessionEntry::Message { message, .. } => Some(ContextEntry::Message(message)),
        SessionEntry::Compaction { compaction, .. } => Some(ContextEntry::Summary(&compaction.summary)),
        SessionEntry::Metadata { .. } | SessionEntry::Record { .. } => None,
    }
}

/// 估算一条记录的 Token；指令和工具定义由 Agent 另外估算。
fn entry_token_estimate(entry: &SessionEntry, image_directory: &std::path::Path) -> u64 {
    match context_entry(entry) {
        Some(ContextEntry::Message(message)) => {
            let tokens = message_token_estimate(message);
            let captions = image_captions(message, image_directory);
            if captions.is_empty() {
                tokens
            } else {
                tokens.saturating_add(text_block_token_estimate(&captions))
            }
        }
        Some(ContextEntry::Summary(summary)) => {
            MESSAGE_OVERHEAD_TOKENS + text_block_token_estimate(&compaction_summary(summary))
        }
        None => 0,
    }
}

pub(crate) fn message_token_estimate(message: &Message) -> u64 {
    content_token_estimate(message.content(), matches!(message, Message::ToolResult { .. }))
}

fn text_block_token_estimate(text: &str) -> u64 {
    estimate_tokens_of(text) + CONTENT_BLOCK_OVERHEAD_TOKENS
}

/// 思考文本也计入预算；不回传思考的模型可能因此稍早压缩。
fn content_token_estimate(content: &[ContentBlock], tool_result: bool) -> u64 {
    let mut tokens = MESSAGE_OVERHEAD_TOKENS;
    for block in content {
        tokens = tokens.saturating_add(match block {
            ContentBlock::Text { text, .. } | ContentBlock::Thinking { thinking: text, .. } => {
                text_block_token_estimate(text)
            }
            // 没有提供方实际用量时按尺寸估算图片 Token；拿到回复后优先使用提供方上报的用量。
            ContentBlock::Image(image) => {
                u64::from(image.width).div_ceil(32) * u64::from(image.height).div_ceil(32) + 128
            }
            ContentBlock::ToolCall(call) => {
                estimate_tokens_of(&call.tool_name)
                    + estimate_tokens_of(&call.arguments.to_string())
                    + CONTENT_BLOCK_OVERHEAD_TOKENS
            }
        });
    }
    if tool_result {
        tokens += TOOL_RESULT_OVERHEAD_TOKENS;
    }
    tokens
}

/// 输入 Token：本轮内最近一次回复的实际用量已包含指令、工具定义和回复本身，只补算
/// 之后新增的消息。跨轮、压缩后或换了模型时没有可用的实际用量，全部重新估算。
fn input_tokens(
    session: &SessionData,
    entries: &[&SessionEntry],
    identity: (&str, &str),
    definition_tokens: u64,
) -> u64 {
    let image_directory = session.image_directory();
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
        && observation.provider == identity.0
        && observation.model == identity.1
        && let Some(usage) = &observation.usage
    {
        let after = entries
            .iter()
            .position(|entry| entry.id() == id)
            .expect("latest assistant belongs to the active context")
            + 1;
        return usage
            .total_tokens
            .saturating_add(context_token_estimate(&entries[after..], &image_directory));
    }
    context_token_estimate(entries, &image_directory).saturating_add(definition_tokens)
}

/// 生成请求用的消息和输入 Token 估算；图片由调用方在锁外读取。
pub(crate) fn prepare_context(
    session: &SessionData,
    identity: (&str, &str),
    definition_tokens: u64,
) -> (Vec<Message>, u64) {
    let entries = resolve_context_entries(session);
    (project_messages(&entries), input_tokens(session, &entries, identity, definition_tokens))
}

/// 只估算输入 Token，压缩判断和输出预算共用。
pub(crate) fn estimated_tokens(session: &SessionData, identity: (&str, &str), definition_tokens: u64) -> u64 {
    input_tokens(session, &resolve_context_entries(session), identity, definition_tokens)
}

/// 要总结的早期对话；切点不会拆开一组工具调用和结果。
pub struct CompactionPrefix {
    pub(crate) messages: Vec<Message>,
    pub(crate) image_directory: std::path::PathBuf,
    /// 这段对话的 Token 估算，包括开头的上一份摘要。
    pub(crate) estimated_tokens: u64,
    pub(crate) updating: bool,
    pub(crate) first_kept_entry_id: String,
}

impl CompactionPrefix {
    /// 先选出需要总结的历史；没有更早内容时无需准备模型、指令或工具。
    pub async fn select(session: &super::SessionWriter) -> Option<Self> {
        let session = std::sync::Arc::clone(session);
        tokio::task::spawn_blocking(move || compaction_prefix(&super::lock_writer(&session)))
            .await
            .expect("summary selection completes while the runtime is running")
    }
}

/// 摘要保留的近期对话比例；自动压缩和独立压缩共用。
const KEEP_RECENT_RATIO: f64 = 0.1;

pub(crate) fn compaction_prefix(session: &SessionData) -> Option<CompactionPrefix> {
    let entries = resolve_context_entries(session);
    let image_directory = session.image_directory();
    let keep_recent_tokens =
        (context_token_estimate(&entries, &image_directory) as f64 * KEEP_RECENT_RATIO).ceil() as u64;
    let cut = find_cut_point(&entries, keep_recent_tokens, &image_directory);
    if cut == 0 {
        return None;
    }
    // 已有摘要留在开头，与普通请求一致；如果切点前只有上一份摘要，说明没有新对话可总结。
    let updating = matches!(entries[0], SessionEntry::Compaction { .. });
    if updating && cut == 1 {
        return None;
    }
    let selected = &entries[..cut];
    Some(CompactionPrefix {
        messages: project_messages(selected),
        estimated_tokens: context_token_estimate(selected, &image_directory),
        image_directory,
        updating,
        first_kept_entry_id: entries[cut].id().to_string(),
    })
}

/// 从后往前累加到保留预算；若当前记录是工具结果，继续退到整组调用之前。
/// 预算为零时仍保留最后一个完整单元。
fn find_cut_point(
    entries: &[&SessionEntry],
    keep_recent_tokens: u64,
    image_directory: &std::path::Path,
) -> usize {
    let mut accumulated = 0u64;
    for index in (0..entries.len()).rev() {
        accumulated = accumulated.saturating_add(entry_token_estimate(entries[index], image_directory));
        if accumulated >= keep_recent_tokens
            && !matches!(entries[index], SessionEntry::Message { message: Message::ToolResult { .. }, .. })
        {
            return index;
        }
    }
    0
}

fn model_message(entry: &SessionEntry) -> Message {
    match context_entry(entry).expect("context contains messages and summaries") {
        ContextEntry::Message(message) => message.clone(),
        ContextEntry::Summary(summary) => Message::user(compaction_summary(summary)),
    }
}

/// 请求附加的一个文本块；生成与预算都按整块处理，避免多张图片分别向上取整。
fn image_captions(message: &Message, directory: &std::path::Path) -> String {
    let mut captions = String::new();
    for (index, image) in message.images().enumerate() {
        captions.push_str(&format!(
            "\n[Image #{}: {}; {} × {}; saved copy: {}]",
            index + 1,
            image.name,
            image.width,
            image.height,
            directory.join(&image.id).display()
        ));
    }
    captions
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
            let content = match message {
                Message::Developer { content }
                | Message::Notification { content }
                | Message::User { content, .. }
                | Message::Assistant { content, .. }
                | Message::ToolResult { content, .. } => content,
            };
            content.push(ContentBlock::text(captions));
        }
    }
    Ok(images)
}

/// 当前有效的上下文：最新一份摘要，加上它保留位置之后的消息；没有摘要时是全部消息。
fn resolve_context_entries(session: &SessionData) -> Vec<&SessionEntry> {
    let entries = session.entries();
    let mut context = Vec::new();
    let start = if let Some(entry @ SessionEntry::Compaction { compaction, .. }) =
        entries.iter().rev().find(|entry| matches!(entry, SessionEntry::Compaction { .. }))
    {
        context.push(entry);
        entries
            .iter()
            .position(|candidate| candidate.id() == compaction.first_kept_entry_id)
            .expect("compaction retains an active context entry")
    } else {
        0
    };
    context.extend(
        entries[start..]
            .iter()
            .filter(|entry| matches!(context_entry(entry), Some(ContextEntry::Message(_)))),
    );
    context
}
