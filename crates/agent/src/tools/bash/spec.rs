//! bash 参数解析与工具规格。

use serde::{Deserialize, Deserializer, de::Error as _};
use serde_json::{Value, json};

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
        description: super::DESCRIPTION,
        parameters: json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "Bash command to execute" },
                "timeout_ms": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Timeout in milliseconds for this command (default: 300000)"
                },
            },
            "required": ["command"],
            "additionalProperties": false,
        }),
    }
}
