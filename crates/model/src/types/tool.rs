use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use singularity_protocol::RequestTool as ModelToolSchema;

/// 模型工具调用。参数只持一份值；不合法的参数由工具 preflight 拒绝。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelToolCall {
    pub tool_call_id: String,
    pub tool_name: String,
    pub arguments: Value,
}
