//! 上下文视图：从会话 ledger 派生出模型请求的输入，并统一计量。
//!
//! 对话和固定定义都来自会话记录；普通请求与摘要请求共用对话投影。
//! 请求装配、压缩判定与溢出恢复共用这一个视图。原始会话始终由会话 ledger 持有。

use singularity_model::{ModelMessage, ModelRole};

use crate::message::{AgentMessage, COMPACTION_SUMMARY_PREFIX, COMPACTION_SUMMARY_SUFFIX, ContentBlock};

use super::format::SessionEntry;
use super::manager::SessionData;

/// 启发式 Token 估算：按 UTF-16 字符数除以 4 向上取整。
pub(crate) fn estimate_tokens_of(text: &str) -> u64 {
    let chars = text.encode_utf16().count() as u64;
    chars.div_ceil(4)
}

fn compaction_summary(summary: &str) -> String {
    format!("{COMPACTION_SUMMARY_PREFIX}{summary}{COMPACTION_SUMMARY_SUFFIX}")
}

const UNKNOWN_TOOL_OUTCOME: &str = "[previous execution was interrupted; outcome unknown. Inspect the current state before deciding whether to repeat an action with side effects.]";

/// 将会话条目映射为模型消息列表；工具调用缺少对应结果（如被中断）时补一条占位消息，让调用配对闭合。
fn project_messages(entries: &[&SessionEntry]) -> Vec<ContextMessage> {
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
        for call in calls {
            let next_matches = remaining.peek().is_some_and(|entry| {
                matches!(entry,
                SessionEntry::Message { message: AgentMessage::ToolResult { tool_call_id, .. }, .. }
                    if tool_call_id == &call.tool_call_id)
            });
            messages.push(if next_matches {
                model_message(remaining.next().expect("matching result exists"))
            } else {
                let mut result = ModelMessage::text(ModelRole::Tool, UNKNOWN_TOOL_OUTCOME);
                result.tool_call_id = Some(call.tool_call_id.clone());
                ContextMessage { message: result, images: Vec::new() }
            });
        }
    }
    messages
}

fn context_token_estimate(entries: &[&SessionEntry]) -> u64 {
    let mut tokens = 0;
    let mut calls = 0;
    let mut results = 0;
    for entry in entries {
        tokens += entry_token_estimate(entry);
        if let SessionEntry::Message { message, .. } = entry {
            calls += message.tool_calls().count();
            if message.tool_call_id().is_some() {
                results += 1;
            }
        }
    }
    let unknown_result_tokens =
        content_token_estimate(&[ContentBlock::Text { text: UNKNOWN_TOOL_OUTCOME.to_string() }], true);
    tokens + (calls - results) as u64 * unknown_result_tokens
}

enum ContextEntry<'a> {
    Message(&'a AgentMessage),
    Summary(&'a str),
}

/// 把 ledger 条目穷尽分类；计量、保留边界和请求投影共用这一处可见性判断。
fn context_entry(entry: &SessionEntry) -> Option<ContextEntry<'_>> {
    match entry {
        SessionEntry::Message { message, .. } => Some(ContextEntry::Message(message)),
        SessionEntry::Compaction { compaction, .. } => Some(ContextEntry::Summary(&compaction.summary)),
        SessionEntry::Metadata { .. } | SessionEntry::Record { .. } => None,
    }
}

/// 估算可压缩历史的模型内容；当前文件指令由 Agent 另行计量。
pub(crate) fn entry_token_estimate(entry: &SessionEntry) -> u64 {
    match context_entry(entry) {
        Some(ContextEntry::Message(message)) => message_token_estimate(message),
        Some(ContextEntry::Summary(summary)) => estimate_tokens_of(&compaction_summary(summary)) + 8,
        None => 0,
    }
}

pub(crate) fn message_token_estimate(message: &crate::message::AgentMessage) -> u64 {
    content_token_estimate(message.content(), matches!(message, AgentMessage::ToolResult { .. }))
}

/// 只估算模型请求真正会带上的内容：公开思考不进入文本投影
/// （见 `model_message`），所以计零。
fn content_token_estimate(content: &[ContentBlock], tool_result: bool) -> u64 {
    let mut tokens = 4u64;
    for block in content {
        tokens = tokens.saturating_add(match block {
            ContentBlock::Text { text } => estimate_tokens_of(text) + 4,
            ContentBlock::Thinking { .. } => 0,
            // 仅用于发送前的启发式压力判断；实际用量仍由提供方 usage 校正。
            ContentBlock::Image(image) => {
                u64::from(image.width).div_ceil(32) * u64::from(image.height).div_ceil(32) + 128
            }
            ContentBlock::ToolCall(call) => {
                estimate_tokens_of(&call.tool_name) + estimate_tokens_of(&call.arguments.to_string()) + 4
            }
        });
    }
    if tool_result {
        tokens += 4;
    }
    tokens
}

/// 普通请求共用一次历史整理，生成消息材料和对应估价；图片仍由调用方在锁外读取。
pub(crate) fn prepare_context(session: &SessionData) -> (Vec<ContextMessage>, u64) {
    let entries = resolve_context_entries(session);
    (project_messages(&entries), context_token_estimate(&entries))
}

/// 本轮请求开销与实测校正由 Agent 添加。
pub(crate) fn estimated_tokens(session: &SessionData) -> u64 {
    context_token_estimate(&resolve_context_entries(session))
}

/// 已按工具配对边界选好的摘要前缀。
pub(crate) struct CompactionPrefix {
    pub(crate) messages: Vec<ContextMessage>,
    pub(crate) image_directory: std::path::PathBuf,
    /// 选中历史的已有估价；上一份摘要由摘要请求的更新指令另行计量。
    pub(crate) estimated_tokens: u64,
    pub(crate) previous_summary: Option<String>,
    pub(crate) first_kept_entry_id: String,
}

pub(crate) fn compaction_prefix(session: &SessionData, keep_recent_ratio: f64) -> Option<CompactionPrefix> {
    let entries = resolve_context_entries(session);
    let keep_recent_tokens = (context_token_estimate(&entries) as f64 * keep_recent_ratio).ceil() as u64;
    let cut = find_cut_point(&entries, keep_recent_tokens);
    if cut == 0 {
        return None;
    }
    // 已有摘要交给更新指令，新摘要的对话前缀从它之后开始。
    let (previous_summary, conversation_start) = match entries[0] {
        SessionEntry::Compaction { compaction, .. } => (Some(compaction.summary.clone()), 1),
        _ => (None, 0),
    };
    let selected = &entries[conversation_start..cut];
    let messages = project_messages(selected);
    if messages.is_empty() {
        return None;
    }
    Some(CompactionPrefix {
        messages,
        image_directory: session.image_directory(),
        estimated_tokens: context_token_estimate(selected),
        previous_summary,
        first_kept_entry_id: entries[cut].id().to_string(),
    })
}

/// 从后往前累加到保留预算；若当前记录是工具结果，继续退到整组调用之前。
/// 预算为零时仍保留最后一个完整单元。
fn find_cut_point(entries: &[&SessionEntry], keep_recent_tokens: u64) -> usize {
    let mut accumulated = 0u64;
    for index in (0..entries.len()).rev() {
        accumulated = accumulated.saturating_add(entry_token_estimate(entries[index]));
        if accumulated >= keep_recent_tokens
            && !matches!(
                entries[index],
                SessionEntry::Message {
                    message: AgentMessage::ToolResult { .. },
                    ..
                }
            )
        {
            return index;
        }
    }
    0
}

fn model_message(entry: &SessionEntry) -> ContextMessage {
    let message = match context_entry(entry).expect("context contains only messages and summaries") {
        ContextEntry::Message(message) => message,
        ContextEntry::Summary(summary) => {
            return ContextMessage {
                message: ModelMessage::text(ModelRole::User, compaction_summary(summary)),
                images: Vec::new(),
            };
        }
    };
    let text = message.content_text();
    let model_message = match message {
        AgentMessage::User { .. } | AgentMessage::Notification { .. } => {
            ModelMessage::text(ModelRole::User, text)
        }
        AgentMessage::Assistant { .. } => ModelMessage {
            role: ModelRole::Assistant,
            content: text,
            images: Vec::new(),
            tool_call_id: None,
            tool_calls: message.tool_calls().cloned().collect(),
            provider_reasoning_replay: message.provider_reasoning_replay().cloned(),
        },
        AgentMessage::ToolResult { tool_call_id, .. } => {
            let mut result = ModelMessage::text(ModelRole::Tool, text);
            result.tool_call_id = Some(tool_call_id.clone());
            result
        }
    };
    ContextMessage {
        message: model_message,
        images: message.images().cloned().collect(),
    }
}

pub(crate) struct ContextMessage {
    message: ModelMessage,
    images: Vec<singularity_protocol::ImageAttachment>,
}

/// 生成和摘要共用的模型投影。图片读取发生在写者锁之外。
pub(crate) fn load_messages(
    materials: Vec<ContextMessage>,
    directory: &std::path::Path,
) -> super::Result<Vec<ModelMessage>> {
    materials
        .into_iter()
        .map(|material| {
            let mut message = material.message;
            for (index, image) in material.images.iter().enumerate() {
                let path = directory.join(&image.id);
                message.content.push_str(&format!(
                    "\n[Image #{}: {}; {} × {}; saved copy: {}]",
                    index + 1,
                    image.name,
                    image.width,
                    image.height,
                    path.display()
                ));
                message.images.push(crate::image::load_image(directory, image)?);
            }
            Ok(message)
        })
        .collect()
}

/// 最新摘要替换此前的摘要和历史前缀，保留位置之后的消息仍按日志顺序排列。
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
    context.extend(entries[start..].iter().filter(|entry| matches!(entry, SessionEntry::Message { .. })));
    context
}
