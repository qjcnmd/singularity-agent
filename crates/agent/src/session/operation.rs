//! Sequential operation recovery for the current session format.
use std::collections::HashSet;

use super::format::{LedgerRecord, OperationKind, Result, SessionEntry, SessionError};
use crate::message::AgentMessage;

/// The single operation that may still be open after validating the whole ledger.
///
/// Open tools keep only their call ids in call order; names stay with the original
/// ToolCall record, which already owns them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationState {
    pub operation_id: String,
    pub kind: OperationKind,
    pub turn_id: Option<String>,
    pub open_tools: Vec<String>,
}

/// Validates the full ledger sequence and returns the operation that is still open,
/// if any. Invalid records are reported, not repaired by guessing.
pub fn reduce_operations(entries: &[SessionEntry]) -> Result<Option<OperationState>> {
    let mut active: Option<OperationState> = None;
    let mut seen = HashSet::new();
    for entry in entries {
        match entry {
            SessionEntry::Record {
                record:
                    LedgerRecord::OperationStarted {
                        operation_id,
                        kind,
                        turn_id,
                    },
                ..
            } => {
                if active.is_some() || !seen.insert(operation_id.clone()) {
                    return Err(SessionError::InvalidStructure(
                        "overlapping or duplicate operation".into(),
                    ));
                }
                active = Some(OperationState {
                    operation_id: operation_id.clone(),
                    kind: *kind,
                    turn_id: turn_id.clone(),
                    open_tools: Vec::new(),
                });
            }
            SessionEntry::Record {
                record:
                    LedgerRecord::OperationFinished {
                        operation_id,
                        turn_id,
                        ..
                    },
                ..
            } => {
                let Some(operation) = active.take() else {
                    return Err(SessionError::InvalidStructure(
                        "terminal does not match the active operation".into(),
                    ));
                };
                if operation.operation_id != *operation_id
                    || operation.turn_id.as_deref() != turn_id.as_deref()
                {
                    return Err(SessionError::InvalidStructure(
                        "terminal does not match the active operation".into(),
                    ));
                }
            }
            SessionEntry::Message { message, .. } => {
                let Some(operation) = active.as_mut() else {
                    continue;
                };
                if matches!(message, AgentMessage::Assistant { .. }) {
                    operation
                        .open_tools
                        .extend(message.tool_calls().map(|call| call.tool_call_id.clone()));
                } else if let Some(id) = message.tool_call_id() {
                    operation
                        .open_tools
                        .retain(|tool_call_id| tool_call_id != id);
                }
            }
            _ => {}
        }
    }
    Ok(active)
}
