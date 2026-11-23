use std::fmt::Write as _;

use app_core::estimate_tokens_of;
use model::{ContentBlock, Message, ModelConfigurationSnapshot};

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

fn context_token_estimate_for(
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

fn message_with_captions_tokens(
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

fn entry_token_estimate_for(
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

fn latest_reply_usage<'a>(
    session: &'a SessionData,
    entries: &[&SessionEntry],
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
    let after = entries.iter().position(|e| e.id() == id).expect("latest assistant in active context") + 1;
    let id_pair = (obs.provider.as_str(), obs.model.as_str());
    let tail = context_token_estimate_for(&entries[after..], session, Some(id_pair));
    Some((id_pair.0, id_pair.1, usage.total_tokens.saturating_add(tail)))
}

pub(crate) fn input_tokens(
    session: &SessionData,
    entries: &[&SessionEntry],
    identity: (&str, &str),
    definition_tokens: u64,
) -> u64 {
    if let Some((provider, model, total)) = latest_reply_usage(session, entries)
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

pub fn retention_budget_tokens(model: ModelConfigurationSnapshot) -> u64 {
    let message_budget = model.context_window().saturating_sub(u64::from(model.max_output_tokens));
    (message_budget * 15 / 100).max(1)
}

fn balanced_units(active: &[&SessionEntry]) -> Vec<std::ops::Range<usize>> {
    let msg_start = usize::from(matches!(active.first(), Some(SessionEntry::Compaction { .. })));
    let mut units: Vec<std::ops::Range<usize>> = Vec::new();
    for (index, entry) in active.iter().enumerate().skip(msg_start) {
        if matches!(context_entry(entry), Some(ContextEntry::Message(Message::ToolResult { .. })))
            && let Some(previous) = units.last_mut()
        {
            previous.end = index + 1;
        } else {
            units.push(index..index + 1);
        }
    }
    units
}

pub(crate) fn select_compactable_split(
    active: &[&SessionEntry],
    session: &SessionData,
    retain_tokens: u64,
) -> Option<(usize, String)> {
    let units = balanced_units(active);
    if units.is_empty() {
        return None;
    }

    let mut accumulated = 0u64;
    let mut keep_unit = units.len();
    for (idx, range) in units.iter().enumerate().rev() {
        let unit_tokens = context_token_estimate_for(&active[range.clone()], session, None);
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
    pub(crate) first_kept_entry_id: String,
}

impl CompactionInput {
    pub async fn select(session: &super::SessionWriter, retain_tokens: u64) -> Option<Self> {
        let session = std::sync::Arc::clone(session);
        tokio::task::spawn_blocking(move || {
            super::with_writer(&session, |session| {
                let data = session.data();
                let active = resolve_context_entries(data);
                let (split_idx, first_kept_entry_id) =
                    select_compactable_split(&active, data, retain_tokens)?;
                let prefix_estimated_tokens = context_token_estimate_for(&active[..split_idx], data, None);
                if prefix_estimated_tokens <= message_token_estimate(&frame_compacted_summary("")) {
                    return None;
                }
                Some(Self { first_kept_entry_id })
            })
        })
        .await
        .expect("compaction selection completes while the runtime is running")
    }
}

#[derive(Debug)]
pub(crate) struct PlannedCompactionStep {
    pub(crate) session_id: String,
    pub(crate) messages: Vec<Message>,
    pub(crate) first_kept_entry_id: String,
    pub(crate) raw_prefix_tokens: u64,
    pub(crate) estimated_request_input: u64,
    pub(crate) image_directory: std::path::PathBuf,
    pub(crate) is_final_chunk: bool,
}

fn scale_u64(value: u64, num: u64, den: u64, ceil: bool) -> u64 {
    let (prod, d) = (u128::from(value) * u128::from(num), u128::from(den));
    u64::try_from(if ceil { prod.div_ceil(d) } else { prod / d }).unwrap_or(u64::MAX)
}

pub(crate) fn compaction_input_limit(
    model: ModelConfigurationSnapshot,
    definition_tokens: u64,
    instruction_tokens: u64,
    overflow_input_tokens: Option<u64>,
) -> Option<u64> {
    let min_out = crate::request_execution::min_compaction_output_tokens(model);
    let min_req = definition_tokens.saturating_add(instruction_tokens);
    let window_limit = model.context_window().saturating_sub(min_out);
    let limit = match overflow_input_tokens {
        Some(overflow) if overflow <= min_req.saturating_add(1) => return None,
        Some(overflow) => window_limit
            .min(overflow.saturating_sub(min_out.max(overflow / 5)).max(min_req + 1).min(overflow - 1)),
        None => window_limit,
    };
    (limit > min_req).then_some(limit)
}

pub(crate) fn plan_compaction_step(
    data: &SessionData,
    target_first_kept_entry_id: &str,
    identity: (&str, &str),
    definition_tokens: u64,
    instruction_tokens: u64,
    model: ModelConfigurationSnapshot,
    overflow_input_tokens: Option<u64>,
) -> Result<Option<PlannedCompactionStep>, &'static str> {
    const NO_HEADROOM: &str = "当前指令与工具定义已占满模型上下文窗口，无法留出压缩摘要空间。";
    let active = resolve_context_entries(data);
    let Some(target_split_idx) = active.iter().position(|e| e.id() == target_first_kept_entry_id) else {
        return Ok(None);
    };
    let prefix_units: Vec<_> =
        balanced_units(&active).into_iter().take_while(|r| r.end <= target_split_idx).collect();
    let empty_summary = message_token_estimate(&frame_compacted_summary(""));
    if prefix_units.is_empty()
        || context_token_estimate_for(&active[..target_split_idx], data, Some(identity)) <= empty_summary
    {
        return Ok(None);
    }

    let limit = compaction_input_limit(model, definition_tokens, instruction_tokens, overflow_input_tokens)
        .ok_or(NO_HEADROOM)?;
    let full_est =
        definition_tokens.saturating_add(context_token_estimate_for(&active, data, Some(identity)));
    let calibrated = latest_reply_usage(data, &active)
        .filter(|&(p, m, real)| (p, m) == identity && real > 0 && full_est > 0)
        .map(|(_, _, real)| real);
    let max_base = limit.saturating_sub(instruction_tokens);
    let max_prefix = calibrated
        .map_or(max_base, |real| scale_u64(max_base, full_est, real, false))
        .saturating_sub(definition_tokens);
    if max_prefix <= empty_summary {
        return Err(NO_HEADROOM);
    }

    let mut acc = active
        .first()
        .filter(|e| matches!(e, SessionEntry::Compaction { .. }))
        .map_or(0, |e| entry_token_estimate_for(e, data, Some(identity)));
    let mut fitting = 0usize;
    for range in &prefix_units {
        let next =
            acc.saturating_add(context_token_estimate_for(&active[range.clone()], data, Some(identity)));
        if next > max_prefix {
            break;
        }
        acc = next;
        fitting += 1;
    }

    let min_chunk = empty_summary
        .saturating_add(instruction_tokens)
        .min(max_prefix / 2)
        .max(empty_summary + 1);
    let use_fit = fitting == prefix_units.len() || (fitting >= 1 && acc >= min_chunk);
    let chunk_end = prefix_units[fitting - usize::from(use_fit)].end;
    let raw_prefix_tokens = context_token_estimate_for(&active[..chunk_end], data, Some(identity));
    let image_directory = data.image_directory();
    let mut messages = project_messages(&active[..chunk_end]);
    let mut projected: u64 = messages
        .iter()
        .map(|m| message_with_captions_tokens(m, &image_directory, Some(identity)))
        .sum();
    if projected > max_prefix {
        projected = fold_oversized_messages(&mut messages, max_prefix);
        if projected > max_prefix {
            return Err(NO_HEADROOM);
        }
    }
    let base = definition_tokens.saturating_add(projected);
    let scaled = calibrated.map_or(base, |real| scale_u64(base, real, full_est, true));
    Ok(Some(PlannedCompactionStep {
        session_id: data.session_id().to_owned(),
        messages,
        first_kept_entry_id: active[chunk_end].id().to_owned(),
        raw_prefix_tokens,
        estimated_request_input: scaled.saturating_add(instruction_tokens),
        image_directory,
        is_final_chunk: chunk_end == target_split_idx,
    }))
}

fn fold_oversized_messages(messages: &mut [Message], max_tokens: u64) -> u64 {
    let sum = |ms: &[Message]| -> u64 { ms.iter().map(message_token_estimate).sum() };
    for content in messages.iter_mut().map(message_content_mut) {
        content.retain(|block| !matches!(block, ContentBlock::Thinking { .. }));
        for block in content {
            if let ContentBlock::Image(img) = block {
                let text =
                    format!("[Image omitted for compaction: {}; {} × {}]", img.name, img.width, img.height);
                *block = ContentBlock::text(text);
            }
        }
    }
    for tool_only in [true, false] {
        let mut current = sum(messages);
        if current <= max_tokens {
            return current;
        }
        if !tool_only {
            for block in messages.iter_mut().flat_map(message_content_mut) {
                if let ContentBlock::ToolCall(call) = block {
                    call.arguments = serde_json::json!({});
                }
            }
            current = sum(messages);
            if current <= max_tokens {
                return current;
            }
        }
        let texts: Vec<&mut String> = messages
            .iter_mut()
            .filter(|m| !tool_only || matches!(m, Message::ToolResult { .. }))
            .flat_map(message_content_mut)
            .filter_map(|b| match b {
                ContentBlock::Text { text, .. } => Some(text),
                _ => None,
            })
            .collect();
        let total_text: u64 = texts.iter().map(|t| estimate_tokens_of(t)).sum();
        if total_text == 0 {
            continue;
        }
        let keep = total_text.saturating_sub(current - max_tokens);
        for text in texts {
            let tokens = estimate_tokens_of(text);
            let cap = scale_u64(tokens, keep, total_text, false);
            if tokens > cap {
                trim_text_to_tokens(text, cap);
            }
        }
    }
    sum(messages)
}

fn trim_text_to_tokens(text: &mut String, max_tokens: u64) {
    const MARKER: &str = "\n...[truncated for compaction]...\n";
    let (budget, suffix) = max_tokens
        .checked_sub(estimate_tokens_of(MARKER) + 1)
        .map_or((max_tokens, ""), |b| (b + 1, MARKER));
    let mut len = text
        .floor_char_boundary(usize::try_from(budget.saturating_mul(4)).unwrap_or(text.len()).min(text.len()));
    while len > 0 {
        let est = estimate_tokens_of(&text[..len]);
        if est <= budget {
            break;
        }
        let next = usize::try_from(scale_u64(u64::try_from(len).unwrap_or(u64::MAX), budget, est, false))
            .unwrap_or(0)
            .min(len - 1);
        len = text.floor_char_boundary(next);
    }
    text.truncate(len);
    text.push_str(suffix);
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

fn message_content_mut(message: &mut Message) -> &mut Vec<ContentBlock> {
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
