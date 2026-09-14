use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use singularity_protocol::RequestTool as ModelToolSchema;

/// 模型工具调用。参数只持一份值；不合法的参数由工具 preflight 拒绝。
///
/// 这是工具调用载荷在全仓的唯一表示：会话日志、公开历史条目与模型请求都复用它。
/// 序列化名沿用日志与公开事件的既有形状（`id` / `name` / `args`，与
/// `HistoryItem::ToolCall` 一致）；provider 的 wire 形状由 Chat/Responses 编码器
/// 各自构造，不经此派生。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelToolCall {
    #[serde(rename = "id")]
    pub tool_call_id: String,
    #[serde(rename = "name")]
    pub tool_name: String,
    #[serde(rename = "args")]
    pub arguments: Value,
}
