//! Bash 与后台任务工具的参数和说明。

use std::{num::NonZeroU64, sync::LazyLock};

use serde::Deserialize;
use serde_json::json;

use crate::tools::registry::{ToolExecution, ToolSpec, error_result};

pub(super) const DEFAULT_TIMEOUT_MS: u64 = 300_000;
pub(super) const MAX_WAIT_MS: u64 = 30_000;

static DESCRIPTION: LazyLock<String> = LazyLock::new(|| {
    format!(
        "Execute a Bash command in the current working directory (Git Bash on Windows). Use POSIX syntax; `2>nul` creates a file named nul. Returns stdout and stderr, truncated to the last {}; full truncated output is saved to a file. Independent commands may run in parallel; send dependent commands in order. Each command starts exactly once. run_in_background returns immediately; otherwise commands still running after yield_time_ms continue in the background. Use task_list, task_output and task_stop to manage them. Background completion notifies you automatically: do not repeatedly poll. timeout_ms limits total execution time, not just foreground waiting. Foreground calls default to {DEFAULT_TIMEOUT_MS} ms; explicit background calls have no default timeout. Stopping a task terminates its entire process tree. No interactive stdin or PTY is available.",
        crate::tools::truncate::default_cap_summary()
    )
});

/// 已解析的 Bash 调用；由工具注册表检查等待时间。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BashArgs {
    pub(crate) command: String,
    #[serde(default)]
    pub(crate) description: Option<String>,
    #[serde(default)]
    pub(crate) timeout_ms: Option<NonZeroU64>,
    #[serde(default)]
    pub(crate) run_in_background: bool,
    #[serde(default = "default_yield_time_ms")]
    pub(crate) yield_time_ms: NonZeroU64,
}

impl BashArgs {
    pub(crate) fn validate(self) -> Result<Self, ToolExecution> {
        if self.yield_time_ms.get() > MAX_WAIT_MS {
            return Err(error_result("yield_time_ms must be between 1 and 30000"));
        }
        Ok(self)
    }

    pub(super) fn total_timeout_ms(&self) -> Option<u64> {
        self.timeout_ms
            .map(NonZeroU64::get)
            .or_else(|| (!self.run_in_background).then_some(DEFAULT_TIMEOUT_MS))
    }
}

fn default_yield_time_ms() -> NonZeroU64 {
    NonZeroU64::new(10_000).expect("default wait is positive")
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

pub(crate) fn spec() -> ToolSpec {
    ToolSpec {
        name: "bash",
        snippet: "Run commands, optionally continuing in the background",
        description: &DESCRIPTION,
        parameters: json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "Bash command to execute" },
                "description": { "type": "string", "minLength": 1, "description": "Short description of this command" },
                "timeout_ms": { "type": "integer", "minimum": 1, "description": "Total execution timeout in milliseconds" },
                "run_in_background": { "type": "boolean", "default": false },
                "yield_time_ms": { "type": "integer", "minimum": 1, "maximum": MAX_WAIT_MS, "default": 10000 }
            },
            "required": ["command", "description"],
            "additionalProperties": false
        }),
    }
}

pub(crate) fn task_list_spec() -> ToolSpec {
    ToolSpec {
        name: "task_list",
        snippet: "List this session's background commands and their states",
        description: "List actual background commands in this session. Completion notifies you automatically; do not repeatedly poll.",
        parameters: json!({ "type": "object", "properties": {}, "additionalProperties": false }),
    }
}

pub(crate) fn task_output_spec() -> ToolSpec {
    ToolSpec {
        name: "task_output",
        snippet: "Read a background command's accumulated output and state",
        description: "Read the bounded accumulated output tail and current state. wait_ms waits for completion for at most 30000 ms; it does not restart the command or reset its timeout. Completion notifies you automatically, so avoid short polling loops.",
        parameters: json!({
            "type": "object",
            "properties": {
                "task_id": { "type": "string" },
                "wait_ms": { "type": "integer", "minimum": 0, "maximum": MAX_WAIT_MS, "default": 0 }
            },
            "required": ["task_id"], "additionalProperties": false
        }),
    }
}

pub(crate) fn task_stop_spec() -> ToolSpec {
    ToolSpec {
        name: "task_stop",
        snippet: "Stop a background command and wait for its process tree to exit",
        description: "Stop this background command and all descendants, then wait for cleanup. Stopping an already finished command returns its existing result.",
        parameters: json!({
            "type": "object", "properties": { "task_id": { "type": "string" } },
            "required": ["task_id"], "additionalProperties": false
        }),
    }
}
