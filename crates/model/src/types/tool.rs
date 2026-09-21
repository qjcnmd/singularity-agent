use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use singularity_protocol::RequestTool as ModelToolSchema;

/// 一次模型工具调用。参数只保存一份原始值，合法性由工具预检环节拒绝。
///
/// 全仓唯一的工具调用载荷：会话日志、公开历史条目和模型请求都用它。序列化字段沿用日志与公开
/// 事件已有的形状（`id` / `name` / `args`，与 `HistoryItem::ToolCall` 一致）；发给提供方的
/// 线上格式由 Chat/Responses 编码器各自构造，不从这个类型派生。
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
