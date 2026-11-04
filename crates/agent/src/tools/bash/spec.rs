//! Bash 与后台任务工具的参数和说明。

use std::{num::NonZeroU64, sync::LazyLock};

use serde::Deserialize;
use serde_json::json;

use crate::tools::registry::{ToolExecution, error_result};

pub(super) const MAX_WAIT_MS: u64 = 30_000;

static DESCRIPTION: LazyLock<String> = LazyLock::new(|| {
    format!(
        "Execute a Bash command in the current working directory (Git Bash on Windows). Use POSIX syntax; `2>nul` creates a file named nul. Returns stdout and stderr, truncated to the last {}; full truncated output is saved to a file. Independent commands may run in parallel; send dependent commands in order. Each command starts exactly once. yield_time_ms controls how long to wait before returning a task ID for a still-running command (0 to 30000 ms, default 10000); use 0 to return immediately. Use task_list, task_output and task_stop to manage background commands. Background completion automatically delivers status and new output. Continue independent work; if none remains, give a brief status message and finish this response; you will resume on completion. Do not poll, sleep, or repeatedly read logs to wait. timeout_ms optionally limits total execution time, including time spent in the background; without it there is no total time limit. Stopping a task terminates its entire process tree. No interactive stdin or PTY is available.",
        crate::tools::truncate::default_cap_summary()
    )
});

/// 已解析的 Bash 调用；由工具注册表检查等待时间。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BashArgs {
    pub(crate) command: String,
    pub(crate) description: String,
    #[serde(default)]
    pub(crate) timeout_ms: Option<NonZeroU64>,
    #[serde(default = "default_yield_time_ms")]
    pub(crate) yield_time_ms: u64,
}

impl BashArgs {
    pub(crate) fn validate(self) -> Result<Self, ToolExecution> {
        if self.description.is_empty() {
            return Err(error_result("description must not be empty"));
        }
        if self.yield_time_ms > MAX_WAIT_MS {
            return Err(error_result("yield_time_ms must be between 0 and 30000"));
        }
        Ok(self)
    }
}

fn default_yield_time_ms() -> u64 {
    10_000
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TaskIdArgs {
    pub(crate) task_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TaskListArgs {}

pub(crate) fn spec() -> singularity_core::ToolDefinition {
    singularity_core::ToolDefinition {
        name: "bash".to_string(),
        description: DESCRIPTION.clone(),
        parameters_schema: json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "Bash command to execute" },
                "description": { "type": "string", "minLength": 1, "description": "Short description of this command" },
                "timeout_ms": { "type": "integer", "minimum": 1, "description": "Optional total execution timeout in milliseconds; no time limit when omitted" },
                "yield_time_ms": { "type": "integer", "minimum": 0, "maximum": MAX_WAIT_MS, "default": 10000, "description": "Wait before returning a task ID; 0 returns immediately" }
            },
            "required": ["command", "description"],
            "additionalProperties": false
        }),
    }
}

pub(crate) fn task_list_spec() -> singularity_core::ToolDefinition {
    singularity_core::ToolDefinition {
        name: "task_list".to_string(),
        description: "List this session's background commands, including saved commands from earlier runs. Commands left running in saved history are interrupted; they are not restarted. Completion notifies you automatically; do not repeatedly poll.".to_string(),
        parameters_schema: json!({ "type": "object", "properties": {}, "additionalProperties": false }),
    }
}

pub(crate) fn task_output_spec() -> singularity_core::ToolDefinition {
    singularity_core::ToolDefinition {
        name: "task_output".to_string(),
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

pub(crate) fn task_stop_spec() -> singularity_core::ToolDefinition {
    singularity_core::ToolDefinition {
        name: "task_stop".to_string(),
        description: "Stop this background command and all descendants, then wait for cleanup. Stopping an already finished command returns its existing result.".to_string(),
        parameters_schema: json!({
            "type": "object", "properties": { "task_id": { "type": "string" } },
            "required": ["task_id"], "additionalProperties": false
        }),
    }
}
