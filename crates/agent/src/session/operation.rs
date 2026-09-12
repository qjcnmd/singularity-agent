//! Sequential operation recovery for the current session format.
use super::format::{LedgerRecord, OperationKind, Result, SessionEntry, SessionError};
use crate::message::{AgentMessage, ContentBlock};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnresolvedTool {
    pub tool_call_id: String,
    pub tool_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationState {
    pub operation_id: String,
    pub kind: OperationKind,
    pub turn_id: Option<String>,
    pub finished: Option<singularity_protocol::TurnStatus>,
    pub open_tools: Vec<UnresolvedTool>,
}

/// Only one operation can be active in a session. Invalid records are reported, not repaired by guessing.
pub fn reduce_operations(entries: &[SessionEntry]) -> Result<Vec<OperationState>> {
    let mut operations: Vec<OperationState> = Vec::new();
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
                if operations.last().is_some_and(|op| op.finished.is_none())
                    || operations.iter().any(|op| op.operation_id == *operation_id)
                {
                    return Err(SessionError::InvalidStructure(
                        "overlapping or duplicate operation".into(),
                    ));
                }
                operations.push(OperationState {
                    operation_id: operation_id.clone(),
                    kind: *kind,
                    turn_id: turn_id.clone(),
                    finished: None,
                    open_tools: Vec::new(),
                });
            }
            SessionEntry::Record {
                record:
                    LedgerRecord::OperationFinished {
                        operation_id,
                        turn_id,
                        outcome,
                        ..
                    },
                ..
            } => {
                let op = operations
                    .last_mut()
                    .filter(|op| {
                        op.operation_id == *operation_id
                            && op.turn_id == *turn_id
                            && op.finished.is_none()
                    })
                    .ok_or_else(|| {
                        SessionError::InvalidStructure(
                            "terminal does not match the active operation".into(),
                        )
                    })?;
                op.finished = Some(*outcome);
                op.open_tools.clear();
            }
            SessionEntry::Message { message, .. } => {
                let Some(op) = operations.last_mut().filter(|op| op.finished.is_none()) else {
                    continue;
                };
                if matches!(message, AgentMessage::Assistant { .. }) {
                    for call in message.tool_calls() {
                        if let ContentBlock::ToolCall { id, name, .. } = call {
                            op.open_tools.push(UnresolvedTool {
                                tool_call_id: id.clone(),
                                tool_name: name.clone(),
                            });
                        }
                    }
                } else if let Some(id) = message.tool_call_id() {
                    op.open_tools.retain(|tool| tool.tool_call_id != *id);
                }
            }
            _ => {}
        }
    }
    Ok(operations)
}

pub fn open_operations(operations: &[OperationState]) -> Vec<&OperationState> {
    operations
        .iter()
        .filter(|op| op.finished.is_none())
        .collect()
}
