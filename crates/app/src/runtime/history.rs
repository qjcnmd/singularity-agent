use agent::session::{LedgerRecord, SessionData, SessionEntry, SessionError, SessionMetadata};
use model::{ContentBlock, Message};
use protocol::{
    HistoryItem, SessionModelUsage, ThreadHistory, ThreadSummary, ThreadTurn, ToolStatus, TurnStatus,
};

struct IndexedTurn {
    turn_id: String,
    status: TurnStatus,
    error: Option<protocol::TurnErrorDetail>,
    manually_stopped: bool,
    entries: std::ops::Range<usize>,
}

impl IndexedTurn {
    fn project(
        &self,
        session: &SessionData,
        usage: &mut SessionModelUsage,
    ) -> Result<ThreadTurn, SessionError> {
        let mut items = Vec::new();
        let started_at = session.entries()[self.entries.start].timestamp().to_owned();
        let mut finished_at = None;
        let mut first_token_at = None;
        let mut tool_items = std::collections::HashMap::new();
        for entry in &session.entries()[self.entries.clone()] {
            if let Some(observation) = entry.request_observation() {
                accumulate_request_usage(usage, observation);
            }
            match entry {
                SessionEntry::Message {
                    message,
                    id,
                    observation,
                    display,
                    timestamp,
                    ..
                } => match message {
                    Message::Developer { .. } | Message::Notification { .. } => {}
                    Message::User { .. } | Message::Assistant { .. } => {
                        let mut calls = message.tool_calls();
                        if first_token_at.is_none()
                            && matches!(message, Message::Assistant { .. })
                            && let Some(decode_ms) =
                                observation.as_ref().and_then(|observation| observation.decode_ms)
                        {
                            first_token_at = app_core::iso_minus_millis(timestamp, decode_ms);
                        }
                        let status = match observation.as_ref().map(|observation| observation.status) {
                            Some(protocol::ProviderAttemptStatus::Error) => TurnStatus::Failed,
                            Some(protocol::ProviderAttemptStatus::Cancelled) => TurnStatus::Interrupted,
                            _ => TurnStatus::Completed,
                        };
                        let mut public = agent::message::public_items(
                            message,
                            display.as_ref(),
                            id,
                            status,
                            ToolStatus::Interrupted,
                            timestamp,
                        );
                        if observation.is_some() {
                            agent::message::attach_usage(&mut public, usage);
                        }
                        for item in public {
                            if matches!(item, HistoryItem::Tool { .. }) {
                                let call = calls.next().expect("public tools follow message order");
                                tool_items.insert(call.tool_call_id.clone(), items.len());
                            }
                            items.push(item);
                        }
                    }
                    Message::ToolResult { tool_call_id, .. } => {
                        let position = *tool_items.get(tool_call_id).ok_or_else(|| {
                            SessionError::InvalidSession(format!(
                                "tool result {id} refers to missing tool call {tool_call_id}"
                            ))
                        })?;
                        let HistoryItem::Tool { result, status, .. } = &mut items[position] else {
                            unreachable!("tool positions contain calls");
                        };
                        let displayed = agent::message::public_tool_result(message, display.as_ref())
                            .expect("matched a tool result message");
                        *status = if displayed.is_error {
                            ToolStatus::Failed
                        } else {
                            ToolStatus::Completed
                        };
                        *result = Some(displayed);
                    }
                },
                SessionEntry::Compaction { compaction, id, .. } => {
                    items.push(HistoryItem::Compaction {
                        id: id.clone(),
                        summary: compaction.summary.clone(),
                    });
                }
                SessionEntry::Metadata { .. } => {}
                SessionEntry::Record {
                    timestamp,
                    record: LedgerRecord::OperationFinished { turn_id: id, .. },
                    ..
                } if id == &self.turn_id => finished_at = Some(timestamp.clone()),
                SessionEntry::Record { .. } => {}
            }
        }
        Ok(ThreadTurn {
            started_at,
            finished_at,
            first_token_at,
            turn_id: self.turn_id.clone(),
            status: self.status,
            manually_stopped: self.manually_stopped,
            error: self.error.clone(),
            items,
            stats: Some(usage.clone()),
        })
    }
}

pub(crate) fn is_manually_stopped(outcome: TurnStatus, user_stopped: bool) -> bool {
    outcome == TurnStatus::Interrupted && user_stopped
}

fn index_turn_history(entries: &[SessionEntry]) -> Vec<IndexedTurn> {
    let mut turns: Vec<IndexedTurn> = Vec::new();
    for (position, entry) in entries.iter().enumerate() {
        if let SessionEntry::Record {
            id: turn_id,
            record: LedgerRecord::OperationStarted,
            ..
        } = entry
        {
            if let Some(last) = turns.last_mut() {
                last.entries.end = position;
            }
            turns.push(IndexedTurn {
                turn_id: turn_id.clone(),
                status: TurnStatus::Interrupted,
                error: None,
                manually_stopped: false,
                entries: position..entries.len(),
            });
            continue;
        }
        if let SessionEntry::Record {
            record:
                LedgerRecord::OperationFinished {
                    turn_id: id,
                    outcome,
                    error,
                    user_stopped,
                    ..
                },
            ..
        } = entry
            && let Some(last) = turns.last_mut()
            && &last.turn_id == id
        {
            last.status = *outcome;
            last.error.clone_from(error);
            last.manually_stopped = is_manually_stopped(*outcome, *user_stopped);
        }
    }
    turns
}

const MAX_SESSION_TITLE_CHARS: usize = 8;

pub(crate) fn context_usage(
    entries: &[SessionEntry],
    selected: &protocol::ModelSelection,
    capacity: Option<u64>,
) -> Option<protocol::ContextUsage> {
    let capacity = capacity.filter(|capacity| *capacity > 0)?;
    for entry in entries.iter().rev() {
        if matches!(
            entry,
            SessionEntry::Compaction { .. }
                | SessionEntry::Record {
                    record: LedgerRecord::ToolResultsCleared { .. },
                    ..
                }
        ) {
            return None;
        }
        let Some(request) = entry.request_observation() else {
            continue;
        };
        if request.purpose == protocol::RequestPurpose::Compaction {
            continue;
        }
        let Some(usage) = &request.usage else { continue };
        if request.provider != selected.provider || request.model != selected.model {
            return None;
        }
        return Some(protocol::ContextUsage { used: usage.input_tokens, capacity });
    }
    None
}

fn default_title(entry: &SessionEntry) -> Option<String> {
    let SessionEntry::Message {
        message: message @ Message::User { .. },
        display,
        ..
    } = entry
    else {
        return None;
    };
    let mut title = String::new();
    let mut remaining = MAX_SESSION_TITLE_CHARS;
    let words = message.content().iter().enumerate().filter_map(|(index, block)| match block {
        ContentBlock::Text { text, .. } => {
            Some(agent::message::user_text_for_display(display.as_ref(), index, text).split_whitespace())
        }
        ContentBlock::Thinking { .. } | ContentBlock::ToolCall(_) | ContentBlock::Image(_) => None,
    });
    for word in words.flatten() {
        if remaining == 0 {
            break;
        }
        if !title.is_empty() {
            if remaining == 1 {
                break;
            }
            title.push(' ');
            remaining -= 1;
        }
        for character in word.chars().take(remaining) {
            title.push(character);
            remaining -= 1;
        }
    }
    (!title.is_empty()).then_some(title)
}

pub(crate) fn session_usage(entries: &[SessionEntry]) -> SessionModelUsage {
    let mut usage = SessionModelUsage {
        cache_usage_complete: true,
        ..SessionModelUsage::default()
    };
    for entry in entries {
        if let Some(observation) = entry.request_observation() {
            accumulate_request_usage(&mut usage, observation);
        }
    }
    usage
}

pub(crate) fn accumulate_request_usage(
    usage: &mut SessionModelUsage,
    observation: &protocol::RequestObservation,
) {
    if let Some(ttft_ms) = observation.ttft_ms {
        usage.ttft_ms = usage.ttft_ms.saturating_add(ttft_ms);
        usage.ttft_requests = usage.ttft_requests.saturating_add(1);
    }
    let Some(request_usage) = &observation.usage else {
        return;
    };
    usage.input_tokens = usage.input_tokens.saturating_add(request_usage.input_tokens);
    usage.cached_input_tokens =
        usage.cached_input_tokens.saturating_add(request_usage.cached_input_tokens.unwrap_or(0));
    usage.total_tokens = usage.total_tokens.saturating_add(request_usage.total_tokens);
    usage.cache_usage_complete &= request_usage.cached_input_tokens.is_some();
    if let Some(ms) = observation.decode_ms
        && ms > 0
    {
        usage.decode_ms = usage.decode_ms.saturating_add(ms);
        usage.decode_tokens = usage.decode_tokens.saturating_add(request_usage.output_tokens);
    }
    usage.usage_present = true;
}

pub(crate) fn summarize_thread(session: &SessionData) -> ThreadSummary {
    let mut scan = ThreadSummaryScan::default();
    for entry in session.entries().iter().rev() {
        scan.update(entry, SummaryOrder::NewestFirst);
        if scan.title.is_some() && scan.updated_at.is_some() && scan.last_turn.is_some() {
            break;
        }
    }
    if scan.title.is_none() {
        scan.title = session.entries().iter().find_map(default_title);
    }
    scan.finish(session.session_id(), session.cwd_string(), session.created_at().to_owned())
}

#[derive(Default)]
pub(crate) struct ThreadSummaryScan {
    updated_at: Option<String>,
    title: Option<String>,
    last_turn: Option<(TurnStatus, bool)>,
}

#[derive(Clone, Copy)]
enum SummaryOrder {
    OldestFirst,
    NewestFirst,
}

impl ThreadSummaryScan {
    pub(crate) fn observe(&mut self, entry: &SessionEntry) {
        self.update(entry, SummaryOrder::OldestFirst);
        if self.title.is_none() {
            self.title = default_title(entry);
        }
    }

    fn update(&mut self, entry: &SessionEntry, order: SummaryOrder) {
        let should_replace = |has_value: bool| matches!(order, SummaryOrder::OldestFirst) || !has_value;
        if let SessionEntry::Metadata { metadata, .. } = entry {
            if let SessionMetadata::ThreadName { name } = metadata
                && should_replace(self.title.is_some())
            {
                name.clone_into(self.title.get_or_insert_with(String::new));
            }
            return;
        }
        if should_replace(self.updated_at.is_some()) {
            entry.timestamp().clone_into(self.updated_at.get_or_insert_with(String::new));
        }
        if !should_replace(self.last_turn.is_some()) {
            return;
        }
        match entry {
            SessionEntry::Record {
                record: LedgerRecord::OperationStarted, ..
            } => {
                self.last_turn = Some((TurnStatus::Interrupted, false));
            }
            SessionEntry::Record {
                record: LedgerRecord::OperationFinished { outcome, user_stopped, .. },
                ..
            } => {
                self.last_turn = Some((*outcome, is_manually_stopped(*outcome, *user_stopped)));
            }
            _ => {}
        }
    }

    pub(crate) fn finish(self, thread_id: &str, cwd: String, created_at: String) -> ThreadSummary {
        ThreadSummary {
            thread_id: thread_id.to_owned(),
            cwd,
            updated_at: self.updated_at.unwrap_or_else(|| created_at.clone()),
            created_at,
            title: self.title,
            status: self.last_turn.map(|(status, _)| status),
            manually_stopped: self.last_turn.is_some_and(|(_, manually_stopped)| manually_stopped),
        }
    }
}

pub(crate) fn read_thread_history(session: &SessionData) -> Result<ThreadHistory, SessionError> {
    let summary = summarize_thread(session);
    let indexed_turns = index_turn_history(session.entries());
    let mut usage = session_usage(
        &session.entries()
            [..indexed_turns.first().map_or(session.entries().len(), |turn| turn.entries.start)],
    );
    let turns = indexed_turns
        .iter()
        .map(|turn| turn.project(session, &mut usage))
        .collect::<Result<_, _>>()?;
    Ok(ThreadHistory { summary, turns })
}
