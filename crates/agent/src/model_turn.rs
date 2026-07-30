//! Model-turn request, tool-view, and history projection helpers.
//!
//! This module owns pure request assembly and provider-history codecs. AgentLoopState remains
//! owned by the parent loop module.

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use singularity_model::{
    ModelMessage, ModelPreferences, ModelRole, ModelToolCall, ModelToolParseStatus,
    ModelToolSchema, ModelTurnRequest, ToolChoiceMode,
};
use singularity_tools::{ToolBroker, ToolCallRequest, ToolResult, approximate_token_count};

use super::context::{ContextBudget, model_messages_from_context};
use super::{
    AGENT_DEVELOPER_INSTRUCTIONS, AgentLoopInput, AgentLoopState, ContextBundle,
    ProviderProtocolContract, is_strict_tool_schema_compatible,
};

pub(super) fn safe_request_digest(request: &ModelTurnRequest) -> String {
    let encoded = serde_json::to_vec(request).unwrap_or_default();
    format!("sha256:{:x}", Sha256::digest(encoded))
}

pub(super) struct ModelToolView {
    pub(super) tools: Vec<ModelToolSchema>,
    pub(super) max_tool_calls: u32,
}

impl ModelToolView {
    pub(super) fn finalization() -> Self {
        Self {
            tools: Vec::new(),
            max_tool_calls: 0,
        }
    }

    /// Constrain the model-facing command schema to the exact input already owned by repair state.
    pub(super) fn restrict_command_input(&mut self, input: &Value) -> Result<(), String> {
        let command = input
            .get("command")
            .and_then(Value::as_str)
            .ok_or_else(|| "repair command input is missing command".to_string())?;
        let cwd = input
            .get("cwd")
            .and_then(Value::as_str)
            .ok_or_else(|| "repair command input is missing cwd".to_string())?;
        let timeout_seconds = input
            .get("timeout_seconds")
            .and_then(Value::as_u64)
            .ok_or_else(|| "repair command input is missing timeout_seconds".to_string())?;
        let tool = self
            .tools
            .iter_mut()
            .find(|tool| tool.name == singularity_tools::COMMAND_TOOL)
            .ok_or_else(|| "repair command tool is not visible".to_string())?;
        tool.parameters_schema = json!({
            "type": "object",
            "properties": {
                "command": {"type": "string", "const": command},
                "cwd": {"type": "string", "const": cwd},
                "timeout_seconds": {"type": "integer", "const": timeout_seconds}
            },
            "required": ["command", "cwd", "timeout_seconds"],
            "additionalProperties": false
        });
        Ok(())
    }
}

pub(super) fn model_turn_request(
    input: &AgentLoopInput,
    budget: &ContextBudget,
    turn_index: u32,
    state: &AgentLoopState,
    tool_view: ModelToolView,
    capabilities: &ProviderProtocolContract,
    finalization_only: bool,
) -> ModelTurnRequest {
    let tools = tool_view.tools;
    let strict_tool_schema = !tools.is_empty()
        && capabilities.supports_strict_tool_schema
        && tools
            .iter()
            .all(|tool| is_strict_tool_schema_compatible(&tool.parameters_schema));
    let mut request = ModelTurnRequest {
        request_id: format!("model_request_{}_{}", input.turn_id, turn_index),
        messages: state.messages.clone(),
        tools,
        tool_choice: Default::default(),
        model_preferences: ModelPreferences {
            max_output_tokens: Some(budget.reserved_output_tokens),
            ..input.model_preferences.clone()
        },
    };
    if finalization_only {
        request.tool_choice.mode = ToolChoiceMode::None;
    }
    request.tool_choice.max_tool_calls = tool_view.max_tool_calls;
    request.tool_choice.strict_tool_schema = strict_tool_schema;
    request
}

pub(super) fn model_tool_schemas(loop_tools: &ToolBroker) -> Vec<ModelToolSchema> {
    loop_tools
        .tool_schema_payloads()
        .into_iter()
        .filter_map(|tool| {
            Some(ModelToolSchema {
                name: tool.get("name")?.as_str()?.to_string(),
                description: tool
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                parameters_schema: tool
                    .get("input_schema")
                    .cloned()
                    .unwrap_or_else(|| json!({})),
            })
        })
        .collect()
}

pub(super) fn visible_model_tool_schemas(loop_tools: &ToolBroker) -> Vec<ModelToolSchema> {
    model_tool_schemas(loop_tools)
}

pub(super) fn model_tool_view(
    loop_tools: &ToolBroker,
    capabilities: &ProviderProtocolContract,
    max_tool_calls: u32,
) -> Result<ModelToolView, String> {
    if capabilities.max_tools_per_request == 0 {
        return Err("provider tool-definition limit must be greater than zero".to_string());
    }
    let visible_tools = visible_model_tool_schemas(loop_tools);
    if visible_tools.len() > capabilities.max_tools_per_request as usize {
        return Err(format!(
            "provider direct tool-definition limit ({}) is below the required tool count ({})",
            capabilities.max_tools_per_request,
            visible_tools.len()
        ));
    }
    Ok(ModelToolView {
        tools: visible_tools,
        max_tool_calls,
    })
}

pub(super) fn resolve_model_tool_calls(
    provider_calls: &[ModelToolCall],
    visible_tool_names: &[String],
) -> Vec<ModelToolCall> {
    provider_calls
        .iter()
        .map(|call| {
            if call.parse_status != ModelToolParseStatus::Valid {
                return call.clone();
            }
            let mut resolved = call.clone();
            if resolved.parse_status == ModelToolParseStatus::Valid
                && !visible_tool_names
                    .iter()
                    .any(|tool_name| tool_name == &resolved.tool_name)
            {
                resolved.parse_status = ModelToolParseStatus::UnknownTool;
            }
            resolved
        })
        .collect()
}

pub(super) fn model_tool_payload_tokens(tools: &[ModelToolSchema]) -> u32 {
    serde_json::to_string(tools).map_or(u32::MAX, |payload| approximate_token_count(&payload))
}

pub(super) fn reserved_model_tool_tokens(
    loop_tools: &ToolBroker,
    capabilities: &ProviderProtocolContract,
) -> Result<u32, String> {
    let visible_tools = visible_model_tool_schemas(loop_tools);
    if visible_tools.len() > capabilities.max_tools_per_request as usize {
        return Err(format!(
            "provider direct tool-definition limit ({}) is below the required tool count ({})",
            capabilities.max_tools_per_request,
            visible_tools.len()
        ));
    }
    Ok(model_tool_payload_tokens(&visible_tools))
}

pub(super) fn model_messages_from_input(
    input: &AgentLoopInput,
    context: &ContextBundle,
    max_tool_calls: u32,
) -> Vec<ModelMessage> {
    let mut messages = vec![ModelMessage::text(
        ModelRole::Developer,
        developer_instructions(input, max_tool_calls),
    )];
    messages.extend(model_messages_from_context(context));
    messages
}

pub(super) fn developer_instructions(input: &AgentLoopInput, max_tool_calls: u32) -> String {
    let tool_call_instruction = if max_tool_calls == 1 {
        "Issue at most one tool call per assistant response and wait for its result.".to_string()
    } else {
        format!(
            "Issue up to {max_tool_calls} tool calls in one response only when every call is an independent read-only operation. Issue mutations, commands, plan updates, approval-sensitive calls, and calls that depend on earlier results one at a time and wait for each result."
        )
    };
    let instructions = format!("{AGENT_DEVELOPER_INSTRUCTIONS} {tool_call_instruction}");
    match input.project_instructions.as_deref() {
        Some(project) => {
            format!("{instructions}\n\nProject instructions:\n{project}")
        }
        None => instructions,
    }
}

pub(super) fn refresh_developer_instructions(
    messages: &mut [ModelMessage],
    input: &AgentLoopInput,
    max_tool_calls: u32,
) {
    if let Some(message) = messages
        .iter_mut()
        .find(|message| message.role == ModelRole::Developer)
    {
        message.content = developer_instructions(input, max_tool_calls);
    }
}

pub(super) fn assistant_message_text(message: Option<&ModelMessage>) -> String {
    message
        .map(|message| message.content.clone())
        .unwrap_or_default()
}

pub(super) fn provider_history_assistant_message(
    original: Option<&ModelMessage>,
    model_visible_calls: &[ModelToolCall],
    execution_calls: &[ModelToolCall],
    rejected_calls: &[bool],
) -> ModelMessage {
    // 拒绝调用的参数永不重放；可移植的原工具名和 call_id 保持配对。
    debug_assert_eq!(model_visible_calls.len(), execution_calls.len());
    debug_assert_eq!(model_visible_calls.len(), rejected_calls.len());
    let mut message = original
        .cloned()
        .unwrap_or_else(|| ModelMessage::assistant_tool_calls(Vec::new()));
    message.tool_calls = model_visible_calls
        .iter()
        .zip(execution_calls)
        .zip(rejected_calls)
        .map(|((model_visible_call, execution_call), rejected)| {
            if !rejected && execution_call.parse_status == ModelToolParseStatus::Valid {
                model_visible_call.clone()
            } else {
                provider_history_rejected_tool_call(model_visible_call)
            }
        })
        .collect();
    message
}

/// Build a provider-facing rejected call without replaying untrusted arguments.
pub(super) fn provider_history_rejected_tool_call(call: &ModelToolCall) -> ModelToolCall {
    ModelToolCall {
        tool_call_id: call.tool_call_id.clone(),
        tool_name: call.tool_name.clone(),
        arguments: json!({}),
        raw_arguments: "{}".to_string(),
        parse_status: ModelToolParseStatus::Valid,
        validation_errors: Vec::new(),
    }
}

pub(super) fn tool_result_message(tool_result: &ToolResult) -> ModelMessage {
    let payload = tool_result.to_message_payload();
    let mut message = ModelMessage::text(ModelRole::Tool, payload.to_string());
    message.tool_call_id = Some(tool_result.tool_call_id.clone());
    message
}

pub(super) fn tool_call_request(call: &ModelToolCall) -> ToolCallRequest {
    // 执行 broker 校验解析后的可执行输入；provider 原始文本保留在模型消息和 approval checkpoint 中，不能定义执行器 payload。
    ToolCallRequest::new(
        call.tool_call_id.clone(),
        call.tool_name.clone(),
        serde_json::to_string(&call.arguments).expect("model tool arguments serialize"),
    )
}
