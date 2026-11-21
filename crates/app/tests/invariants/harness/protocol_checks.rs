//! Messages（Anthropic 兼容）与 OpenAI Responses 上游请求协议不变量校验。

use std::collections::HashSet;

use serde_json::Value;

/// Anthropic Messages 协议不变量：
/// 1. 工具 schema 必须全部是合法 object schema；
/// 2. 角色只能是 `user` / `assistant` 且严格交替，不存在空内容消息；
/// 3. 每个 assistant `tool_use` 紧跟的下一条 user 消息里必须包含对应 `tool_result`，
///    且不存在重复或孤儿 `tool_use` / `tool_result` ID。
pub fn check_messages_request_invariants(body: &Value) -> Result<(), String> {
    if let Some(tools) = body.get("tools").and_then(Value::as_array) {
        for tool in tools {
            if tool.get("name").and_then(Value::as_str).is_none_or(str::is_empty) {
                return Err(format!("Messages tool missing name: {tool}"));
            }
            if tool.pointer("/input_schema/type").and_then(Value::as_str) != Some("object") {
                return Err(format!("Messages tool schema must be object: {tool}"));
            }
        }
    }
    let Some(messages) = body.get("messages").and_then(Value::as_array) else {
        return Err(format!("Messages request must have messages array; body={body}"));
    };
    if messages.is_empty() {
        return Err(format!("Messages request must not have empty messages; body={body}"));
    }

    let mut prev_role: Option<&str> = None;
    let mut expected_tool_ids: HashSet<String> = HashSet::new();
    for (idx, message) in messages.iter().enumerate() {
        let Some(role) = message.get("role").and_then(Value::as_str) else {
            return Err(format!("message missing role at index {idx}"));
        };
        if !matches!(role, "user" | "assistant") {
            return Err(format!("unsupported Messages role {role} at index {idx}"));
        }
        if let Some(prev) = prev_role
            && prev == role
        {
            return Err(format!("Messages roles must strictly alternate at index {idx}: {messages:?}"));
        }
        prev_role = Some(role);
        let Some(blocks) = message.get("content").and_then(Value::as_array) else {
            return Err(format!("message content must be array at index {idx}"));
        };
        if blocks.is_empty() {
            return Err(format!("message content must not be empty at index {idx}"));
        }

        if role == "user" {
            let mut seen_results = HashSet::new();
            for block in blocks {
                if block.get("type").and_then(Value::as_str) == Some("tool_result") {
                    let Some(id) = block.get("tool_use_id").and_then(Value::as_str) else {
                        return Err(format!("tool_result missing tool_use_id at index {idx}"));
                    };
                    if !expected_tool_ids.contains(id) {
                        return Err(format!(
                            "orphaned tool_result {id} without preceding tool_use at index {idx}"
                        ));
                    }
                    if !seen_results.insert(id.to_string()) {
                        return Err(format!("duplicate tool_result id {id} at index {idx}"));
                    }
                }
            }
            if seen_results != expected_tool_ids {
                return Err(format!(
                    "tool_use/tool_result mismatch at index {idx}: expected={expected_tool_ids:?}, seen={seen_results:?}"
                ));
            }
            expected_tool_ids.clear();
        } else {
            if !expected_tool_ids.is_empty() {
                return Err(format!("unanswered tool_use before next assistant message at index {idx}"));
            }
            for block in blocks {
                if block.get("type").and_then(Value::as_str) == Some("tool_use") {
                    let Some(id) = block.get("id").and_then(Value::as_str) else {
                        return Err(format!("tool_use missing id at index {idx}"));
                    };
                    if !expected_tool_ids.insert(id.to_string()) {
                        return Err(format!("duplicate tool_use id {id} at index {idx}"));
                    }
                }
            }
        }
    }
    if !expected_tool_ids.is_empty() {
        return Err(format!("dangling tool_use at end of Messages request: {expected_tool_ids:?}"));
    }
    Ok(())
}

/// OpenAI Responses 协议不变量：
/// 1. 函数工具必须是合法 object schema；
/// 2. 每个 `function_call` 都有匹配的 `function_call_output`，无重复 ID、无孤儿输出。
pub fn check_responses_request_invariants(body: &Value) -> Result<(), String> {
    if let Some(tools) = body.get("tools").and_then(Value::as_array) {
        for tool in tools {
            if tool.get("type").and_then(Value::as_str) != Some("function") {
                return Err(format!("Responses tool type must be function: {tool}"));
            }
            if tool.pointer("/parameters/type").and_then(Value::as_str) != Some("object") {
                return Err(format!("Responses tool schema must be object: {tool}"));
            }
        }
    }
    let Some(input) = body.get("input").and_then(Value::as_array) else {
        return Err(format!("Responses request must have input array; body={body}"));
    };
    if input.is_empty() {
        return Err(format!("Responses request must not have empty input; body={body}"));
    }
    let mut call_ids = HashSet::new();
    let mut output_ids = HashSet::new();
    for item in input {
        match item.get("type").and_then(Value::as_str) {
            Some("function_call") => {
                let Some(id) = item.get("call_id").and_then(Value::as_str) else {
                    return Err(format!("Responses function_call missing call_id: {item}"));
                };
                if !call_ids.insert(id.to_string()) {
                    return Err(format!("duplicate Responses function_call id {id}"));
                }
            }
            Some("function_call_output") => {
                let Some(id) = item.get("call_id").and_then(Value::as_str) else {
                    return Err(format!("Responses function_call_output missing call_id: {item}"));
                };
                if !call_ids.contains(id) {
                    return Err(format!("orphaned function_call_output {id}"));
                }
                if !output_ids.insert(id.to_string()) {
                    return Err(format!("duplicate Responses function_call_output id {id}"));
                }
            }
            _ => {}
        }
    }
    if call_ids != output_ids {
        return Err(format!(
            "Responses function_call/output mismatch: calls={call_ids:?}, outputs={output_ids:?}"
        ));
    }
    Ok(())
}
