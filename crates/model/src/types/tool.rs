use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
/// 模型 tool call 的解析状态。
pub enum ModelToolParseStatus {
    Valid,
    InvalidJson,
    SchemaMismatch,
}

/// 一个可执行 tool 面向模型提供方暴露的模式。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelToolSchema {
    pub name: String,
    pub description: String,
    pub parameters_schema: Value,
}

/// 已解析的模型 tool call，以及原始参数和校验结果。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelToolCall {
    pub tool_call_id: String,
    pub tool_name: String,
    pub arguments: Value,
    pub raw_arguments: String,
    pub parse_status: ModelToolParseStatus,
    pub validation_errors: Vec<String>,
}
