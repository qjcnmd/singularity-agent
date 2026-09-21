//! bash 的参数解析与工具规格：参数结构、JSON schema、说明文本和默认超时都在这里，
//! 执行环（exec）只消费这里的参数和默认值，声明与执行之间是单向依赖。

use std::sync::LazyLock;

use serde::{Deserialize, Deserializer, de::Error as _};
use serde_json::{Value, json};

/// 没有显式给出 timeout_ms 时生效的执行上限：一次工具调用不能无限期占住整个 turn，
/// 否则模型既拿不到反馈，也没法收尾。到点后终止整棵进程树，并把已捕获的输出连同
/// 原因一起返回给模型；需要更久的命令就显式传更大的 timeout_ms（该参数不设上限）。
/// 这个取值覆盖了实测中最长的合法单次调用（数百秒的测试套件），只拦住不会返回的计算。
pub(crate) const DEFAULT_TIMEOUT_MS: u64 = 300_000;

pub(crate) static DESCRIPTION: LazyLock<String> = LazyLock::new(|| {
    format!(
        "Execute a bash command in the current working directory. Returns stdout and stderr. The command runs under bash (Git Bash on Windows); every call starts in the working directory, so changing directory first is unnecessary. Use POSIX syntax: `2>/dev/null` discards stderr, whereas `2>nul` and `>nul` create a real file named `nul` in the workspace. Output is truncated to last {} (whichever is hit first); when truncated, the full output is saved to a temp file and its path is appended as a `Full output:` line. Commands are bounded by {DEFAULT_TIMEOUT_MS} ms unless timeout_ms says otherwise; a bounded command is terminated and its output so far is returned, so pass a larger timeout_ms for long-running work. On Windows, all descendant processes are terminated when this tool call ends, including processes started with & or nohup. Run tests and other work in the foreground within one call; a later call cannot wait for a background process from an earlier call.",
        crate::tools::truncate::default_cap_summary()
    )
});

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BashArgs {
    pub(crate) command: String,
    #[serde(default, deserialize_with = "deserialize_timeout_ms")]
    pub(crate) timeout_ms: Option<u64>,
}

fn deserialize_timeout_ms<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    value
        .as_u64()
        .filter(|timeout| *timeout > 0)
        .map(Some)
        .ok_or_else(|| D::Error::custom("invalid timeout_ms: must be a positive integer"))
}

pub(crate) fn spec() -> super::super::registry::ToolSpec {
    super::super::registry::ToolSpec {
        name: "bash",
        snippet: "Execute bash commands (ls, grep, find, etc.)",
        description: &DESCRIPTION,
        parameters: json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "Bash command to execute" },
                "timeout_ms": {
                    "type": "integer",
                    "minimum": 1,
                    "description": format!("Timeout in milliseconds for this command (default: {DEFAULT_TIMEOUT_MS})")
                },
            },
            "required": ["command"],
            "additionalProperties": false,
        }),
    }
}
