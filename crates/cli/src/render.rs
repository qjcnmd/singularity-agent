//! CLI protocol event rendering and terminal projection.

use super::*;
use singularity_protocol::{ThreadEventParams, Turn, TurnEventParams, TurnStatus};

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
    let params = serde_json::from_value::<ItemEventParams>(message.params.clone()).ok();
    let item_id = params
        .as_ref()
        .map(|params| params.item.item_id.as_str())
        .unwrap_or("");
    let mut output = match method.as_str() {
        "item/agentMessage/delta" => Some(json!({
            "method": method,
            "params": {
                "item_id": item_id,
                "delta": params.and_then(|params| params.delta).unwrap_or_default(),
            },
        })),
        "item/started" | "item/completed" => Some(json!({
            "method": method,
            "params": {"item_id": item_id},
        })),
        // 工具生命周期事件：投影 toolCallId、toolName、args、partialResult 与 result。
        "tool/execution/start" => {
            let params = serde_json::from_value::<Value>(message.params.clone()).ok();
            let tool_call_id = params
                .as_ref()
                .and_then(|p| p.get("toolCallId").and_then(Value::as_str))
                .unwrap_or("");
            let tool_name = params
                .as_ref()
                .and_then(|p| p.get("toolName").and_then(Value::as_str))
                .unwrap_or("");
            let args = params
                .as_ref()
                .and_then(|p| p.get("args"))
                .cloned()
                .unwrap_or(Value::Null);
            Some(json!({
                "method": method,
                "params": {
                    "tool_call_id": tool_call_id,
                    "tool_name": tool_name,
                    "args": args,
                },
            }))
        }
        "tool/execution/update" => {
            let params = serde_json::from_value::<Value>(message.params.clone()).ok();
            let tool_call_id = params
                .as_ref()
                .and_then(|p| p.get("toolCallId").and_then(Value::as_str))
                .unwrap_or("");
            let tool_name = params
                .as_ref()
                .and_then(|p| p.get("toolName").and_then(Value::as_str))
                .unwrap_or("");
            let args = params
                .as_ref()
                .and_then(|p| p.get("args"))
                .cloned()
                .unwrap_or(Value::Null);
            let partial_result = params
                .as_ref()
                .and_then(|p| p.get("partialResult").and_then(Value::as_str))
                .unwrap_or("");
            Some(json!({
                "method": method,
                "params": {
                    "tool_call_id": tool_call_id,
                    "tool_name": tool_name,
                    "args": args,
                    "partial_result": partial_result,
                },
            }))
        }
        "tool/execution/end" => {
            let params = serde_json::from_value::<Value>(message.params.clone()).ok();
            let tool_call_id = params
                .as_ref()
                .and_then(|p| p.get("toolCallId").and_then(Value::as_str))
                .unwrap_or("");
            let tool_name = params
                .as_ref()
                .and_then(|p| p.get("toolName").and_then(Value::as_str))
                .unwrap_or("");
            let result = params
                .as_ref()
                .and_then(|p| p.get("result"))
                .cloned()
                .unwrap_or(Value::Null);
            let is_error = params
                .as_ref()
                .and_then(|p| p.get("isError").and_then(Value::as_bool))
                .unwrap_or(false);
            Some(json!({
                "method": method,
                "params": {
                    "tool_call_id": tool_call_id,
                    "tool_name": tool_name,
                    "result": result,
                    "is_error": is_error,
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
