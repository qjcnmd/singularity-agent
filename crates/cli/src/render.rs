//! CLI protocol event rendering and terminal projection.

use super::*;
use singularity_protocol::{
    AgentDiagnosticParams, ProviderAttemptEventParams, ProviderAttemptSummaryParams,
    ThreadEventParams, ToolExecutionEndParams, ToolExecutionStartParams, ToolExecutionUpdateParams,
    Turn, TurnErrorParams, TurnEventParams, TurnStatus,
};

// 过滤并脱敏可公开渲染的协议事件。
pub(super) fn protocol_events(messages: Vec<JsonRpcNotification>) -> Vec<Value> {
    messages
        .into_iter()
        .filter_map(safe_protocol_event)
        .collect()
}

// 将单条协议通知投影为约定事件字段，不透传未知 envelope 字段。
pub(super) fn safe_protocol_event(message: JsonRpcNotification) -> Option<Value> {
    let method = message.method;
    let mut output = match method.as_str() {
        "thread/started" => {
            let params =
                serde_json::from_value::<ThreadEventParams>(message.params.clone()).ok()?;
            Some(json!({
                "method": method,
                "params": {"thread": params.thread},
            }))
        }
        "turn/started" | "turn/completed" => {
            let params = serde_json::from_value::<TurnEventParams>(message.params.clone()).ok()?;
            Some(json!({
                "method": method,
                "params": {"turn": params.turn},
            }))
        }
        "item/agentMessage/delta" => {
            let params = serde_json::from_value::<ItemEventParams>(message.params.clone()).ok()?;
            Some(json!({
                "method": method,
                "params": {
                    "thread_id": params.thread_id,
                    "turn_id": params.turn_id,
                    "item_id": params.item.item_id,
                    "delta": params.delta.unwrap_or_default(),
                },
            }))
        }
        "item/started" | "item/completed" | "item/failed" => {
            let params = serde_json::from_value::<ItemEventParams>(message.params.clone()).ok()?;
            Some(json!({
                "method": method,
                "params": {
                    "thread_id": params.thread_id,
                    "turn_id": params.turn_id,
                    "item_id": params.item.item_id,
                    "error": params.error,
                },
            }))
        }
        // 工具生命周期事件：投影 toolCallId、toolName、args、partialResult 与 result。
        "tool/execution/start" => {
            let params =
                serde_json::from_value::<ToolExecutionStartParams>(message.params.clone()).ok()?;
            Some(json!({
                "method": method,
                "params": {
                    "thread_id": params.thread_id,
                    "turn_id": params.turn_id,
                    "tool_call_id": params.tool_call_id,
                    "tool_name": params.tool_name,
                    "args": params.args,
                },
            }))
        }
        "tool/execution/update" => {
            let params =
                serde_json::from_value::<ToolExecutionUpdateParams>(message.params.clone()).ok()?;
            Some(json!({
                "method": method,
                "params": {
                    "thread_id": params.thread_id,
                    "turn_id": params.turn_id,
                    "tool_call_id": params.tool_call_id,
                    "tool_name": params.tool_name,
                    "args": params.args,
                    "partial_result": params.partial_result,
                },
            }))
        }
        "tool/execution/end" => {
            let params =
                serde_json::from_value::<ToolExecutionEndParams>(message.params.clone()).ok()?;
            Some(json!({
                "method": method,
                "params": {
                    "thread_id": params.thread_id,
                    "turn_id": params.turn_id,
                    "tool_call_id": params.tool_call_id,
                    "tool_name": params.tool_name,
                    "result": params.result,
                    "is_error": params.is_error,
                },
            }))
        }
        // 保留 turn/error 的匹配身份和脱敏诊断，JSON 模式可以据此审计
        // 终态来源；客户端仍只把精确 threadId/turnId 视为本次 turn 的终态。
        "turn/error" => {
            let params = serde_json::from_value::<TurnErrorParams>(message.params.clone()).ok()?;
            Some(json!({
                "method": method,
                "params": {
                    "thread_id": params.thread_id,
                    "turn_id": params.turn_id,
                    "error": params.error,
                },
            }))
        }
        "agent/diagnostic" => {
            let params =
                serde_json::from_value::<AgentDiagnosticParams>(message.params.clone()).ok()?;
            Some(json!({
                "method": method,
                "params": {
                    "thread_id": params.thread_id,
                    "turn_id": params.turn_id,
                    "severity": params.severity,
                    "code": params.code,
                    "message": params.message,
                },
            }))
        }
        "provider/attempt" => {
            let params =
                serde_json::from_value::<ProviderAttemptEventParams>(message.params.clone())
                    .ok()?;
            Some(json!({
                "method": method,
                "params": {
                    "thread_id": params.thread_id,
                    "turn_id": params.turn_id,
                    "model_turn_ordinal": params.model_turn_ordinal,
                    "operation_phase": params.operation_phase,
                    "provider": params.provider,
                    "model": params.model,
                    "protocol": params.protocol,
                    "attempt_index": params.attempt_index,
                    "status": params.status,
                    "attempt_duration_ms": params.attempt_duration_ms,
                    "retry_scheduled": params.retry_scheduled,
                    "retry_backoff_ms": params.retry_backoff_ms,
                    "error_category": params.error_category,
                    "diagnostic_code": params.diagnostic_code,
                },
            }))
        }
        "provider/attempt/summary" => {
            let params =
                serde_json::from_value::<ProviderAttemptSummaryParams>(message.params.clone())
                    .ok()?;
            Some(json!({
                "method": method,
                "params": {
                    "thread_id": params.thread_id,
                    "turn_id": params.turn_id,
                    "model_turn_ordinal": params.model_turn_ordinal,
                    "attempt_count": params.attempt_count,
                    "retry_count": params.retry_count,
                    "latency_ms": params.latency_ms,
                },
            }))
        }
        _ => Some(json!({"method": method})),
    }?;
    if let Some(event) = message
        .params
        .get("event")
        .and_then(|value| serde_json::from_value::<EventMetadata>(value.clone()).ok())
        .and_then(|metadata| serde_json::to_value(metadata).ok())
    {
        output["event"] = event;
    }
    Some(output)
}

// 按协议 method 渲染 thread、turn 与 item 事件。
pub(super) fn render_messages(messages: &[JsonRpcNotification], render_assistant_summary: bool) {
    for message in messages {
        let method = message.method.as_str();
        match method {
            "thread/started" => {
                if let Ok(params) =
                    serde_json::from_value::<ThreadEventParams>(message.params.clone())
                {
                    println!("thread/started {}", params.thread.thread_id);
                }
            }
            "turn/started" => {
                if let Ok(params) =
                    serde_json::from_value::<TurnEventParams>(message.params.clone())
                {
                    println!("turn/started {}", params.turn.turn_id);
                    render_turn(&params.turn);
                }
            }
            "item/started" | "item/completed" => {
                if let Ok(params) =
                    serde_json::from_value::<ItemEventParams>(message.params.clone())
                {
                    println!("{method} {}", params.item.item_id);
                }
            }
            "item/agentMessage/delta" => {
                if let Ok(params) =
                    serde_json::from_value::<ItemEventParams>(message.params.clone())
                {
                    let text = params.delta.unwrap_or_default();
                    println!("{method} {text}");
                    if render_assistant_summary {
                        println!("assistant {text}");
                    }
                }
            }
            _ => println!("{method}"),
        }
    }
}

// 判断是否应额外输出已完成的 assistant 摘要。
pub(super) fn should_render_assistant_summary(turn: &Turn) -> bool {
    turn.status == TurnStatus::Completed && turn.agent_loop_status == "completed"
}

// 渲染 turn 的稳定状态行。
pub(super) fn render_turn(turn: &Turn) {
    if turn.turn_id.is_empty() {
        return;
    }
    println!(
        "turn {} {} agent_loop_status={}",
        turn.turn_id,
        turn.status.as_storage_text(),
        turn.agent_loop_status
    );
}

// 将失败、blocked 或未能安全轮询的 turn 映射为 CLI 错误。
pub(super) fn fail_for_failed_turn(turn: &Turn) -> Result<(), String> {
    let status = turn.status.as_storage_text();
    let agent_loop_status = turn.agent_loop_status.as_str();
    if matches!(turn.status, TurnStatus::Failed | TurnStatus::Interrupted)
        || matches!(agent_loop_status, "failed" | "cancelled")
    {
        if turn.turn_id.is_empty() {
            return Err(format!("error {status}: turn {status}"));
        }
        return Err(format!(
            "error {status}: turn {status}; turn {} {status}",
            turn.turn_id
        ));
    }
    Ok(())
}

// 解析显式 app-server 路径或相邻的默认二进制。
