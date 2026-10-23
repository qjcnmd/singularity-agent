//! 技能列表、文件搜索、目录选择，以及当前回合加载的指令与工具名称。
//! 供工作台展示和补全使用。

use serde::Serialize;

/// 输入框里的技能候选；选中时只插入名称，路径由 Agent 消费输入时解析。
#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct SkillMetadata {
    pub name: String,
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
pub struct SkillCatalog {
    pub skills: Vec<SkillMetadata>,
    pub diagnostics: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
pub struct FileCandidate {
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
pub struct DirectoryPickResult {
    pub path: Option<String>,
}

/// 本轮请求实际加载的指令文件、技能名称和工具名称，供工作台查看上下文。
#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct InjectedContext {
    pub instructions: Vec<InstructionFile>,
    pub skills: Vec<String>,
    pub tools: Vec<String>,
}

/// 当前上下文实际加载的全局或项目指令文件。
#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct InstructionFile {
    pub path: String,
    pub content: String,
    pub global: bool,
}
