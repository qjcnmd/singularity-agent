//! Durable operation/control 归约与跨记录校验。
//!
//! Session ledger 的物理行是唯一事实源；本模块把它折叠为可恢复 operation、
//! pending write、工具调用和控制投影，并在读侧拒绝破坏 turn/step/tool/control
//! 因果关系的布局。恢复只消费这里确认过的事实，绝不从进程退出方式猜测副作用。

use std::collections::{HashMap, HashSet};

use super::format::{
    ControlChannel, ControlDisposition, LedgerRecord, OperationKind, PendingWriteKind, StepKind,
    ToolReplayClass, control_id,
};
use super::format::{Result, SessionEntry, SessionError};
use crate::message::{AgentMessageRole, ContentBlock};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingWrite {
    pub entry_id: String,
    pub kind: PendingWriteKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnresolvedTool {
    pub tool_call_id: String,
    pub tool_name: String,
    pub result_entry_id: Option<String>,
    pub replay: ToolReplayClass,
    pub started: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationState {
    pub operation_id: String,
    pub kind: OperationKind,
    pub turn_id: Option<String>,
    pub finished: Option<singularity_protocol::TurnStatus>,
    pub last_step: Option<(StepKind, u32, String)>,
    pub open_tools: Vec<UnresolvedTool>,
    pub pending_writes: Vec<PendingWrite>,
    attempts: HashMap<StepKind, u32>,
}

#[derive(Debug, Clone)]
struct StepFact {
    operation_id: String,
    step: StepKind,
    attempt: u32,
    result_entry_id: String,
    position: usize,
}

#[derive(Debug, Clone)]
struct ToolStartFact {
    operation_id: String,
    tool_call_id: String,
    tool_name: String,
    source_order: u32,
    result_entry_id: String,
    position: usize,
}

#[derive(Debug, Clone)]
struct AssistantToolFact {
    operation_id: Option<String>,
    entry_id: String,
    tool_name: String,
    source_order: u32,
    position: usize,
}

#[derive(Debug, Clone)]
struct DeferredFact {
    operation_id: String,
    entry_id: String,
    kind: PendingWriteKind,
}

pub fn reduce_operations(entries: &[SessionEntry]) -> Result<Vec<OperationState>> {
    let entry_positions = entries
        .iter()
        .enumerate()
        .map(|(index, entry)| (entry.id().to_string(), index))
        .collect::<HashMap<_, _>>();
    let mut order = Vec::new();
    let mut states = HashMap::<String, OperationState>::new();
    let mut turn_to_operation = HashMap::<String, String>::new();
    let mut operation_positions = HashMap::<String, usize>::new();
    let mut finish_positions = HashMap::<String, usize>::new();
    let mut step_facts = Vec::new();
    let mut step_by_result = HashMap::<String, (String, StepKind, u32, usize)>::new();
    let mut deferred = HashMap::<String, DeferredFact>::new();
    let mut resolved_deferred = HashSet::<String>::new();
    let mut tool_starts = Vec::new();
    let mut started_tool_ids = HashSet::<String>::new();
    let mut provider_attempts = Vec::<(String, u32, usize)>::new();

    for (position, entry) in entries.iter().enumerate() {
        let SessionEntry::Record { record, .. } = entry else {
            continue;
        };
        match record {
            LedgerRecord::OperationStarted {
                operation_id,
                kind,
                turn_id,
                intent,
            } => {
                if operation_id.trim().is_empty() {
                    return Err(corrupt("empty_operation", "operation id is empty"));
                }
                if states.contains_key(operation_id) {
                    return Err(corrupt(
                        "duplicate_operation",
                        format!("operation {operation_id} started twice"),
                    ));
                }
                match (kind, turn_id, intent) {
                    (
                        OperationKind::Run,
                        Some(turn),
                        super::format::OperationIntent::Run { .. },
                    ) if !turn.trim().is_empty() => {
                        if turn_to_operation
                            .insert(turn.clone(), operation_id.clone())
                            .is_some()
                        {
                            return Err(corrupt(
                                "duplicate_turn",
                                format!("turn {turn} is bound to more than one operation"),
                            ));
                        }
                    }
                    (
                        OperationKind::Compaction,
                        None,
                        super::format::OperationIntent::Compaction { .. },
                    ) => {}
                    (OperationKind::Run, _, _) => {
                        return Err(corrupt(
                            "run_missing_turn",
                            format!("run operation {operation_id} must carry a turn id"),
                        ));
                    }
                    (OperationKind::Compaction, _, _) => {
                        return Err(corrupt(
                            "compaction_turn_binding",
                            format!("compaction operation {operation_id} cannot carry a turn id"),
                        ));
                    }
                }
                order.push(operation_id.clone());
                operation_positions.insert(operation_id.clone(), position);
                states.insert(
                    operation_id.clone(),
                    OperationState {
                        operation_id: operation_id.clone(),
                        kind: *kind,
                        turn_id: turn_id.clone(),
                        finished: None,
                        last_step: None,
                        open_tools: Vec::new(),
                        pending_writes: Vec::new(),
                        attempts: HashMap::new(),
                    },
                );
            }
            LedgerRecord::ControlAccepted { .. } => {}
            other => {
                let operation_id = other.operation_id();
                let Some(state) = states.get_mut(operation_id) else {
                    return Err(corrupt(
                        "unknown_operation",
                        format!("record references unknown operation {operation_id}"),
                    ));
                };
                if state.finished.is_some() {
                    return Err(corrupt(
                        "record_after_finish",
                        format!("record after finished operation {operation_id}"),
                    ));
                }
                match other {
                    LedgerRecord::OperationFinished {
                        turn_id, outcome, ..
                    } => {
                        if *outcome == singularity_protocol::TurnStatus::Running {
                            return Err(corrupt(
                                "running_terminal",
                                format!("operation {operation_id} finished with running status"),
                            ));
                        }
                        if state.turn_id.as_deref() != turn_id.as_deref() {
                            return Err(corrupt(
                                "finish_turn_mismatch",
                                format!(
                                    "operation {operation_id} finished with a different turn id"
                                ),
                            ));
                        }
                        state.finished = Some(*outcome);
                        if *outcome == singularity_protocol::TurnStatus::Interrupted {
                            // An interrupted terminal is itself the durable
                            // classification for any result that was never safe
                            // to synthesize. Keep the deferred record in history,
                            // but do not expose it as an actionable pending write.
                            state.pending_writes.clear();
                        }
                        finish_positions.insert(operation_id.to_string(), position);
                    }
                    LedgerRecord::StepAttempt {
                        step,
                        attempt,
                        result_entry_id,
                        compaction_reason,
                        ..
                    } => {
                        if *attempt == 0 || result_entry_id.trim().is_empty() {
                            return Err(corrupt(
                                "invalid_step_attempt",
                                format!("operation {operation_id} has an invalid step attempt"),
                            ));
                        }
                        if (*step == StepKind::Compaction) != compaction_reason.is_some() {
                            return Err(corrupt(
                                "step_reason_mismatch",
                                format!(
                                    "operation {operation_id} step {step:?} has an invalid compaction reason"
                                ),
                            ));
                        }
                        let expected = state.attempts.get(step).copied().map_or(1, |n| n + 1);
                        if *attempt != expected {
                            return Err(corrupt(
                                "non_consecutive_attempt",
                                format!(
                                    "operation {operation_id} step {step:?} attempt {attempt} is not consecutive after {expected}"
                                ),
                            ));
                        }
                        if step_by_result
                            .insert(
                                result_entry_id.clone(),
                                (operation_id.to_string(), *step, *attempt, position),
                            )
                            .is_some()
                        {
                            return Err(corrupt(
                                "duplicate_step_result",
                                format!(
                                    "result entry {result_entry_id} is assigned to multiple steps"
                                ),
                            ));
                        }
                        state.attempts.insert(*step, *attempt);
                        state.last_step = Some((*step, *attempt, result_entry_id.clone()));
                        step_facts.push(StepFact {
                            operation_id: operation_id.to_string(),
                            step: *step,
                            attempt: *attempt,
                            result_entry_id: result_entry_id.clone(),
                            position,
                        });
                    }
                    LedgerRecord::ProviderAttempt {
                        attempt,
                        provider,
                        model,
                        protocol,
                        status,
                        retry_after_ms,
                        retry_after_source,
                        ..
                    } => {
                        if *attempt == 0
                            || provider.trim().is_empty()
                            || model.trim().is_empty()
                            || protocol.trim().is_empty()
                            || *status == singularity_protocol::ProviderAttemptStatus::Started
                        {
                            return Err(corrupt(
                                "invalid_provider_attempt",
                                format!(
                                    "operation {operation_id} has invalid provider attempt metadata"
                                ),
                            ));
                        }
                        if retry_after_ms.is_some() != retry_after_source.is_some() {
                            return Err(corrupt(
                                "retry_after_provenance_mismatch",
                                format!(
                                    "operation {operation_id} has incomplete Retry-After provenance"
                                ),
                            ));
                        }
                        provider_attempts.push((operation_id.to_string(), *attempt, position));
                    }
                    LedgerRecord::WriteDeferred { entry_id, kind, .. } => {
                        if entry_id.trim().is_empty() {
                            return Err(corrupt(
                                "empty_deferred_entry",
                                "deferred entry id is empty",
                            ));
                        }
                        if deferred
                            .insert(
                                entry_id.clone(),
                                DeferredFact {
                                    operation_id: operation_id.to_string(),
                                    entry_id: entry_id.clone(),
                                    kind: *kind,
                                },
                            )
                            .is_some()
                        {
                            return Err(corrupt(
                                "duplicate_deferred_write",
                                format!("entry {entry_id} was deferred more than once"),
                            ));
                        }
                        if let Some(target_position) = entry_positions.get(entry_id) {
                            if *target_position <= position {
                                return Err(corrupt(
                                    "deferred_target_precedes_declaration",
                                    format!("deferred target {entry_id} precedes its declaration"),
                                ));
                            }
                        } else {
                            state.pending_writes.push(PendingWrite {
                                entry_id: entry_id.clone(),
                                kind: *kind,
                            });
                        }
                    }
                    LedgerRecord::WriteAbandoned {
                        entry_id,
                        kind,
                        reason,
                        ..
                    } => {
                        if reason.trim().is_empty() {
                            return Err(corrupt(
                                "empty_write_abandon_reason",
                                format!("deferred entry {entry_id} has no recovery reason"),
                            ));
                        }
                        let Some(fact) = deferred.get(entry_id) else {
                            return Err(corrupt(
                                "write_abandon_without_defer",
                                format!("entry {entry_id} was abandoned without a deferred write"),
                            ));
                        };
                        if fact.operation_id != operation_id || fact.kind != *kind {
                            return Err(corrupt(
                                "write_abandon_identity_mismatch",
                                format!(
                                    "entry {entry_id} was abandoned by another operation or kind"
                                ),
                            ));
                        }
                        if entry_positions.contains_key(entry_id)
                            || !resolved_deferred.insert(entry_id.clone())
                        {
                            return Err(corrupt(
                                "write_abandon_after_target",
                                format!("deferred entry {entry_id} was already resolved"),
                            ));
                        }
                        state
                            .pending_writes
                            .retain(|pending| pending.entry_id != *entry_id);
                    }
                    LedgerRecord::ToolStarted {
                        tool_call_id,
                        tool_name,
                        source_order,
                        result_entry_id,
                        ..
                    } => {
                        if tool_call_id.trim().is_empty()
                            || tool_name.trim().is_empty()
                            || result_entry_id.trim().is_empty()
                        {
                            return Err(corrupt(
                                "invalid_tool_start",
                                format!(
                                    "operation {operation_id} has incomplete tool_started identity"
                                ),
                            ));
                        }
                        if !started_tool_ids.insert(tool_call_id.clone()) {
                            return Err(corrupt(
                                "duplicate_tool_invocation",
                                format!("tool call {tool_call_id} started twice"),
                            ));
                        }
                        if let Some(target_position) = entry_positions.get(result_entry_id)
                            && *target_position <= position
                        {
                            return Err(corrupt(
                                "tool_result_precedes_start",
                                format!(
                                    "tool result {result_entry_id} precedes tool {tool_call_id}"
                                ),
                            ));
                        }
                        tool_starts.push(ToolStartFact {
                            operation_id: operation_id.to_string(),
                            tool_call_id: tool_call_id.clone(),
                            tool_name: tool_name.clone(),
                            source_order: *source_order,
                            result_entry_id: result_entry_id.clone(),
                            position,
                        });
                    }
                    LedgerRecord::OperationStarted { .. }
                    | LedgerRecord::ControlAccepted { .. } => unreachable!("handled above"),
                }
            }
        }
    }

    for step in &step_facts {
        validate_step_target(step, &entry_positions, entries, &deferred)?;
        if let Some(finish) = finish_positions.get(&step.operation_id) {
            match entry_positions.get(&step.result_entry_id) {
                Some(target) if target < finish => {}
                Some(_) => {
                    return Err(corrupt(
                        "result_after_finish",
                        format!(
                            "step result {} is after operation finish",
                            step.result_entry_id
                        ),
                    ));
                }
                None if states.get(&step.operation_id).is_some_and(|state| {
                    state.finished == Some(singularity_protocol::TurnStatus::Interrupted)
                }) => {}
                None => {
                    return Err(corrupt(
                        "finish_with_pending_write",
                        format!(
                            "operation {} finished with pending step result {}",
                            step.operation_id, step.result_entry_id
                        ),
                    ));
                }
            }
        }
    }
    for fact in deferred.values() {
        let referenced = match fact.kind {
            PendingWriteKind::AssistantMessage | PendingWriteKind::Compaction => {
                step_facts.iter().any(|step| {
                    step.operation_id == fact.operation_id && step.result_entry_id == fact.entry_id
                })
            }
            PendingWriteKind::ToolResult => tool_starts.iter().any(|tool| {
                tool.operation_id == fact.operation_id && tool.result_entry_id == fact.entry_id
            }),
        };
        if !referenced {
            return Err(corrupt(
                "unowned_deferred_write",
                format!(
                    "deferred entry {} has no matching operation step",
                    fact.entry_id
                ),
            ));
        }
        if let Some(target) = entry_positions.get(&fact.entry_id) {
            validate_pending_target(fact.kind, &entries[*target])?;
            if let Some(finish) = finish_positions.get(&fact.operation_id)
                && target >= finish
            {
                return Err(corrupt(
                    "result_after_finish",
                    format!(
                        "deferred target {} is after operation finish",
                        fact.entry_id
                    ),
                ));
            }
        } else if resolved_deferred.contains(&fact.entry_id) {
            continue;
        } else if finish_positions.contains_key(&fact.operation_id)
            && states.get(&fact.operation_id).is_none_or(|state| {
                state.finished != Some(singularity_protocol::TurnStatus::Interrupted)
            })
        {
            return Err(corrupt(
                "finish_with_pending_write",
                format!(
                    "operation {} finished with pending write {}",
                    fact.operation_id, fact.entry_id
                ),
            ));
        }
    }

    let mut assistant_calls = HashMap::<String, AssistantToolFact>::new();
    for (position, entry) in entries.iter().enumerate() {
        let SessionEntry::Message { id, message, .. } = entry else {
            continue;
        };
        if message.role() != AgentMessageRole::Assistant {
            continue;
        }
        let operation_id = step_by_result
            .get(id)
            .and_then(|(operation, step, _, _)| {
                (*step == StepKind::Assistant).then_some(operation.clone())
            })
            .or_else(|| {
                operation_positions
                    .iter()
                    .filter(|(operation, start)| {
                        **start < position
                            && finish_positions
                                .get(*operation)
                                .is_none_or(|finish| position < *finish)
                    })
                    .max_by_key(|(_, start)| **start)
                    .map(|(operation, _)| operation.clone())
            });
        for (source_order, block) in message.tool_calls().enumerate() {
            let ContentBlock::ToolCall {
                id: tool_call_id,
                name,
                ..
            } = block
            else {
                continue;
            };
            if tool_call_id.trim().is_empty() || name.trim().is_empty() {
                return Err(corrupt(
                    "invalid_assistant_tool_call",
                    format!("assistant entry {id} has incomplete tool call"),
                ));
            }
            if assistant_calls
                .insert(
                    tool_call_id.clone(),
                    AssistantToolFact {
                        operation_id: operation_id.clone(),
                        entry_id: tool_call_id.clone(),
                        tool_name: name.clone(),
                        source_order: source_order as u32,
                        position,
                    },
                )
                .is_some()
            {
                return Err(corrupt(
                    "duplicate_tool_call_id",
                    format!("tool call {tool_call_id} is declared twice"),
                ));
            }
        }
    }

    for tool in &tool_starts {
        let Some(call) = assistant_calls.get(&tool.tool_call_id) else {
            // A crash can leave a tool_started record after the assistant entry was
            // compacted or never flushed. The side effect is still unknown and is
            // therefore recoverable as an unresolved tool, never replayed.
            continue;
        };
        if call.operation_id.as_deref() != Some(tool.operation_id.as_str())
            || call.tool_name != tool.tool_name
            || call.source_order != tool.source_order
            || call.position >= tool.position
        {
            return Err(corrupt(
                "tool_identity_mismatch",
                format!(
                    "tool {} does not match its assistant source call",
                    tool.tool_call_id
                ),
            ));
        }
        if let Some(target) = entry_positions.get(&tool.result_entry_id) {
            validate_pending_target(PendingWriteKind::ToolResult, &entries[*target])?;
            let SessionEntry::Message { message, .. } = &entries[*target] else {
                unreachable!()
            };
            if message.tool_call_id().map(String::as_str) != Some(tool.tool_call_id.as_str())
                || message.tool_name().map(String::as_str) != Some(tool.tool_name.as_str())
            {
                return Err(corrupt(
                    "tool_result_identity_mismatch",
                    format!(
                        "result {} does not match tool {}",
                        tool.result_entry_id, tool.tool_call_id
                    ),
                ));
            }
        } else if let Some(fact) = deferred.get(&tool.result_entry_id)
            && (fact.operation_id != tool.operation_id || fact.kind != PendingWriteKind::ToolResult)
        {
            return Err(corrupt(
                "tool_result_deferred_mismatch",
                format!(
                    "tool {} has a deferred result owned by another operation",
                    tool.tool_call_id
                ),
            ));
        }
    }

    let mut result_ids = HashSet::<String>::new();
    for (position, entry) in entries.iter().enumerate() {
        let SessionEntry::Message { message, .. } = entry else {
            continue;
        };
        if message.role() != AgentMessageRole::ToolResult {
            continue;
        }
        let Some(tool_call_id) = message.tool_call_id() else {
            return Err(corrupt(
                "tool_result_missing_call_id",
                "tool result has no tool call id",
            ));
        };
        let Some(tool_name) = message.tool_name() else {
            return Err(corrupt(
                "tool_result_missing_name",
                format!("tool result {tool_call_id} has no tool name"),
            ));
        };
        if !result_ids.insert(tool_call_id.clone()) {
            return Err(corrupt(
                "duplicate_tool_result",
                format!("tool result {tool_call_id} appears twice"),
            ));
        }
        if let Some(tool) = tool_starts
            .iter()
            .find(|tool| tool.tool_call_id == *tool_call_id)
        {
            if tool.tool_name != *tool_name || tool.position >= position {
                return Err(corrupt(
                    "tool_result_identity_mismatch",
                    format!("tool result {tool_call_id} does not match its start"),
                ));
            }
        } else if let Some(call) = assistant_calls.get(tool_call_id) {
            if call.tool_name != *tool_name || call.position >= position {
                return Err(corrupt(
                    "synthetic_tool_result_mismatch",
                    format!("synthetic tool result {tool_call_id} has no matching assistant call"),
                ));
            }
        } else {
            return Err(corrupt(
                "orphan_tool_result",
                format!("tool result {tool_call_id} has no assistant call"),
            ));
        }
    }

    for tool in &tool_starts {
        if result_ids.contains(&tool.tool_call_id) {
            continue;
        }
        let Some(state) = states.get_mut(&tool.operation_id) else {
            return Err(corrupt(
                "unknown_operation",
                format!("tool {} references an unknown operation", tool.tool_call_id),
            ));
        };
        if state.finished.is_some() {
            return Err(corrupt(
                "finish_with_open_tool",
                format!(
                    "operation {} finished with unresolved tool {}",
                    tool.operation_id, tool.tool_call_id
                ),
            ));
        }
        state.open_tools.push(UnresolvedTool {
            tool_call_id: tool.tool_call_id.clone(),
            tool_name: tool.tool_name.clone(),
            result_entry_id: Some(tool.result_entry_id.clone()),
            replay: ToolReplayClass::Never,
            started: true,
        });
    }

    for (operation_id, attempt, position) in provider_attempts {
        if !step_facts.iter().any(|step| {
            step.operation_id == operation_id
                && step.step == StepKind::Assistant
                && step.attempt == attempt
                && step.position < position
        }) {
            return Err(corrupt(
                "provider_without_step",
                format!(
                    "provider attempt {operation_id}/{attempt} has no preceding assistant step"
                ),
            ));
        }
    }

    for call in assistant_calls.values() {
        let Some(operation_id) = call.operation_id.as_ref() else {
            continue;
        };
        let started = tool_starts
            .iter()
            .any(|tool| tool.tool_call_id == call.entry_id);
        let result = result_ids.contains(&call.entry_id);
        if started || result {
            continue;
        }
        let Some(state) = states.get_mut(operation_id) else {
            continue;
        };
        if state.finished.is_some() {
            return Err(corrupt(
                "finish_with_open_tool",
                format!(
                    "operation {operation_id} finished with unresolved tool {}",
                    call.entry_id
                ),
            ));
        }
        state.open_tools.push(UnresolvedTool {
            tool_call_id: call.entry_id.clone(),
            tool_name: call.tool_name.clone(),
            result_entry_id: None,
            replay: ToolReplayClass::Safe,
            started: false,
        });
    }

    Ok(order
        .into_iter()
        .filter_map(|id| states.remove(&id))
        .collect())
}

fn validate_step_target(
    step: &StepFact,
    positions: &HashMap<String, usize>,
    entries: &[SessionEntry],
    deferred: &HashMap<String, DeferredFact>,
) -> Result<()> {
    let expected = match step.step {
        StepKind::Assistant => PendingWriteKind::AssistantMessage,
        StepKind::Compaction => PendingWriteKind::Compaction,
    };
    match positions.get(&step.result_entry_id) {
        Some(target) if *target > step.position => {
            validate_pending_target(expected, &entries[*target])
        }
        Some(_) => Err(corrupt(
            "step_result_precedes_attempt",
            format!("step result {} precedes its attempt", step.result_entry_id),
        )),
        None if deferred.get(&step.result_entry_id).is_some_and(|fact| {
            fact.operation_id == step.operation_id && fact.kind == expected
        }) =>
        {
            Ok(())
        }
        None => Err(corrupt(
            "step_result_missing",
            format!(
                "step {} has no result or deferred write",
                step.result_entry_id
            ),
        )),
    }
}

fn validate_pending_target(kind: PendingWriteKind, entry: &SessionEntry) -> Result<()> {
    let valid = match kind {
        PendingWriteKind::AssistantMessage => {
            matches!(entry, SessionEntry::Message { message, .. } if message.role() == AgentMessageRole::Assistant)
        }
        PendingWriteKind::Compaction => matches!(entry, SessionEntry::Compaction { .. }),
        PendingWriteKind::ToolResult => {
            matches!(entry, SessionEntry::Message { message, .. } if message.role() == AgentMessageRole::ToolResult)
        }
    };
    if valid {
        Ok(())
    } else {
        Err(corrupt(
            "deferred_target_kind_mismatch",
            format!("entry {} does not match deferred kind {kind:?}", entry.id()),
        ))
    }
}

pub fn validate_ledger(entries: &[SessionEntry]) -> Result<()> {
    reduce_operations(entries)?;
    reduce_controls(entries)?;
    Ok(())
}

pub fn open_operations(operations: &[OperationState]) -> Vec<&OperationState> {
    operations
        .iter()
        .filter(|operation| operation.finished.is_none())
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReducedControl {
    pub control_id: String,
    pub turn_id: String,
    pub channel: ControlChannel,
    pub sequence: u64,
    pub text: Option<String>,
    pub disposition: ControlDisposition,
}

pub fn reduce_controls(entries: &[SessionEntry]) -> Result<Vec<ReducedControl>> {
    let mut by_id = HashMap::<String, ReducedControl>::new();
    let mut by_sequence = HashMap::<u64, String>::new();
    for entry in entries {
        let SessionEntry::Record {
            record:
                LedgerRecord::ControlAccepted {
                    control_id: id,
                    turn_id,
                    channel,
                    sequence,
                    disposition,
                    text,
                },
            ..
        } = entry
        else {
            continue;
        };
        if id.trim().is_empty()
            || turn_id.trim().is_empty()
            || *id != control_id(turn_id, *channel, *sequence)
        {
            return Err(corrupt(
                "control_identity_invalid",
                format!("control {id} does not match turn/channel/sequence"),
            ));
        }
        if let Some(existing) = by_sequence.insert(*sequence, id.clone())
            && existing != *id
        {
            return Err(corrupt(
                "control_sequence_collision",
                format!("sequence {sequence} is used by {existing} and {id}"),
            ));
        }
        validate_control_payload(*channel, *disposition, text.as_deref())?;
        let Some(existing) = by_id.get_mut(id) else {
            if *disposition != ControlDisposition::Pending {
                return Err(corrupt(
                    "control_without_acceptance",
                    format!("control {id} has a terminal disposition without acceptance"),
                ));
            }
            by_id.insert(
                id.clone(),
                ReducedControl {
                    control_id: id.clone(),
                    turn_id: turn_id.clone(),
                    channel: *channel,
                    sequence: *sequence,
                    text: text.clone(),
                    disposition: *disposition,
                },
            );
            continue;
        };
        if existing.turn_id != *turn_id
            || existing.channel != *channel
            || existing.sequence != *sequence
        {
            return Err(corrupt(
                "control_identity_drift",
                format!("control {id} changed identity between records"),
            ));
        }
        if existing.disposition != ControlDisposition::Pending {
            return Err(corrupt(
                "control_repeated_terminal",
                format!("control {id} reached a terminal disposition twice"),
            ));
        }
        if *disposition == ControlDisposition::Pending {
            return Err(corrupt(
                "control_repeated_acceptance",
                format!("control {id} was accepted twice"),
            ));
        }
        if let Some(terminal_text) = text
            && Some(terminal_text) != existing.text.as_ref()
        {
            return Err(corrupt(
                "control_payload_drift",
                format!("control {id} changed payload at disposition"),
            ));
        }
        existing.disposition = *disposition;
    }
    let mut controls = by_id.into_values().collect::<Vec<_>>();
    controls.sort_by_key(|control| control.sequence);
    Ok(controls)
}

fn validate_control_payload(
    channel: ControlChannel,
    disposition: ControlDisposition,
    text: Option<&str>,
) -> Result<()> {
    if disposition == ControlDisposition::Pending
        && matches!(channel, ControlChannel::Steer | ControlChannel::FollowUp)
        && text.is_none_or(|value| value.trim().is_empty())
    {
        return Err(corrupt(
            "control_payload_missing",
            format!("{channel:?} pending control requires text"),
        ));
    }
    if channel == ControlChannel::Cancel && text.is_some() {
        return Err(corrupt(
            "cancel_payload_present",
            "cancel control must not carry text",
        ));
    }
    let legal = match disposition {
        ControlDisposition::Pending => true,
        ControlDisposition::Injected => channel == ControlChannel::Steer,
        ControlDisposition::StartedAsNewTurn => {
            matches!(channel, ControlChannel::Steer | ControlChannel::FollowUp)
        }
        ControlDisposition::Cancelled => true,
    };
    if legal {
        Ok(())
    } else {
        Err(corrupt(
            "control_disposition_illegal",
            format!("{channel:?} cannot reach {disposition:?}"),
        ))
    }
}

fn corrupt(reason: &str, detail: impl Into<String>) -> SessionError {
    SessionError::LedgerCorrupt {
        reason: reason.to_string(),
        detail: detail.into(),
    }
}
