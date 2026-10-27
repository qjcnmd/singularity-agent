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
    /// SKILL.md 的完整路径。
    pub path: String,
}

/// 一份文本文件的路径和内容：技能入口或 AGENTS.md。文件不存在时内容为空。
#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct TextDocument {
    pub path: String,
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
pub struct SkillCatalog {
    pub skills: Vec<SkillMetadata>,
    pub diagnostics: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
pub struct DirectoryPickResult {
    pub path: Option<String>,
}

/// 本轮请求实际加载的指令文件和工具名称，供工作台查看上下文。
#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct InjectedContext {
    pub instructions: Vec<InstructionFile>,
    /// 内置工具名。
    pub tools: Vec<String>,
    /// MCP 服务器提供的工具名。
    pub mcp_tools: Vec<String>,
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
