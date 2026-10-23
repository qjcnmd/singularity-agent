//! 对外公开的检查载荷：provider 的重放状态和文件系统的具体实现类型留在各自
//! 模块内，任意工具的参数保持 JSON。

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

/// 本进程最近一次实际装配的请求前缀；技能和工具仅供名称浏览。
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
