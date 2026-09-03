//! Durable operation/control 归约。
//!
//! Session ledger 的物理行是唯一事实源；本模块把它折叠为可恢复 operation、
//! 工具调用投影和控制投影。全部记录由持写者锁的单一写者顺序
//! 追加产生，读侧只信任并投影事实：引用不存在 operation 的记录按无害跳过，
//! 未终结 operation 一律由修复收敛，绝不从进程退出方式猜测副作用。

use std::collections::{HashMap, HashSet};

use super::format::{
    ControlChannel, ControlDisposition, LedgerRecord, OperationKind, SessionEntry,
};
use crate::message::{AgentMessageRole, ContentBlock};

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

/// 把 durable 前缀折叠为按启动顺序排列的 operation 事实。不可失败：
/// 任何引用未知 operation 的记录跳过，任何形状由单一写者的类型化
/// append API 保证。
pub fn reduce_operations(entries: &[SessionEntry]) -> Vec<OperationState> {
    let mut order = Vec::new();
    let mut states = HashMap::<String, OperationState>::new();
    let mut operation_positions = HashMap::<String, usize>::new();
    let mut finish_positions = HashMap::<String, usize>::new();

    for (position, entry) in entries.iter().enumerate() {
        let SessionEntry::Record { record, .. } = entry else {
            continue;
        };
        match record {
            LedgerRecord::OperationStarted {
                operation_id,
                kind,
                turn_id,
            } => {
                if states.contains_key(operation_id) {
                    continue;
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
                        open_tools: Vec::new(),
                    },
                );
            }
            LedgerRecord::OperationFinished {
                operation_id,
                outcome,
                ..
            } => {
                if let Some(state) = states.get_mut(operation_id) {
                    state.finished = Some(*outcome);
                    finish_positions.insert(operation_id.to_string(), position);
                }
            }
            LedgerRecord::ControlAccepted { .. } => {}
        }
    }

    let mut result_ids = HashSet::<String>::new();
    for entry in entries {
        let SessionEntry::Message { message, .. } = entry else {
            continue;
        };
        if message.role() == AgentMessageRole::ToolResult
            && let Some(tool_call_id) = message.tool_call_id()
        {
            result_ids.insert(tool_call_id.to_string());
        }
    }

    // assistant 消息声明的工具调用：未终结且缺失对应结果时，归入 open_tools。
    for (position, entry) in entries.iter().enumerate() {
        let SessionEntry::Message { message, .. } = entry else {
            continue;
        };
        if message.role() != AgentMessageRole::Assistant {
            continue;
        }
        let Some(operation_id) = operation_positions
            .iter()
            .filter(|(operation, start)| {
                **start < position
                    && finish_positions
                        .get(*operation)
                        .is_none_or(|finish| position < *finish)
            })
            .max_by_key(|(_, start)| **start)
            .map(|(operation, _)| operation)
        else {
            continue;
        };
        let Some(state) = states.get_mut(operation_id) else {
            continue;
        };
        if state.finished.is_some() {
            continue;
        }
        for block in message.tool_calls() {
            let ContentBlock::ToolCall {
                id: tool_call_id,
                name,
                ..
            } = block
            else {
                continue;
            };
            if !result_ids.contains(tool_call_id)
                && !state
                    .open_tools
                    .iter()
                    .any(|tool| &tool.tool_call_id == tool_call_id)
            {
                state.open_tools.push(UnresolvedTool {
                    tool_call_id: tool_call_id.clone(),
                    tool_name: name.clone(),
                });
            }
        }
    }

    order
        .into_iter()
        .filter_map(|id| states.remove(&id))
        .collect()
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

/// 控制事实折叠：pending 接受记录与其后的终态 disposition 记录折叠为单条
/// 最终归宿，按 FIFO sequence 排序。
pub fn reduce_controls(entries: &[SessionEntry]) -> Vec<ReducedControl> {
    let mut by_id = HashMap::<String, ReducedControl>::new();
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
        match by_id.get_mut(id) {
            Some(existing) => {
                if *disposition != ControlDisposition::Pending {
                    existing.disposition = *disposition;
                }
                if text.is_some() {
                    existing.text = text.clone();
                }
            }
            None => {
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
            }
        }
    }
    let mut controls = by_id.into_values().collect::<Vec<_>>();
    controls.sort_by_key(|control| control.sequence);
    controls
}
