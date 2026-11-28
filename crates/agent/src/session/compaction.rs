use std::collections::{HashMap, HashSet};

use app_core::estimate_tokens_of;
use model::{ContentBlock, Message, ModelConfigurationSnapshot};

use super::context::{
    ContextEntry, cleared_tool_result_ids, context_entry, context_token_estimate_for,
    frame_compacted_summary, message_content_mut, message_token_estimate, message_with_captions_tokens,
    project_messages, resolve_context_entries,
};
use super::format::SessionEntry;
use super::manager::SessionData;

pub(crate) fn retention_budget_tokens(model: ModelConfigurationSnapshot) -> u64 {
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
    target: (&str, &str),
    cleared: &HashSet<&str>,
) -> Option<(usize, String)> {
    let units = balanced_units(active);
    if units.is_empty() {
        return None;
    }

    let mut accumulated = 0u64;
    let mut keep_unit = units.len();
    for (idx, range) in units.iter().enumerate().rev() {
        let unit_tokens = context_token_estimate_for(&active[range.clone()], session, Some(target), cleared);
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

fn compactable_prefix_split(
    active: &[&SessionEntry],
    data: &SessionData,
    retain_tokens: u64,
    identity: (&str, &str),
    cleared: &HashSet<&str>,
) -> Option<(usize, String, u64)> {
    let (split_idx, first_kept_entry_id) =
        select_compactable_split(active, data, retain_tokens, identity, cleared)?;
    let raw_prefix_tokens = context_token_estimate_for(&active[..split_idx], data, Some(identity), cleared);
    if raw_prefix_tokens <= message_token_estimate(&frame_compacted_summary("")) {
        return None;
    }
    Some((split_idx, first_kept_entry_id, raw_prefix_tokens))
}

pub(crate) fn has_compactable_prefix(data: &SessionData, retain_tokens: u64, identity: (&str, &str)) -> bool {
    let active = resolve_context_entries(data);
    let cleared = cleared_tool_result_ids(data);
    compactable_prefix_split(&active, data, retain_tokens, identity, &cleared).is_some()
}

#[derive(Debug)]
pub(crate) struct PlannedCompactionStep {
    pub(crate) session_id: String,
    pub(crate) messages: Vec<Message>,
    pub(crate) first_kept_entry_id: String,
    pub(crate) raw_prefix_tokens: u64,
    pub(crate) estimated_request_input: u64,
    pub(crate) image_directory: std::path::PathBuf,
}

fn compaction_input_limit(
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
        Some(overflow) => {
            let headroom = overflow - min_req;
            let shrink = min_out.max(overflow / 5).min(headroom / 2).max(1);
            window_limit.min(overflow - shrink)
        }
        None => window_limit,
    };
    (limit > min_req).then_some(limit)
}

pub(crate) fn plan_compaction_step(
    data: &SessionData,
    retain_tokens: u64,
    identity: (&str, &str),
    definition_tokens: u64,
    instruction_tokens: u64,
    model: ModelConfigurationSnapshot,
    overflow_input_tokens: Option<u64>,
) -> Result<Option<PlannedCompactionStep>, &'static str> {
    const NO_HEADROOM: &str = "当前指令与工具定义已占满模型上下文窗口，无法留出压缩摘要空间。";
    let active = resolve_context_entries(data);
    let cleared = cleared_tool_result_ids(data);
    let Some((split_idx, first_kept_entry_id, raw_prefix_tokens)) =
        compactable_prefix_split(&active, data, retain_tokens, identity, &cleared)
    else {
        return Ok(None);
    };
    let empty_summary = message_token_estimate(&frame_compacted_summary(""));
    let limit = compaction_input_limit(model, definition_tokens, instruction_tokens, overflow_input_tokens)
        .ok_or(NO_HEADROOM)?;
    let max_prefix = limit.saturating_sub(definition_tokens).saturating_sub(instruction_tokens);
    if max_prefix <= empty_summary {
        return Err(NO_HEADROOM);
    }

    let image_directory = data.image_directory();
    let mut messages = project_messages(&active[..split_idx], &cleared);
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
    Ok(Some(PlannedCompactionStep {
        session_id: data.session_id().to_owned(),
        messages,
        first_kept_entry_id,
        raw_prefix_tokens,
        estimated_request_input: definition_tokens
            .saturating_add(projected)
            .saturating_add(instruction_tokens),
        image_directory,
    }))
}

fn fold_oversized_messages(messages: &mut [Message], max_tokens: u64) -> u64 {
    let sum = |ms: &[Message]| -> u64 { ms.iter().map(message_token_estimate).sum() };
    let mut is_tool_output = Vec::with_capacity(messages.len());
    let mut preserved_headers: Vec<Vec<bool>> = Vec::with_capacity(messages.len());
    let mut call_tags: HashMap<String, (usize, String)> = HashMap::new();
    let mut next_tag = 1usize;

    for message in messages.iter_mut() {
        let tool_meta = match message {
            Message::ToolResult { tool_call_id, is_error, .. } => Some((tool_call_id.clone(), *is_error)),
            _ => None,
        };
        is_tool_output.push(tool_meta.is_some());
        if let Message::Assistant { provider_metadata, .. } = message {
            *provider_metadata = None;
        }
        let mut rewritten = Vec::new();
        let mut flags = Vec::new();
        if let Some((call_id, is_error)) = &tool_meta {
            let err = if *is_error { " (error)" } else { "" };
            let header = match call_tags.get(call_id.as_str()) {
                Some((tag, name)) => format!("[Tool result #{tag}: {name}{err}]"),
                None => format!("[Tool result ({call_id}){err}]"),
            };
            rewritten.push(ContentBlock::text(header));
            flags.push(true);
        }
        for block in message_content_mut(message).drain(..) {
            match block {
                ContentBlock::Thinking { .. } => {}
                ContentBlock::Image(img) => {
                    rewritten.push(ContentBlock::text(format!(
                        "[Image omitted for compaction: {}; {} × {}]",
                        img.name, img.width, img.height
                    )));
                    flags.push(false);
                }
                ContentBlock::ToolCall(call) => {
                    let tag = next_tag;
                    next_tag += 1;
                    call_tags.insert(call.tool_call_id.clone(), (tag, call.tool_name.clone()));
                    rewritten.push(ContentBlock::text(format!("[Tool call #{tag}: {}]", call.tool_name)));
                    flags.push(true);
                    rewritten.push(ContentBlock::text(call.arguments.to_string()));
                    flags.push(false);
                }
                other @ ContentBlock::Text { .. } => {
                    rewritten.push(other);
                    flags.push(false);
                }
            }
        }
        preserved_headers.push(flags);
        if tool_meta.is_some() {
            *message = Message::User { content: rewritten };
        } else {
            *message_content_mut(message) = rewritten;
        }
    }

    for tool_only in [true, false] {
        let current = sum(messages);
        if current <= max_tokens {
            return current;
        }
        let texts: Vec<&mut String> = messages
            .iter_mut()
            .zip(is_tool_output.iter().copied())
            .zip(preserved_headers.iter())
            .filter(|((_, is_tool), _)| !tool_only || *is_tool)
            .flat_map(|((m, _), flags)| {
                message_content_mut(m).iter_mut().zip(flags.iter().copied()).filter_map(|(b, is_header)| {
                    match b {
                        ContentBlock::Text { text, .. } if !is_header => Some(text),
                        _ => None,
                    }
                })
            })
            .collect();
        let total_text: u64 = texts.iter().map(|t| estimate_tokens_of(t)).sum();
        if total_text == 0 {
            continue;
        }
        let keep = total_text.saturating_sub(current - max_tokens);
        for text in texts {
            let tokens = estimate_tokens_of(text);
            let cap = tokens.saturating_mul(keep) / total_text;
            if tokens > cap {
                trim_text_to_tokens(text, cap);
            }
        }
    }
    sum(messages)
}

fn trim_text_to_tokens(text: &mut String, max_tokens: u64) {
    const MARKER: &str = "\n...[truncated for compaction]...\n";
    let marker_tokens = estimate_tokens_of(MARKER);
    if max_tokens <= marker_tokens + 2 {
        let head_len = take_prefix_for_tokens(text, max_tokens).len();
        text.truncate(head_len);
        return;
    }
    let budget = max_tokens - marker_tokens;
    let head_budget = budget / 3;
    let tail_budget = budget - head_budget;
    let head = take_prefix_for_tokens(text, head_budget);
    let tail = take_suffix_for_tokens(text, tail_budget);
    if head.len() + tail.len() >= text.len() {
        return;
    }
    *text = format!("{head}{MARKER}{tail}");
}

fn take_prefix_for_tokens(text: &str, max_tokens: u64) -> &str {
    let limit = max_tokens.saturating_mul(4);
    let (mut quarters, mut end) = (0u64, 0usize);
    for (idx, ch) in text.char_indices() {
        let cost = if ch.is_ascii() { 1 } else { 4 };
        if quarters + cost > limit {
            break;
        }
        quarters += cost;
        end = idx + ch.len_utf8();
    }
    &text[..end]
}

fn take_suffix_for_tokens(text: &str, max_tokens: u64) -> &str {
    let limit = max_tokens.saturating_mul(4);
    let (mut quarters, mut start) = (0u64, text.len());
    for (idx, ch) in text.char_indices().rev() {
        let cost = if ch.is_ascii() { 1 } else { 4 };
        if quarters + cost > limit {
            break;
        }
        quarters += cost;
        start = idx;
    }
    &text[start..]
}
