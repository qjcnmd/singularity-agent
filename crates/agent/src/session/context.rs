//! 从会话日志生成发给模型的对话上下文，并估算 Token。
//!
//! 对话来自日志；指令和工具定义由 Agent 提供。普通请求、整理请求、轮换判断和
//! 超限后的重发都用这里生成的上下文。日志本身不被修改。

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

/// 把日志记录转换成模型消息；工具调用没有结果（如被中断）时补一条“结果未知”，
/// 让每个调用都有对应结果。
fn project_messages(entries: &[&SessionEntry], session: &SessionData) -> Vec<Message> {
    let mut messages = Vec::new();
    let mut remaining = entries.iter().copied().peekable();
    while let Some(entry) = remaining.next() {
        if matches!(entry, SessionEntry::ContextRotation { .. }) {
            messages.push(Message::developer(context_rotation_handoff(session)));
        }
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

fn context_token_estimate(entries: &[&SessionEntry], session: &SessionData) -> u64 {
    let mut tokens = 0;
    let mut calls = 0;
    let mut results = 0;
    for entry in entries {
        tokens += entry_token_estimate(entry, session);
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
fn entry_token_estimate(entry: &SessionEntry, session: &SessionData) -> u64 {
    match context_entry(entry) {
        Some(ContextEntry::Message(message)) => {
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
                + message_token_estimate(&model_message(entry))
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

/// 思考文本也计入预算；不回传思考的模型可能因此稍早轮换。
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
/// 之后新增的消息。跨轮、轮换后或换了模型时没有可用的实际用量，全部重新估算。
fn input_tokens(
    session: &SessionData,
    entries: &[&SessionEntry],
    identity: (&str, &str),
    definition_tokens: u64,
) -> u64 {
    let latest = session
        .entries()
        .iter()
        .rev()
        .take_while(|entry| {
            !matches!(
                entry,
                SessionEntry::ContextRotation { .. }
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
        return usage.total_tokens.saturating_add(context_token_estimate(&entries[after..], session));
    }
    context_token_estimate(entries, session).saturating_add(definition_tokens)
}

/// 生成请求用的消息和输入 Token 估算；图片由调用方在锁外读取。
pub(crate) fn prepare_context(
    session: &SessionData,
    identity: (&str, &str),
    definition_tokens: u64,
) -> (Vec<Message>, u64) {
    let entries = resolve_context_entries(session);
    (
        project_messages(&entries, session),
        input_tokens(session, &entries, identity, definition_tokens),
    )
}

/// 只估算输入 Token，轮换判断和输出预算共用。
pub(crate) fn estimated_tokens(session: &SessionData, identity: (&str, &str), definition_tokens: u64) -> u64 {
    input_tokens(session, &resolve_context_entries(session), identity, definition_tokens)
}

/// 整理请求使用完整的当前上下文，与普通请求共用消息投影。
pub struct ContextRotationInput {
    pub(crate) messages: Vec<Message>,
    pub(crate) image_directory: std::path::PathBuf,
    /// 包括上一份用户消息整理内容和接手提示。
    pub(crate) estimated_tokens: u64,
}

impl ContextRotationInput {
    /// 没有新增对话时无需准备模型、指令或工具。
    pub async fn select(session: &super::SessionWriter) -> Option<Self> {
        let session = std::sync::Arc::clone(session);
        tokio::task::spawn_blocking(move || {
            let session = super::lock_writer(&session);
            let entries = resolve_context_entries(&session);
            entries.iter().any(|entry| matches!(entry, SessionEntry::Message { .. })).then(|| Self {
                messages: project_messages(&entries, &session),
                estimated_tokens: context_token_estimate(&entries, &session),
                image_directory: session.image_directory(),
            })
        })
        .await
        .expect("context_rotation selection completes while the runtime is running")
    }
}

fn model_message(entry: &SessionEntry) -> Message {
    match context_entry(entry).expect("context contains messages and user message notes") {
        ContextEntry::Message(message) => {
            let mut message = message.clone();
            if let Message::User { content, .. }
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

/// 最新整理记录本身就是分界；只接续该记录之后的正常消息。
fn resolve_context_entries(session: &SessionData) -> Vec<&SessionEntry> {
    let entries = session.entries();
    let start = entries
        .iter()
        .rposition(|entry| matches!(entry, SessionEntry::ContextRotation { .. }))
        .unwrap_or(0);
    entries[start..].iter().filter(|entry| context_entry(entry).is_some()).collect()
}
