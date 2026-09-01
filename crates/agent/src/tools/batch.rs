//! 工具批次执行：按模型给定 source order 串行执行一批 preflight 判定的工具调用，
//! 保留 panic 隔离、逐工具事件发射与单工具失败不阻断其余调用的合同。

use std::path::Path;

use singularity_core::CancellationToken;
use singularity_model::ModelToolCall;

use crate::agent::{AgentEvent, AgentEvents, emit};
use crate::tools::{
    ExecuteContext, PreparedTool, ToolExecution, ToolPreflight, ToolRegistrySnapshot,
};

/// 一次模型工具调用及其 preflight 判定与预分配的结果条目 id。
pub(crate) struct PreparedToolCall {
    pub call: ModelToolCall,
    pub prepared: ToolPreflight,
    pub result_entry_id: String,
}

/// 工具执行失败时的通用错误 Execution。
pub(crate) fn tool_error_execution(error: impl std::fmt::Display) -> ToolExecution {
    ToolExecution {
        content: format!("tool execution failed: {error}"),
        is_error: true,
    }
}

/// 执行一个已通过 preflight 判定的工具，以 `catch_unwind` 隔离 panic，
/// 并通过 `on_update` 回调实时投递流式更新。
fn execute_prepared_tool(
    registry: &ToolRegistrySnapshot,
    prepared: PreparedTool,
    cwd: &Path,
    cancellation: &CancellationToken,
    mut on_update: impl FnMut(&str),
) -> ToolExecution {
    let mut update = |text: &str| on_update(text);
    registry.execute_prepared(
        prepared,
        ExecuteContext {
            cwd,
            signal: cancellation,
            on_update: Some(&mut update),
        },
    )
}

/// 按模型给定的 source order 串行执行一批工具调用：每个工具保留
/// `catch_unwind` panic 隔离与逐工具事件发射；preflight 拒绝项不进入执行，
/// 直接以模型可见失败收尾。单个工具失败不影响其余调用继续执行。
pub(crate) fn execute_tool_batch(
    registry: &ToolRegistrySnapshot,
    calls: &[PreparedToolCall],
    cwd: &Path,
    cancellation: &CancellationToken,
    events: &mut AgentEvents<'_>,
) -> Vec<ToolExecution> {
    let mut results = Vec::with_capacity(calls.len());
    for item in calls {
        emit(
            events,
            AgentEvent::ToolExecutionStarted {
                tool_name: item.call.tool_name.clone(),
                tool_call_id: item.call.tool_call_id.clone(),
                arguments: item.call.arguments.clone(),
            },
        );
        match &item.prepared {
            ToolPreflight::Rejected(execution) => {
                emit(
                    events,
                    AgentEvent::ToolExecutionEnded {
                        tool_name: item.call.tool_name.clone(),
                        tool_call_id: item.call.tool_call_id.clone(),
                        execution: execution.clone(),
                    },
                );
                results.push(execution.clone());
            }
            ToolPreflight::Ready(prepared) => {
                let prepared = prepared.clone();
                let call = item.call.clone();
                let execution = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    execute_prepared_tool(registry, prepared, cwd, cancellation, |text| {
                        emit(
                            events,
                            AgentEvent::ToolExecutionUpdate {
                                tool_name: call.tool_name.clone(),
                                tool_call_id: call.tool_call_id.clone(),
                                arguments: call.arguments.clone(),
                                partial_result: text.to_string(),
                            },
                        );
                    })
                }))
                .unwrap_or_else(|_| tool_error_execution("tool execution panicked"));
                emit(
                    events,
                    AgentEvent::ToolExecutionEnded {
                        tool_name: call.tool_name.clone(),
                        tool_call_id: call.tool_call_id.clone(),
                        execution: execution.clone(),
                    },
                );
                results.push(execution);
            }
        }
    }
    results
}
