//! 工具注册表快照：工具描述与 schema、参数预检和执行分派。

use std::path::Path;

use serde::de::DeserializeOwned;
use serde_json::Value;
use singularity_core::CancellationToken;
use singularity_model::ModelToolSchema;

use super::bash;
use super::edit;
use super::glob;
use super::grep;
use super::read;
use super::write;

/// 一次工具执行的模型可见结果。工具自身失败（路径不存在、参数非法、
/// 取消等）一律以 is_error=true 的结果表达，不进入任何错误通道。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolExecution {
    pub content: String,
    /// Actual file changes for display and history; excluded from model input.
    pub diff: Option<String>,
    pub is_error: bool,
    /// Wall-clock execution time measured by the batch owner, not sent to the model.
    pub duration_ms: Option<u64>,
}

/// 工具批次开始前执行查找与参数解析 preflight 的结果（静态枚举派发，零堆分配闭包）。
#[derive(Debug, Clone)]
pub(crate) enum PreparedTool {
    Read(read::ReadArgs),
    Glob(glob::GlobArgs),
    Grep(grep::GrepArgs),
    Bash(bash::BashArgs),
    Edit(edit::EditArgs),
    Write(write::WriteArgs),
    Skill(singularity_core::skills::Skill),
}

impl PreparedTool {
    /// Only read-only tools may overlap. Mutations and shell commands are barriers.
    pub(crate) fn supports_parallel(&self) -> bool {
        matches!(
            self,
            Self::Read(_) | Self::Glob(_) | Self::Grep(_) | Self::Skill(_)
        )
    }

    /// Execute a call prepared by ToolRegistrySnapshot::preflight. Failures are model-visible
    /// results; the only error channel remains ToolExecution::is_error.
    pub(crate) fn execute(&self, ctx: ExecuteContext<'_>) -> ToolExecution {
        if let Some(aborted) = ctx.abort_if_cancelled() {
            return aborted;
        }
        match self {
            Self::Read(args) => read::execute(args, ctx),
            Self::Glob(args) => glob::execute(args, ctx),
            Self::Grep(args) => grep::execute(args, ctx),
            Self::Bash(args) => bash::execute(args, ctx),
            Self::Edit(args) => edit::execute(args, ctx),
            Self::Write(args) => write::execute(args, ctx),
            Self::Skill(skill) => match skill.load() {
                Ok(content) => ToolExecution {
                    content,
                    is_error: false,
                    diff: None,
                    duration_ms: None,
                },
                Err(error) => error_result(error),
            },
        }
    }
}

/// 工具执行上下文：工作目录、中断信号与流式输出回调。
pub(crate) struct ExecuteContext<'a> {
    pub cwd: &'a Path,
    pub signal: &'a CancellationToken,
    pub on_update: Option<&'a mut dyn FnMut(&str)>,
}

/// 取消时向模型可见的失败文案；全仓唯一来源，工具不得自行拼写。
pub(crate) const ABORTED_MESSAGE: &str = "Operation aborted";

impl ExecuteContext<'_> {
    /// 取消信号已触发时返回模型可见的 abort 失败结果；未触发返回 None。
    /// 工具在入口与耗时段落后统一调用它检查取消，避免各工具自行判断。
    pub(crate) fn abort_if_cancelled(&self) -> Option<ToolExecution> {
        self.signal
            .is_cancelled()
            .then(|| error_result(ABORTED_MESSAGE))
    }
}

/// 工具规格：模型可见的名称/一行简介/描述/JSON Schema（parameters），
/// 以及真实的参数解析+执行绑定（preflight 阶段 typed 解析一次）。
#[derive(Debug, Clone)]
pub(crate) struct ToolSpec {
    pub name: &'static str,
    /// 系统提示词工具名单里跟随名称的一行简介（模型选工具的第一层依据；
    /// 完整约束在 description 随 schema 下发）。
    pub snippet: &'static str,
    pub description: &'static str,
    pub parameters: Value,
}

/// 一次 turn 冻结的工具注册表快照；new() 注册默认工具集
/// （read/glob/grep/bash/edit/write/skill）。提示词名单、provider schema、参数
/// 校验和执行分发由本模块维护；PreparedTool 决定哪些调用可以并行。
#[derive(Debug, Default)]
pub struct ToolRegistrySnapshot {
    tools: Vec<ToolSpec>,
    pub(crate) skills: singularity_core::skills::SkillCatalog,
}

impl ToolRegistrySnapshot {
    /// 创建注册表并注册默认工具（read/glob/grep/bash/edit/write/skill）。
    pub fn new() -> Self {
        Self {
            skills: Default::default(),
            tools: vec![
                bash::spec(),
                edit::spec(),
                glob::spec(),
                grep::spec(),
                read::spec(),
                write::spec(),
                ToolSpec {
                    name: "skill",
                    snippet: "Load a skill's complete instructions and resource directory.",
                    description: "Load a skill by exact name from the available skills catalog. Follow its instructions for the current task. This reads instructions; it does not run scripts automatically.",
                    parameters: serde_json::json!({"type":"object","properties":{"name":{"type":"string","description":"Exact skill name from the catalog"}},"required":["name"],"additionalProperties":false}),
                },
            ],
        }
    }

    /// 系统提示词的工具名单：(名称, 一行简介)，确定性排序，与 provider
    /// schema 出自同一快照。
    pub fn prompt_lines(&self) -> Vec<(&'static str, &'static str)> {
        self.tools
            .iter()
            .map(|spec| (spec.name, spec.snippet))
            .collect()
    }

    /// 从当前注册表派生完整的 provider 请求 schema。
    pub fn provider_schemas(&self) -> Vec<ModelToolSchema> {
        self.tools
            .iter()
            .map(|spec| ModelToolSchema {
                name: spec.name.to_string(),
                description: spec.description.to_string(),
                parameters_schema: spec.parameters.clone(),
            })
            .collect()
    }

    /// 查找并解析调用而不执行。Agent 批次在派发 worker 前按模型给定 source
    /// order 逐项调用本方法；typed 反序列化在此完成一次。未知工具名与
    /// 参数解析失败都以模型可见拒绝收尾。
    pub(crate) fn preflight(
        &self,
        name: &str,
        args: &Value,
    ) -> Result<PreparedTool, ToolExecution> {
        match name {
            "read" => deserialize_args_or_error::<read::ReadArgs>(args).map(PreparedTool::Read),
            "glob" => deserialize_args_or_error::<glob::GlobArgs>(args).map(PreparedTool::Glob),
            "grep" => deserialize_args_or_error::<grep::GrepArgs>(args).map(PreparedTool::Grep),
            "bash" => deserialize_args_or_error::<bash::BashArgs>(args).map(PreparedTool::Bash),
            "edit" => deserialize_args_or_error::<edit::EditArgs>(args).map(PreparedTool::Edit),
            "write" => deserialize_args_or_error::<write::WriteArgs>(args).map(PreparedTool::Write),
            "skill" => {
                #[derive(serde::Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Args {
                    name: String,
                }
                deserialize_args_or_error::<Args>(args).and_then(|args| {
                    self.skills
                        .skills
                        .iter()
                        .find(|s| s.name == args.name && !s.disable_model_invocation)
                        .cloned()
                        .map(PreparedTool::Skill)
                        .ok_or_else(|| {
                            error_result(format!(
                                "skill unavailable for model invocation: {}",
                                args.name
                            ))
                        })
                })
            }
            _ => Err(error_result(format!(
                "tool execution failed: unknown tool: {name}"
            ))),
        }
    }
}

/// 工具失败结果（is_error=true）的构造捷径。
pub(crate) fn error_result(message: impl Into<String>) -> ToolExecution {
    ToolExecution {
        content: message.into(),
        is_error: true,
        diff: None,
        duration_ms: None,
    }
}

/// 反序列化工具参数；失败时把错误文本包装为模型可见的 is_error 结果。
/// 调用方把返回的失败结果直接作为工具执行结果透传，例如：
///
/// 示例：
/// let args = match deserialize_args_or_error::<MyArgs>(&raw_args) {
///     Ok(args) => args,
///     Err(execution) => return execution,
/// };
///
pub(crate) fn deserialize_args_or_error<T: DeserializeOwned>(
    args: &Value,
) -> Result<T, ToolExecution> {
    serde_json::from_value(args.clone())
        .map_err(|error| error_result(format!("invalid tool arguments: {error}")))
}
