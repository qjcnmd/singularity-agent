//! Bash 与后台任务工具的参数和说明。

use std::{num::NonZeroU64, sync::LazyLock};

use serde::Deserialize;
use serde_json::json;

use crate::tools::registry::{ToolExecution, error_result};

pub(super) const MAX_WAIT_MS: u64 = 30_000;

static DESCRIPTION: LazyLock<String> = LazyLock::new(|| {
    format!(
        "Execute a Bash command in the current working directory (Git Bash on Windows). Use POSIX syntax; `2>nul` creates a file named nul. Returns stdout and stderr, truncated to the last {}; full truncated output is saved to a file. Independent commands may run in parallel; send dependent commands in order. Each command starts exactly once. yield_time_ms controls how long to wait before returning a task ID for a still-running command (0 to 30000 ms, default 10000); use 0 to return immediately. Use task_list, task_output and task_stop to manage background commands. Background completion notifies you automatically: do not repeatedly poll. timeout_ms optionally limits total execution time, including time spent in the background; without it there is no total time limit. Stopping a task terminates its entire process tree. No interactive stdin or PTY is available.",
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
pub(crate) struct TaskOutputArgs {
    pub(crate) task_id: String,
    #[serde(default)]
    pub(crate) wait_ms: u64,
}

impl TaskOutputArgs {
    pub(crate) fn validate(self) -> Result<Self, ToolExecution> {
        if self.wait_ms > MAX_WAIT_MS {
            return Err(error_result("wait_ms must be between 0 and 30000"));
        }
        Ok(self)
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TaskListArgs {}

pub(crate) fn spec() -> singularity_model::ModelToolSchema {
    singularity_model::ModelToolSchema {
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

pub(crate) fn task_list_spec() -> singularity_model::ModelToolSchema {
    singularity_model::ModelToolSchema {
        name: "task_list".to_string(),
        description: "List this session's background commands, including saved commands from earlier runs. Commands left running in saved history are interrupted; they are not restarted. Completion notifies you automatically; do not repeatedly poll.".to_string(),
        parameters_schema: json!({ "type": "object", "properties": {}, "additionalProperties": false }),
    }
}

pub(crate) fn task_output_spec() -> singularity_model::ModelToolSchema {
    singularity_model::ModelToolSchema {
        name: "task_output".to_string(),
        description: "Read a background command's bounded accumulated output tail and current state, including saved output after reopening the session. wait_ms waits for a live command for at most 30000 ms; it does not restart the command or reset its timeout. Completion notifies you automatically, so avoid short polling loops.".to_string(),
        parameters_schema: json!({
            "type": "object",
            "properties": {
                "task_id": { "type": "string" },
                "wait_ms": { "type": "integer", "minimum": 0, "maximum": MAX_WAIT_MS, "default": 0 }
            },
            "required": ["task_id"], "additionalProperties": false
        }),
    }
}

pub(crate) fn task_stop_spec() -> singularity_model::ModelToolSchema {
    singularity_model::ModelToolSchema {
        name: "task_stop".to_string(),
        description: "Stop this background command and all descendants, then wait for cleanup. Stopping an already finished command returns its existing result.".to_string(),
        parameters_schema: json!({
            "type": "object", "properties": { "task_id": { "type": "string" } },
            "required": ["task_id"], "additionalProperties": false
        }),
    }
}
