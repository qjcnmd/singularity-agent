//! 对外公开的检查载荷：provider 的重放状态和文件系统的具体实现类型留在各自
//! 模块内，任意工具的参数保持 JSON。

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 请求里内嵌的检查载荷：system/developer 消息、工具定义和本次请求的偏好。请求身份由所属的
/// `RequestObservation`（或展示它的历史条目）承载，这里不再复制一份 id，也不构成独立的生命周期。
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

/// 技能候选在界面上可见的摘要。调用方只需要能显示的名称和描述；文件身份和
/// 调用标志留在 core 的 Skill 里，服务端已经按 user_invocable 过滤过，不必过线。
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
