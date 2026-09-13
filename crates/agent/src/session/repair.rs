//! Mark interrupted operations and unknown tool outcomes; never replay side effects.

use singularity_protocol::{TurnModelUsage, TurnStatus};

use super::format::{LedgerRecord, OperationKind, Result};
use super::manager::SessionManager;
use super::operation::reduce_operations;
#[cfg(any(test, feature = "test-support"))]
use super::{SessionData, SessionEntry};
use crate::message::{AgentMessage, ContentBlock};

/// 恢复只报告未知结果；由模型检查当前状态并决定下一步，宿主不自动重放。
pub const REPAIR_UNKNOWN_OUTCOME: &str = "[previous execution was interrupted; outcome unknown. Inspect the current state before deciding whether to repeat an action with side effects.]";

impl SessionManager {
    /// 归约 durable 前缀并收敛至多一个未终结 operation；返回被修复的 operation 数。
    ///
    /// 修复顺序确定（同输入同输出）：先按落盘序补未解决工具的 synthetic
    /// failed 结果，再落盘该 operation 的唯一终态记录。
    pub fn repair_interrupted_operations(&mut self) -> Result<usize> {
        let Some(operation) = reduce_operations(self.entries())? else {
            return Ok(0);
        };
        for tool in &operation.open_tools {
            let result = AgentMessage::ToolResult {
                content: vec![ContentBlock::Text {
                    text: REPAIR_UNKNOWN_OUTCOME.to_string(),
                }],
                tool_call_id: Some(tool.tool_call_id.clone()),
                tool_name: Some(tool.tool_name.clone()),
                is_error: Some(true),
                duration_ms: None,
                diff: None,
            };
            let _ = self.append_message(result)?;
        }
        self.append_record(LedgerRecord::OperationFinished {
            operation_id: operation.operation_id,
            turn_id: operation.turn_id,
            outcome: TurnStatus::Interrupted,
            usage: (operation.kind == OperationKind::Run).then(TurnModelUsage::default),
            truncated: false,
            user_stopped: false,
        })?;
        Ok(1)
    }
}

#[cfg(any(test, feature = "test-support"))]
impl SessionData {
    /// 返回会话中的 ledger 记录。
    pub fn ledger_records(&self) -> Vec<LedgerRecord> {
        self.entries
            .iter()
            .filter_map(|entry| match entry {
                SessionEntry::Record { record, .. } => Some(record.clone()),
                _ => None,
            })
            .collect()
    }
}
