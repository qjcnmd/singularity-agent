//! 公开的检查载荷：provider 重放状态与文件系统实现类型留在各自模块内，
//! 任意工具参数保持 JSON。

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 请求内嵌的检查载荷：system/developer 消息、工具定义与本次请求偏好。
/// 请求身份由所属的 `RequestObservation`（或展示它的历史条目）承载，这里不再
/// 复制同一个 id，也不构成独立的请求生命周期。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct ModelRequestSnapshot {
    pub messages: Vec<RequestMessage>,
    pub tools: Vec<RequestTool>,
    pub model_preferences: RequestPreferences,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct RequestMessage {
    pub role: String,
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct RequestTool {
    pub name: String,
    pub description: String,
    pub parameters_schema: Value,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct RequestPreferences {
    pub max_output_tokens: Option<u32>,
}

/// 技能候选的界面可见摘要。调用方只需要可显示的名称与描述；文件身份与
/// 调用标志留在 core 的 Skill，服务端已按 user_invocable 过滤，无需过线。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct SkillMetadata {
    pub name: String,
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
pub struct SkillCatalog {
    pub skills: Vec<SkillMetadata>,
    pub diagnostics: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
pub struct FileCandidate {
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
pub struct DirectoryPickResult {
    pub path: Option<String>,
}
