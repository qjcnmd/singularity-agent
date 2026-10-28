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

/// 本轮请求实际加载的各上下文模块，供工作台查看上下文。
#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct InjectedContext {
    pub modules: Vec<ContextModule>,
}

/// 新建任务页上的一个上下文胶囊：一类指令、一组工具、Skills 或一个 MCP 服务器。
#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct ContextModule {
    pub id: String,
    pub group: ContextGroup,
    pub label: String,
    /// 悬停说明：用途，以及包含的工具名、指令文件或连接错误。
    pub detail: String,
    /// 请求里这部分指令和工具定义的估算 Token 数；MCP 服务器尚未连接时为 0。
    pub tokens: u64,
    /// 基础能力，不能关闭。
    pub locked: bool,
    /// 本任务是否开着；新建任务页的预览里全部为 true。
    pub enabled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum ContextGroup {
    Instructions,
    Tools,
    Skills,
    Mcp,
}
