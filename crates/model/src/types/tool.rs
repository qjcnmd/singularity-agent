use serde::{Deserialize, Serialize};
use serde_json::Value;

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
