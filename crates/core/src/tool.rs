use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters_schema: Value,
}

pub fn estimate_tool_tokens<'a>(tools: impl IntoIterator<Item = &'a ToolDefinition>) -> u64 {
    let tools: Vec<_> = tools.into_iter().collect();
    if tools.is_empty() {
        return 0;
    }
    crate::estimate_tokens_of(&serde_json::to_string(&tools).expect("tool definitions are serializable")) + 4
}
