use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use singularity_protocol::RequestTool as ModelToolSchema;

/// 已解析的模型 tool call，以及原始参数和校验结果。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelToolCall {
    pub tool_call_id: String,
    pub tool_name: String,
    pub arguments: Value,
    pub raw_arguments: String,
    pub validation_errors: Vec<String>,
}
