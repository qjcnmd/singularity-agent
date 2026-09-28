//! 从日志尾部恢复尚未结束的执行；已经结束的操作不参与恢复。
use super::format::{LedgerRecord, SessionEntry};
use crate::message::AgentMessage;

/// 未结束的回合或独立压缩，以及按调用顺序排列的未闭合工具。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationState {
    pub turn_id: Option<String>,
    pub open_tools: Vec<String>,
}

/// 单会话顺序执行，只有最后一次操作可能需要补写中断结果。
pub fn reduce_operations(entries: &[SessionEntry]) -> Option<OperationState> {
    for (position, entry) in entries.iter().enumerate().rev() {
        match entry {
            SessionEntry::Record {
                record: LedgerRecord::OperationFinished { .. },
                ..
            } => return None,
            SessionEntry::Record {
                record: LedgerRecord::OperationStarted { turn_id },
                ..
            } => {
                let mut operation = OperationState {
                    turn_id: turn_id.clone(),
                    open_tools: Vec::new(),
                };
                for entry in &entries[position + 1..] {
                    if let SessionEntry::Message { message, .. } = entry {
                        if matches!(message, AgentMessage::Assistant { .. }) {
                            operation
                                .open_tools
                                .extend(message.tool_calls().map(|call| call.tool_call_id.clone()));
                        } else if let Some(id) = message.tool_call_id() {
                            operation.open_tools.retain(|call| call != id);
                        }
                    }
                }
                return Some(operation);
            }
            _ => {}
        }
    }
    None
}
