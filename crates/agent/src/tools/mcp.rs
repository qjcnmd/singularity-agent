use std::collections::BTreeMap;
use std::fmt::Write as _;

use mcp::McpTool;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use super::registry::{MCP_CALL_TOOL_NAME, MCP_SCHEMA_TOOL_NAME};
use super::{ToolExecution, error_result};
use crate::image::InputImage;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct McpSchemaArgs {
    pub(crate) server: String,
    pub(crate) tools: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct McpCallArgs {
    pub(crate) server: String,
    pub(crate) tool: String,
    #[serde(default, deserialize_with = "super::registry::deserialize_null_default")]
    pub(crate) arguments: Map<String, Value>,
}

pub(crate) fn schema_spec() -> model::ToolDefinition {
    model::ToolDefinition {
        name: MCP_SCHEMA_TOOL_NAME.into(),
        description: "Inspect the full description and JSON input schema of one or more specific MCP tools on a server before invoking them via mcp_call. Only request the tools you need.".into(),
        parameters_schema: json!({
            "type": "object",
            "properties": {
                "server": { "type": "string", "description": "MCP server ID from the developer instructions" },
                "tools": {
                    "type": "array",
                    "minItems": 1,
                    "items": { "type": "string" },
                    "description": "One or more specific tool names exposed by that MCP server"
                }
            },
            "required": ["server", "tools"],
            "additionalProperties": false
        }),
    }
}

pub(crate) fn call_spec() -> model::ToolDefinition {
    model::ToolDefinition {
        name: MCP_CALL_TOOL_NAME.into(),
        description: "Invoke an MCP tool by server ID and tool name. Call mcp_schema first if the tool takes arguments whose schema you have not yet inspected; tools marked (no args) can be called directly.".into(),
        parameters_schema: json!({
            "type": "object",
            "properties": {
                "server": { "type": "string", "description": "MCP server ID" },
                "tool": { "type": "string", "description": "Tool name exposed by that MCP server" },
                "arguments": { "type": "object", "description": "JSON object arguments matching the tool input schema (omit or pass {} if the tool takes no arguments)" }
            },
            "required": ["server", "tool"],
            "additionalProperties": false
        }),
    }
}

pub(crate) fn lookup_tool<'a>(
    servers: &'a BTreeMap<String, Vec<McpTool>>,
    server: &str,
    tool: &str,
) -> Result<&'a McpTool, ToolExecution> {
    let tools = servers.get(server).ok_or_else(|| error_result(format!("Unknown MCP server: {server}")))?;
    tools
        .iter()
        .find(|candidate| candidate.name == tool)
        .ok_or_else(|| error_result(format!("Unknown tool '{tool}' on MCP server '{server}'")))
}

pub(crate) fn lookup_tools(
    servers: &BTreeMap<String, Vec<McpTool>>,
    server: &str,
    names: &[String],
) -> Result<Vec<McpTool>, ToolExecution> {
    if names.is_empty() {
        return Err(error_result("tools must contain at least one tool name"));
    }
    let mut resolved = Vec::with_capacity(names.len());
    let mut seen = std::collections::HashSet::new();
    for name in names {
        if !seen.insert(name.as_str()) {
            continue;
        }
        resolved.push(lookup_tool(servers, server, name)?.clone());
    }
    Ok(resolved)
}

pub(crate) fn execute_schema(tools: &[McpTool]) -> ToolExecution {
    let sections: Vec<String> = tools
        .iter()
        .map(|tool| {
            let schema =
                serde_json::to_string_pretty(tool.parameters_schema.as_ref()).expect("tool schema is JSON");
            let description = if tool.description.trim().is_empty() {
                "(no description)"
            } else {
                tool.description.trim()
            };
            format!("Tool: {}\nDescription: {description}\nInput schema:\n{schema}", tool.name)
        })
        .collect();
    ToolExecution::text(sections.join("\n\n---\n\n"))
}

pub(super) async fn execute(
    tool: McpTool,
    args: Map<String, Value>,
    output_directory: std::path::PathBuf,
    signal: &CancellationToken,
) -> ToolExecution {
    if signal.is_cancelled() {
        return error_result(super::ABORTED_MESSAGE);
    }
    let result = tool.call(args, signal).await;
    tokio::task::spawn_blocking(move || {
        let mut execution = match result {
            Ok(result) => {
                let mut execution = ToolExecution::text(result.content);
                execution.is_error = result.is_error;
                for data in result.images {
                    match InputImage::from_base64(format!("{}.image", tool.name), &data) {
                        Ok(image) => execution.images.push(image),
                        Err(error) => {
                            let _ = write!(execution.content, "\n\nMCP 图片无效：{error}");
                            execution.is_error = true;
                        }
                    }
                }
                execution
            }
            Err(error) => error_result(error.to_string()),
        };
        execution.content = super::output::format_head_tail_bounded(&execution.content, &output_directory);
        execution
    })
    .await
    .expect("MCP result conversion completes while the runtime is running")
}
