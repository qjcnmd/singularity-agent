use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 模型发起的一次工具调用；参数保存原始 JSON，执行前由工具注册表检查。
///
/// 会话日志、历史条目和模型请求都用这个类型，日志中的字段名是 `id` / `name` / `args`。
/// 发给提供方的格式由 Chat / Responses 编码器各自生成。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelToolCall {
    #[serde(rename = "id")]
    pub tool_call_id: String,
    #[serde(rename = "name")]
    pub tool_name: String,
    #[serde(rename = "args")]
    pub arguments: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<super::ProviderBlockMetadata>,
}
