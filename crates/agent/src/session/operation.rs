//! 当前会话格式下 operation 的顺序恢复。
use std::collections::HashSet;

use super::format::{LedgerRecord, OperationKind, Result, SessionEntry, SessionError};
use crate::message::AgentMessage;

/// 校验完整 ledger 之后，仍可能处于 open 状态的那个 operation。
///
/// open 工具只按调用顺序保留 call id；名称留在原始 ToolCall 记录里。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationState {
    pub operation_id: String,
    pub kind: OperationKind,
    pub turn_id: Option<String>,
    pub open_tools: Vec<String>,
}

/// 校验完整的 ledger 序列，返回仍处于 open 的 operation（如果有）。
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
                // 终态必须对上当前 operation 的存在性与两个身份；缺失和不匹配走同一个失败出口。
                let Some(operation) = active.take().filter(|operation| {
                    operation.operation_id == *operation_id
                        && operation.turn_id.as_deref() == turn_id.as_deref()
                }) else {
                    return Err(SessionError::InvalidStructure(
                        "terminal does not match the active operation".into(),
                    ));
                };
                // 可信的终态必须已经闭合全部工具调用：还留着未配对调用的终结记录
                // 是无效序列，而未闭合的 operation 由既有修复补上未知结果。
                if !operation.open_tools.is_empty() {
                    return Err(SessionError::InvalidStructure(
                        "terminal record with unresolved tool calls".into(),
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
            // 元数据、压缩与其他记录都不改变 operation 状态。
            _ => {}
        }
    }
    Ok(active)
}
