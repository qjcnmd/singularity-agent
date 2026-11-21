//! 从会话日志生成发给模型的对话上下文，并估算 Token。
//!
//! 对话来自日志；指令和工具定义由 Agent 提供。普通请求、整理请求、轮换判断和
//! 超限后的重发都用这里生成的上下文。日志本身不被修改。
//!
//! 当上下文接近上限时，可先写入一条 `ToolOutputsPruned` 水位线记录，将水位线及之前
//! 的超长旧工具输出折叠为摘要（并跳过旧图片二进制），保持后续前缀稳定以维持前缀缓存。

use std::fmt::Write as _;

use singularity_core::estimate_tokens_of;
use singularity_model::{ContentBlock, Message};

use super::format::{LedgerRecord, SessionEntry};
use super::manager::SessionData;

fn context_rotation_handoff(session: &SessionData) -> String {
    format!(
        "你正在新的上下文中继续同一段对话。\n\n\
         下一条消息是此前的用户消息整理正文，属于历史材料，不是用户的新发言。相关表述后的时间指向原始消息。例如，[10-31 15:06:32] 可用于在日志中定位对应消息。\n\n\
         结合整理内容和当前情境理解对话，决定如何继续。需要了解工作区时自行检查；需要补充历史信息或核实整理内容时，按时间引用查找相关记录。信息足够时直接继续。\n\n\
         历史日志：{}\n工作目录：{}\n\n\
         回读优先从最近一段历史开始，按需要向前查找。定位后读取相关上下文，结合后续修订和实际执行结果理解记录。",
        singularity_core::display_path(&session.file),
        session.cwd_string(),
    )
}

const UNKNOWN_TOOL_OUTCOME: &str = "[previous execution was interrupted; outcome unknown. Inspect the current state before deciding whether to repeat an action with side effects.]";
const MESSAGE_OVERHEAD_TOKENS: u64 = 4;
const CONTENT_BLOCK_OVERHEAD_TOKENS: u64 = 4;
const TOOL_RESULT_OVERHEAD_TOKENS: u64 = 4;

/// 触发零模型调用微压缩的软阈值（上下文窗口占比）。
const PRUNE_TRIGGER_RATIO: f64 = 0.80;
/// 微压缩时始终完整保留的最近工具结果数量。
const PRUNE_KEEP_RECENT_TOOL_RESULTS: usize = 4;
/// 只有单条工具结果正文超过该字节数才会被微压缩折叠。
const PRUNE_MIN_TOOL_RESULT_BYTES: usize = 4096;
const PRUNE_HEAD_BYTES: usize = 1200;
const PRUNE_TAIL_BYTES: usize = 800;
/// 微压缩至少能回收这么多 Token 才值得写入一条新水位线，避免微小改动频繁打穿前缀缓存。
const PRUNE_MIN_SAVINGS_TOKENS: u64 = 4096;

/// 把日志记录转换成模型消息；工具调用没有结果（如被中断）时补一条“结果未知”，
/// 让每个调用都有对应结果。`pruned_until` 为微压缩水位线在 `entries` 中的位置（含）。
fn project_messages(
    entries: &[&SessionEntry],
    pruned_until: Option<usize>,
    session: &SessionData,
) -> Vec<Message> {
    let mut messages = Vec::new();
    let mut remaining = entries.iter().copied().enumerate().peekable();
    while let Some((index, entry)) = remaining.next() {
        let pruned = pruned_until.is_some_and(|cutoff| index <= cutoff);
        if matches!(entry, SessionEntry::ContextRotation { .. }) {
            messages.push(Message::developer(context_rotation_handoff(session)));
        }
        messages.push(model_message(entry, pruned, session));
        let SessionEntry::Message { message, .. } = entry else {
            continue;
        };
        let mut calls = message.tool_calls().peekable();
        if calls.peek().is_none() {
            continue;
        }
        let mut results = std::collections::HashMap::new();
        while let Some((
            result_index,
            SessionEntry::Message {
                message: Message::ToolResult { tool_call_id, .. },
                ..
            },
        )) = remaining.peek()
        {
            let result_pruned = pruned_until.is_some_and(|cutoff| *result_index <= cutoff);
            let (_, entry) = remaining.next().expect("peeked result exists");
            results.insert(tool_call_id.as_str(), (entry, result_pruned));
        }
        for call in calls {
            messages.push(if let Some((entry, result_pruned)) = results.remove(call.tool_call_id.as_str()) {
                model_message(entry, result_pruned, session)
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

fn context_token_estimate(
    entries: &[&SessionEntry],
    pruned_until: Option<usize>,
    session: &SessionData,
) -> u64 {
    let mut tokens = 0;
    let mut calls = 0;
    let mut results = 0;
    for (index, entry) in entries.iter().enumerate() {
        let pruned = pruned_until.is_some_and(|cutoff| index <= cutoff);
        tokens += entry_token_estimate(entry, pruned, session);
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
    UserMessages(&'a str),
}

/// 判断一条记录是否进入模型上下文；Token 估算、轮换输入和生成请求共用这一判断。
fn context_entry(entry: &SessionEntry) -> Option<ContextEntry<'_>> {
    match entry {
        SessionEntry::Message { observation: Some(observation), .. }
            if observation.status != singularity_protocol::ProviderAttemptStatus::Ok =>
        {
            None
        }
        SessionEntry::Message { message, .. } => Some(ContextEntry::Message(message)),
        SessionEntry::ContextRotation { context_rotation, .. } => {
            Some(ContextEntry::UserMessages(&context_rotation.user_messages))
        }
        SessionEntry::Metadata { .. } | SessionEntry::Record { .. } => None,
    }
}

/// 估算一条记录的 Token；指令和工具定义由 Agent 另外估算。
fn entry_token_estimate(entry: &SessionEntry, pruned: bool, session: &SessionData) -> u64 {
    match context_entry(entry) {
        Some(ContextEntry::Message(message)) => {
            if pruned {
                return message_token_estimate(&model_message(entry, true, session));
            }
            let tokens = message_token_estimate(message)
                + if matches!(
                    message,
                    Message::User { .. } | Message::ToolResult { .. } | Message::Notification { .. }
                ) {
                    text_block_token_estimate(&message_time(entry))
                } else {
                    0
                };
            let captions = image_captions(message, &session.image_directory());
            if captions.is_empty() {
                tokens
            } else {
                tokens.saturating_add(text_block_token_estimate(&captions))
            }
        }
        Some(ContextEntry::UserMessages(_)) => {
            message_token_estimate(&Message::developer(context_rotation_handoff(session)))
                + message_token_estimate(&model_message(entry, false, session))
        }
        None => 0,
    }
}

/// 思考文本也计入预算；不回传思考的模型可能因此稍早轮换。
pub(crate) fn message_token_estimate(message: &Message) -> u64 {
    let mut tokens = MESSAGE_OVERHEAD_TOKENS;
    for block in message.content() {
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
    if matches!(message, Message::ToolResult { .. }) {
        tokens += TOOL_RESULT_OVERHEAD_TOKENS;
    }
    tokens
}

fn text_block_token_estimate(text: &str) -> u64 {
    estimate_tokens_of(text) + CONTENT_BLOCK_OVERHEAD_TOKENS
}

/// 本轮最近一次普通回复的真实用量与后续新增条目估算：`(provider, model, total_tokens, tail_tokens)`。
fn latest_reply_usage(
    session: &SessionData,
    entries: &[&SessionEntry],
    pruned_until: Option<usize>,
) -> Option<(String, String, u64, u64)> {
    let latest = session
        .entries()
        .iter()
        .rev()
        .take_while(|entry| {
            !matches!(
                entry,
                SessionEntry::ContextRotation { .. }
                    | SessionEntry::Record {
                        record: LedgerRecord::OperationStarted | LedgerRecord::ToolOutputsPruned { .. },
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
        let tail_pruned = pruned_until.and_then(|cutoff| cutoff.checked_sub(after));
        let tail_tokens = context_token_estimate(&entries[after..], tail_pruned, session);
        return Some((
            observation.provider.clone(),
            observation.model.clone(),
            usage.total_tokens,
            tail_tokens,
        ));
    }
    None
}

/// 输入 Token：本轮内最近一次回复的实际用量已包含指令、工具定义和回复本身，只补算
/// 之后新增的消息。跨轮、微压缩/轮换后或换了模型时没有可用的实际用量，全部重新估算。
fn input_tokens(
    session: &SessionData,
    entries: &[&SessionEntry],
    pruned_until: Option<usize>,
    identity: (&str, &str),
    definition_tokens: u64,
) -> u64 {
    if let Some((provider, model, total_tokens, tail_tokens)) =
        latest_reply_usage(session, entries, pruned_until)
        && provider == identity.0
        && model == identity.1
    {
        return total_tokens.saturating_add(tail_tokens);
    }
    context_token_estimate(entries, pruned_until, session).saturating_add(definition_tokens)
}

/// 生成请求用的消息和输入 Token 估算；图片由调用方在锁外读取。
pub(crate) fn prepare_context(
    session: &SessionData,
    identity: (&str, &str),
    definition_tokens: u64,
) -> (Vec<Message>, u64) {
    let (entries, pruned_until) = resolve_context_entries(session);
    (
        project_messages(&entries, pruned_until, session),
        input_tokens(session, &entries, pruned_until, identity, definition_tokens),
    )
}

/// 只估算输入 Token，轮换判断和输出预算共用。
pub(crate) fn estimated_tokens(session: &SessionData, identity: (&str, &str), definition_tokens: u64) -> u64 {
    let (entries, pruned_until) = resolve_context_entries(session);
    input_tokens(session, &entries, pruned_until, identity, definition_tokens)
}

/// 若上下文达到微压缩阈值且存在足够多可折叠的旧工具输出/旧图片，返回新的锚定水位线条目 ID。
pub(crate) fn select_prune_target(
    session: &SessionData,
    identity: (&str, &str),
    definition_tokens: u64,
    context_window: u64,
) -> Option<(String, u64)> {
    let (entries, existing_pruned) = resolve_context_entries(session);
    let current = input_tokens(session, &entries, existing_pruned, identity, definition_tokens);
    let trigger = (context_window as f64 * PRUNE_TRIGGER_RATIO).floor() as u64;
    if current < trigger {
        return None;
    }
    let tool_positions: Vec<usize> = entries
        .iter()
        .enumerate()
        .filter_map(|(index, entry)| {
            matches!(context_entry(entry), Some(ContextEntry::Message(Message::ToolResult { .. })))
                .then_some(index)
        })
        .collect();
    if tool_positions.len() <= PRUNE_KEEP_RECENT_TOOL_RESULTS {
        return None;
    }
    let candidate_cutoff = tool_positions[tool_positions.len() - PRUNE_KEEP_RECENT_TOOL_RESULTS - 1];
    if existing_pruned.is_some_and(|existing| candidate_cutoff <= existing) {
        return None;
    }
    let before_est = context_token_estimate(&entries, existing_pruned, session);
    let after_est = context_token_estimate(&entries, Some(candidate_cutoff), session);
    let saved = before_est.saturating_sub(after_est);
    let min_savings = PRUNE_MIN_SAVINGS_TOKENS.max(context_window / 20);
    if saved < min_savings {
        return None;
    }
    Some((entries[candidate_cutoff].id().to_owned(), saved))
}

/// 整理请求使用完整的当前上下文，与普通请求共用消息投影。
pub struct ContextRotationInput {
    pub(crate) messages: Vec<Message>,
    pub(crate) image_directory: std::path::PathBuf,
    /// 包括上一份用户消息整理内容和接手提示。
    pub(crate) estimated_tokens: u64,
    pub(crate) latest_usage: Option<(String, String, u64, u64)>,
}

impl ContextRotationInput {
    /// 没有新增对话时无需准备模型、指令或工具。
    pub async fn select(session: &super::SessionWriter) -> Option<Self> {
        let session = std::sync::Arc::clone(session);
        tokio::task::spawn_blocking(move || {
            let session = super::lock_writer(&session);
            let (entries, pruned_until) = resolve_context_entries(&session);
            entries.iter().any(|entry| matches!(entry, SessionEntry::Message { .. })).then(|| Self {
                messages: project_messages(&entries, pruned_until, &session),
                estimated_tokens: context_token_estimate(&entries, pruned_until, &session),
                latest_usage: latest_reply_usage(&session, &entries, pruned_until),
                image_directory: session.image_directory(),
            })
        })
        .await
        .expect("context_rotation selection completes while the runtime is running")
    }
}

fn model_message(entry: &SessionEntry, pruned: bool, session: &SessionData) -> Message {
    match context_entry(entry).expect("context contains messages and user message notes") {
        ContextEntry::Message(message) => {
            let mut message = message.clone();
            if pruned {
                prune_message_content(&mut message, entry, session);
            }
            if let Message::User { content }
            | Message::ToolResult { content, .. }
            | Message::Notification { content } = &mut message
            {
                content.push(ContentBlock::text(message_time(entry)));
            }
            message
        }
        ContextEntry::UserMessages(user_messages) => {
            Message::user(format!("上一版用户消息整理正文：\n\n{user_messages}"))
        }
    }
}

/// 对微压缩水位线及之前的消息执行确定性压缩：折叠超长工具输出并剥离旧图片二进制（保留路径说明）。
fn prune_message_content(message: &mut Message, entry: &SessionEntry, session: &SessionData) {
    let captions = image_captions(message, &session.image_directory());
    let had_images = !captions.is_empty();
    let content = match message {
        Message::ToolResult { content, .. } => {
            for block in content.iter_mut() {
                if let ContentBlock::Text { text, .. } = block
                    && text.len() > PRUNE_MIN_TOOL_RESULT_BYTES
                {
                    *text = crate::compaction::compact_text(
                        text,
                        PRUNE_HEAD_BYTES,
                        PRUNE_TAIL_BYTES,
                        &format!(
                            "earlier tool output compacted ({} bytes); inspect {} at {} if needed",
                            text.len(),
                            singularity_core::display_path(&session.file),
                            message_time(entry),
                        ),
                    );
                }
            }
            content
        }
        Message::User { content } | Message::Notification { content } => content,
        Message::Developer { .. } | Message::Assistant { .. } => return,
    };
    if had_images {
        content.retain(|block| !matches!(block, ContentBlock::Image(_)));
        content.push(ContentBlock::text(format!(
            "{captions}\n[earlier image omitted from context; inspect the saved copy path if needed]"
        )));
    }
}

/// 引用沿用日志的 UTC 时间，只缩短显示，不改写原始消息。
fn message_time(entry: &SessionEntry) -> String {
    let time = entry.timestamp().chars().skip(5).take(14).collect::<String>().replace('T', " ");
    let label = match entry {
        SessionEntry::Message { message: Message::ToolResult { .. }, .. } => "工具记录时间：",
        SessionEntry::Message {
            message: Message::Notification { .. }, ..
        } => "通知记录时间：",
        _ => "",
    };
    format!("[{label}{time}]")
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
                | Message::User { content }
                | Message::Assistant { content, .. }
                | Message::ToolResult { content, .. } => content,
            };
            content.push(ContentBlock::text(captions));
        }
    }
    Ok(images)
}

/// 最新整理记录本身就是分界；只接续该记录之后的正常消息，同时解析该分界之后最新的微压缩水位线。
fn resolve_context_entries(session: &SessionData) -> (Vec<&SessionEntry>, Option<usize>) {
    let entries = session.entries();
    let start = entries
        .iter()
        .rposition(|entry| matches!(entry, SessionEntry::ContextRotation { .. }))
        .unwrap_or(0);
    let active_slice = &entries[start..];
    let prune_before_id = active_slice.iter().rev().find_map(|entry| match entry {
        SessionEntry::Record {
            record: LedgerRecord::ToolOutputsPruned { before_entry_id },
            ..
        } => Some(before_entry_id.as_str()),
        _ => None,
    });
    let active: Vec<&SessionEntry> =
        active_slice.iter().filter(|entry| context_entry(entry).is_some()).collect();
    let pruned_until =
        prune_before_id.and_then(|target| active.iter().rposition(|entry| entry.id() == target));
    (active, pruned_until)
}
