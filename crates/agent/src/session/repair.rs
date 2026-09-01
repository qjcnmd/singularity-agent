//! 崩溃恢复：operation 归约驱动的确定性修复。
//!
//! 打开写路径时把 durable 前缀归约为 operation 事实，再把未终结 operation 收敛：
//! 未解决工具（含 `replay: never` 的已启动调用）一律补写模型可见的 synthetic
//! failed ToolResult——绝不自动重放任何副作用；target 该 turn 的 pending cancel
//! 由本次 interrupted 收敛实现其 disposition（落 `control_accepted(cancelled)`，
//! 先于终态记录，与进程内取消刷盘同一顺序）；随后为该 operation 落盘唯一一条
//! `operation_finished(interrupted)`。单 lane 不变量（至多一个未终结 operation）
//! 被破坏时按 corruption 拒绝打开，不猜测、不静默降级。

use singularity_protocol::{TurnModelUsage, TurnStatus};

use super::format::{
    ControlChannel, ControlDisposition, ControlRequest, LedgerRecord, OperationKind,
    PendingWriteKind, Result, SessionEntry, SessionMetadata,
};
use super::manager::SessionManager;
use super::operation::{open_operations, reduce_controls, reduce_operations};
use crate::message::{AgentMessage, ContentBlock};

/// 恢复收敛的工具结果文本：全仓唯一来源，明确告知模型不得重试。
pub const REPAIR_UNKNOWN_OUTCOME: &str = "[previous execution outcome unknown; do not retry]";

impl SessionManager {
    /// 归约 durable 前缀并收敛未终结 operation；返回被修复的 operation 数。
    ///
    /// 修复顺序确定（同输入同输出）：先按落盘序补未解决工具的 synthetic
    /// failed 结果，再收敛 target 本 turn 的 pending cancel，最后落盘该
    /// operation 的唯一终态记录。
    pub fn repair_interrupted_operations(&mut self) -> Result<usize> {
        let operations = reduce_operations(self.entries())?;
        let open = open_operations(&operations);
        if open.len() > 1 {
            return Err(super::format::SessionError::LedgerCorrupt {
                reason: "multiple_open_operations".to_string(),
                detail: format!("{} operations are open at once", open.len()),
            });
        }
        let Some(operation) = open.first() else {
            return Ok(0);
        };
        let pending_writes = operation.pending_writes.clone();
        for tool in &operation.open_tools {
            let result = AgentMessage::ToolResult {
                content: vec![ContentBlock::Text {
                    text: REPAIR_UNKNOWN_OUTCOME.to_string(),
                }],
                tool_call_id: Some(tool.tool_call_id.clone()),
                tool_name: Some(tool.tool_name.clone()),
                is_error: Some(true),
            };
            if let Some(pending) = pending_writes.iter().find(|pending| {
                pending.kind == PendingWriteKind::ToolResult
                    && tool.result_entry_id.as_deref() == Some(pending.entry_id.as_str())
            }) {
                let _ = self.append_message_with_id(&pending.entry_id, result)?;
            } else {
                let _ = self.append_message(result)?;
            }
        }
        for pending in pending_writes {
            if pending.kind != PendingWriteKind::ToolResult {
                self.append_record(LedgerRecord::WriteAbandoned {
                    operation_id: operation.operation_id.clone(),
                    entry_id: pending.entry_id,
                    kind: pending.kind,
                    reason: REPAIR_UNKNOWN_OUTCOME.to_string(),
                })?;
            }
        }
        if let Some(turn_id) = &operation.turn_id {
            for control in reduce_controls(self.entries())?
                .into_iter()
                .filter(|control| {
                    control.channel == ControlChannel::Cancel
                        && control.turn_id == *turn_id
                        && control.disposition == ControlDisposition::Pending
                })
            {
                let request = ControlRequest {
                    control_id: control.control_id,
                    turn_id: control.turn_id,
                    channel: control.channel,
                    sequence: control.sequence,
                    text: control.text,
                };
                self.append_record(request.disposition_record(ControlDisposition::Cancelled))?;
            }
        }
        self.append_record(LedgerRecord::OperationFinished {
            operation_id: operation.operation_id.clone(),
            turn_id: operation.turn_id.clone(),
            outcome: TurnStatus::Interrupted,
            usage: (operation.kind == OperationKind::Run).then(TurnModelUsage::default),
            truncated: false,
        })?;
        Ok(1)
    }

    /// 返回当前 leaf 路径上的 metadata。
    pub fn metadata_entries(&self) -> Vec<SessionMetadata> {
        self.entries
            .iter()
            .filter_map(|entry| match entry {
                SessionEntry::Metadata { metadata, .. } => Some(metadata.clone()),
                _ => None,
            })
            .collect()
    }

    /// 返回当前 leaf 路径上的 ledger 记录。
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
