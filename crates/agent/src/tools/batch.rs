//! 工具批次执行：按模型给定 source order 串行执行一批 preflight 判定的工具调用，
//! 保留 panic 隔离、逐工具事件发射与单工具失败不阻断其余调用的合同。

use std::path::Path;

use singularity_core::CancellationToken;
use singularity_model::ModelToolCall;

use crate::agent::{AgentEvent, AgentEvents};
use crate::tools::{ExecuteContext, PreparedTool, ToolExecution, ToolRegistry};

/// preflight 判定结果：可执行工具，或模型可见的拒绝执行。
pub(crate) enum Prepared {
    Ready(PreparedTool),
    Rejected(ToolExecution),
}

/// 一次模型工具调用及其 preflight 判定。
pub(crate) struct PreparedToolCall {
    pub call: ModelToolCall,
    pub prepared: Prepared,
}

/// 通过单一事件出口投递一个事件；事件投影是尽力而为的，回调不再返回
/// 错误——投影失败由消费方（app-server/CLI）自行吸收诊断，不影响轮次结果。
pub(crate) fn emit(events: &mut AgentEvents<'_>, event: AgentEvent) {
    if let Some(callback) = events.on_event.as_deref_mut() {
        callback(event);
    }
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
    registry: &ToolRegistry,
    prepared: PreparedTool,
    call: &ModelToolCall,
    cwd: &Path,
    cancellation: &CancellationToken,
    mut on_update: impl FnMut(&str),
) -> ToolExecution {
    let mut update = |text: &str| on_update(text);
    match registry.execute_prepared(
        prepared,
        ExecuteContext {
            args: call.arguments.clone(),
            cwd,
            signal: Some(cancellation),
            on_update: Some(&mut update),
        },
    ) {
        Ok(execution) => execution,
        Err(error) => tool_error_execution(error),
    }
}

/// 按模型给定的 source order 串行执行一批工具调用：每个工具保留
/// `catch_unwind` panic 隔离与逐工具事件发射；preflight 拒绝项不进入执行，
/// 直接以模型可见失败收尾。单个工具失败不影响其余调用继续执行。
pub(crate) fn execute_tool_batch(
    registry: &ToolRegistry,
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
            Prepared::Rejected(execution) => {
                emit(
                    events,
                    AgentEvent::ToolExecutionEnded {
                        tool_name: item.call.tool_name.clone(),
                        tool_call_id: item.call.tool_call_id.clone(),
                        execution: execution.clone(),
                    },
                );
                results.push(execution.clone());
                continue;
            }
            Prepared::Ready(prepared) => {
                let prepared = prepared.clone();
                let call = item.call.clone();
                let execution = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    execute_prepared_tool(registry, prepared, &call, cwd, cancellation, |text| {
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
