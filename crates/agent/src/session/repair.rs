//! 崩溃恢复只落盘标记：中断的 operation 与结果未知的工具调用，绝不重放副作用。

use singularity_protocol::{TurnModelUsage, TurnStatus};

use super::format::{LedgerRecord, OperationKind, Result};
use super::manager::SessionManager;
use super::operation::OperationState;
#[cfg(any(test, feature = "test-support"))]
use super::{SessionData, SessionEntry};

/// 恢复只告诉模型「结果未知」；下一步由模型看当前状态自己决定，宿主不会自动重放。
pub const REPAIR_UNKNOWN_OUTCOME: &str = "[previous execution was interrupted; outcome unknown. Inspect the current state before deciding whether to repeat an action with side effects.]";

impl SessionManager {
    /// 消费本次打开时校验出的 operation，把中断的执行收尾。
    ///
    /// 修复顺序固定，同输入必然同输出：先按落盘顺序给未解决的工具调用补一条合成的
    /// 失败结果，再落盘该 operation 唯一的终态记录。
    pub(super) fn repair_interrupted_operation(
        &mut self,
        operation: Option<OperationState>,
    ) -> Result<()> {
        let Some(operation) = operation else {
            return Ok(());
        };
        for tool_call_id in &operation.open_tools {
            let result = crate::message::tool_result_text(
                tool_call_id,
                REPAIR_UNKNOWN_OUTCOME.to_string(),
                true,
            );
            let _ = self.append_message(result)?;
        }
        self.append_record(LedgerRecord::OperationFinished {
            operation_id: operation.operation_id,
            turn_id: operation.turn_id,
            outcome: TurnStatus::Interrupted,
            usage: (operation.kind == OperationKind::Run).then(TurnModelUsage::default),
            // 崩溃修复只补齐终态事实，不编造具体的失败原因。
            error: None,
            truncated: false,
            user_stopped: false,
        })?;
        Ok(())
    }
}

#[cfg(any(test, feature = "test-support"))]
impl SessionData {
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
