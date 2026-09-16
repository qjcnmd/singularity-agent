//! 当前会话格式的 operation 顺序恢复。
use std::collections::HashSet;

use super::format::{LedgerRecord, OperationKind, Result, SessionEntry, SessionError};
use crate::message::AgentMessage;

/// 校验完整 ledger 后仍可能处于 open 状态的那一个 operation。
///
/// open 工具只按调用顺序保留 call id；名称保留在原始
/// ToolCall 记录中，该记录本就拥有它们。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationState {
    pub operation_id: String,
    pub kind: OperationKind,
    pub turn_id: Option<String>,
    pub open_tools: Vec<String>,
}

/// 校验完整 ledger 序列并返回仍处于 open 的 operation（若有）。
/// 非法记录只上报，不靠猜测修复。
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
                        .retain(|tool_call_id| tool_call_id.as_str() != id);
                }
            }
            _ => {}
        }
    }
    Ok(active)
}
