use std::num::NonZeroU64;

use serde::Deserialize;
use serde_json::json;

use crate::tools::registry::{
    BASH_TOOL_NAME, TASK_LIST_TOOL_NAME, TASK_OUTPUT_TOOL_NAME, TASK_STOP_TOOL_NAME, ToolExecution,
    error_result,
};

pub(super) const MAX_WAIT_MS: u64 = 30_000;
pub(super) const MAX_TIMEOUT_MS: u64 = 24 * 60 * 60 * 1_000;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BashArgs {
    pub(crate) command: String,
    pub(crate) description: String,
    #[serde(default)]
    pub(crate) timeout_ms: Option<NonZeroU64>,
    #[serde(default)]
    pub(crate) yield_time_ms: Option<u64>,
}

impl BashArgs {
    pub(crate) fn yield_time_ms(&self) -> u64 {
        self.yield_time_ms.unwrap_or(10_000)
    }

    pub(crate) fn validate(self) -> Result<Self, ToolExecution> {
        if self.description.is_empty() {
            return Err(error_result("description must not be empty"));
        }
        if self.timeout_ms.is_some_and(|ms| ms.get() > MAX_TIMEOUT_MS) {
            return Err(error_result(format!("timeout_ms must be at most {MAX_TIMEOUT_MS}")));
        }
        if self.yield_time_ms() > MAX_WAIT_MS {
            return Err(error_result("yield_time_ms must be between 0 and 30000"));
        }
        Ok(self)
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TaskIdArgs {
    pub(crate) task_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TaskListArgs {}

pub(crate) fn spec() -> app_core::ToolDefinition {
    app_core::ToolDefinition {
        name: BASH_TOOL_NAME.to_string(),
        description: format!(
            "Execute a Bash command in the current working directory (Git Bash on Windows). Use POSIX syntax; `2>nul` creates a file named nul. Returns stdout and stderr; output over {} keeps the beginning and end with the middle omitted, and the full output is saved to a file. Independent commands may run in parallel; send dependent commands in order. Each command starts exactly once. yield_time_ms controls how long to wait before returning a task ID for a still-running command (0 to 30000 ms, default 10000); use 0 to return immediately. Use task_list, task_output and task_stop to manage background commands. Background completion automatically delivers status and new output. Continue independent work; if none remains, give a brief status message and finish this response; you will resume on completion. Do not poll, sleep, or repeatedly read logs to wait. timeout_ms optionally limits total execution time, including time spent in the background; without it there is no total time limit. Stopping a task terminates its entire process tree. No interactive stdin or PTY is available.",
            crate::tools::registry::command_cap_summary()
        ),
        parameters_schema: json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "Bash command to execute" },
                "description": { "type": "string", "minLength": 1, "description": "Short description of this command" },
                "timeout_ms": { "type": "integer", "minimum": 1, "maximum": MAX_TIMEOUT_MS, "description": "Optional total execution timeout in milliseconds; no time limit when omitted" },
                "yield_time_ms": { "type": "integer", "minimum": 0, "maximum": MAX_WAIT_MS, "default": 10000, "description": "Wait before returning a task ID; 0 returns immediately" }
            },
            "required": ["command", "description"],
            "additionalProperties": false
        }),
    }
}

pub(crate) fn task_list_spec() -> app_core::ToolDefinition {
    app_core::ToolDefinition {
        name: TASK_LIST_TOOL_NAME.to_string(),
        description: "List this session's background commands, including saved commands from earlier runs. Commands left running in saved history are interrupted; they are not restarted. Completion notifies you automatically; do not repeatedly poll.".to_string(),
        parameters_schema: json!({ "type": "object", "properties": {}, "additionalProperties": false }),
    }
}

pub(crate) fn task_output_spec() -> app_core::ToolDefinition {
    app_core::ToolDefinition {
        name: TASK_OUTPUT_TOOL_NAME.to_string(),
        description: "Inspect a background command when its current output helps you act now. Returns immediately with status and only output not already delivered by bash, task_output or a completion notice. No cursor or waiting is needed. Completion delivers new output automatically; do not poll. Use read on the full-output file to revisit earlier text.".to_string(),
        parameters_schema: json!({
            "type": "object",
            "properties": {
                "task_id": { "type": "string" }
            },
            "required": ["task_id"], "additionalProperties": false
        }),
    }
}

pub(crate) fn task_stop_spec() -> app_core::ToolDefinition {
    app_core::ToolDefinition {
        name: TASK_STOP_TOOL_NAME.to_string(),
        description: "Stop this background command and all descendants, then wait for cleanup. Stopping an already finished command returns its existing result.".to_string(),
        parameters_schema: json!({
            "type": "object", "properties": { "task_id": { "type": "string" } },
            "required": ["task_id"], "additionalProperties": false
        }),
    }
}
