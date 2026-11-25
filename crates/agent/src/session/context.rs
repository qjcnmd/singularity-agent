use std::fmt::Write as _;

use app_core::estimate_tokens_of;
use model::{ContentBlock, Message};

use super::format::SessionEntry;
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

pub(super) fn context_token_estimate_for(
    entries: &[&SessionEntry],
    session: &SessionData,
    target: Option<(&str, &str)>,
) -> u64 {
    let mut tokens = 0u64;
    let mut calls = 0usize;
    let mut results = 0usize;
    for entry in entries {
        tokens = tokens.saturating_add(entry_token_estimate_for(entry, session, target));
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

pub(super) enum ContextEntry<'a> {
    Message(&'a Message),
    Summary(&'a str),
}

pub(super) fn context_entry(entry: &SessionEntry) -> Option<ContextEntry<'_>> {
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

pub(super) fn message_with_captions_tokens(
    message: &Message,
    image_directory: &std::path::Path,
    target: Option<(&str, &str)>,
) -> u64 {
    let captions = image_captions(message, image_directory);
    let mut tokens = message_token_estimate_for(message, target);
    if !captions.is_empty() {
        tokens = tokens.saturating_add(text_block_token_estimate(&captions));
    }
    tokens
}

pub(super) fn entry_token_estimate_for(
    entry: &SessionEntry,
    session: &SessionData,
    target: Option<(&str, &str)>,
) -> u64 {
    match context_entry(entry) {
        Some(ContextEntry::Message(message)) => {
            message_with_captions_tokens(message, &session.image_directory(), target)
        }
        Some(ContextEntry::Summary(summary)) => message_token_estimate(&frame_compacted_summary(summary)),
        None => 0,
    }
}

pub(crate) fn message_token_estimate(message: &Message) -> u64 {
    message_token_estimate_for(message, None)
}

pub(crate) fn message_token_estimate_for(message: &Message, target: Option<(&str, &str)>) -> u64 {
    let keep_thinking = match (target, message) {
        (Some((p, m)), Message::Assistant { provider_metadata, .. }) => provider_metadata
            .as_ref()
            .is_some_and(|meta| meta.provider_name == p && meta.model_name == m),
        _ => true,
    };
    let mut block_tokens = 0u64;
    let mut kept_blocks = 0usize;
    for block in message.content() {
        if !keep_thinking && matches!(block, ContentBlock::Thinking { .. }) {
            continue;
        }
        kept_blocks += 1;
        block_tokens = block_tokens.saturating_add(match block {
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
    if matches!(message, Message::Assistant { .. }) && kept_blocks == 0 {
        return 0;
    }
    let mut tokens = MESSAGE_OVERHEAD_TOKENS.saturating_add(block_tokens);
    if matches!(message, Message::ToolResult { .. }) {
        tokens = tokens.saturating_add(TOOL_RESULT_OVERHEAD_TOKENS);
    }
    tokens
}

fn text_block_token_estimate(text: &str) -> u64 {
    estimate_tokens_of(text).saturating_add(CONTENT_BLOCK_OVERHEAD_TOKENS)
}

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

pub(super) fn latest_reply_usage<'a>(
    session: &'a SessionData,
    entries: &[&SessionEntry],
    definition_tokens: u64,
) -> Option<(&'a str, &'a str, u64)> {
    let latest = session
        .entries()
        .iter()
        .rev()
        .take_while(|entry| !matches!(entry, SessionEntry::Compaction { .. }))
        .find(|entry| matches!(context_entry(entry), Some(ContextEntry::Message(Message::Assistant { .. }))));
    let Some(SessionEntry::Message { id, observation: Some(obs), .. }) = latest else {
        return None;
    };
    let usage = obs.usage.as_ref()?;
    let reply_pos = entries.iter().position(|e| e.id() == id).expect("latest assistant in active context");
    let id_pair = (obs.provider.as_str(), obs.model.as_str());
    let tail = context_token_estimate_for(&entries[reply_pos + 1..], session, Some(id_pair));
    let calibrated = match session.last_reply_definition_tokens.as_ref() {
        Some((recorded_id, prev_def)) if recorded_id == id => {
            let adjusted = if definition_tokens >= *prev_def {
                usage.total_tokens.saturating_add(definition_tokens - *prev_def)
            } else {
                usage.total_tokens.saturating_sub(*prev_def - definition_tokens).max(definition_tokens)
            };
            adjusted.saturating_add(tail)
        }
        _ => {
            let est_prev_input = context_token_estimate_for(&entries[..reply_pos], session, Some(id_pair))
                .saturating_add(definition_tokens);
            let def_growth = est_prev_input.saturating_sub(usage.input_tokens);
            usage.total_tokens.saturating_add(def_growth).saturating_add(tail)
        }
    };
    Some((id_pair.0, id_pair.1, calibrated))
}

pub(crate) fn input_tokens(
    session: &SessionData,
    entries: &[&SessionEntry],
    identity: (&str, &str),
    definition_tokens: u64,
) -> u64 {
    if let Some((provider, model, total)) = latest_reply_usage(session, entries, definition_tokens)
        && (provider, model) == identity
    {
        return total;
    }
    context_token_estimate_for(entries, session, Some(identity)).saturating_add(definition_tokens)
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
) -> PreparedContext {
    let entries = resolve_context_entries(session);
    let tokens = input_tokens(session, &entries, identity, definition_tokens);
    PreparedContext::new(session, &entries, tokens)
}

pub(crate) struct PreparedContext {
    pub(crate) session_id: String,
    pub(crate) messages: Vec<Message>,
    pub(crate) input_tokens: u64,
    pub(crate) image_directory: std::path::PathBuf,
}

impl PreparedContext {
    pub(crate) fn new(session: &SessionData, entries: &[&SessionEntry], input_tokens: u64) -> Self {
        Self {
            session_id: session.session_id().to_owned(),
            messages: project_messages(entries),
            input_tokens,
            image_directory: session.image_directory(),
        }
    }
}

fn model_message(entry: &SessionEntry) -> Message {
    match context_entry(entry).expect("context contains messages and compaction summaries") {
        ContextEntry::Message(message) => message.clone(),
        ContextEntry::Summary(summary) => frame_compacted_summary(summary),
    }
}

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

pub(super) fn message_content_mut(message: &mut Message) -> &mut Vec<ContentBlock> {
    match message {
        Message::Developer { content }
        | Message::Notification { content }
        | Message::User { content }
        | Message::Assistant { content, .. }
        | Message::ToolResult { content, .. } => content,
    }
}

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
