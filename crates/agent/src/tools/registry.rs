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
    /// 供展示与历史的实际文件改动；不进入模型输入。
    pub diff: Option<String>,
    pub is_error: bool,
    /// 由批次所有者计量的墙钟执行耗时，不发送给模型。
    pub duration_ms: Option<u64>,
    /// read 真实读取到的源文件范围；只有 read 的成功结果携带，展示层据此编号。
    pub read_source: Option<singularity_protocol::ReadSource>,
}

impl ToolExecution {
    /// 成功的纯文本结果；耗时由工具批次结算。
    pub fn text(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
            diff: None,
            duration_ms: None,
            read_source: None,
        }
    }

    /// 附上展示与历史使用的文件差异。
    pub fn with_diff(mut self, diff: String) -> Self {
        self.diff = Some(diff);
        self
    }

    /// 附上实际读取的源文件范围。
    pub fn with_read_source(mut self, source: singularity_protocol::ReadSource) -> Self {
        self.read_source = Some(source);
        self
    }
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
    /// 仅只读工具可以重叠执行。变更与 shell 命令是屏障。
    pub(crate) fn supports_parallel(&self) -> bool {
        match self {
            Self::Read(_) | Self::Glob(_) | Self::Grep(_) | Self::Skill(_) => true,
            Self::Bash(_) | Self::Edit(_) | Self::Write(_) => false,
        }
    }

    /// 执行由 ToolRegistrySnapshot::preflight 准备好的调用。失败是模型可见的
    /// 结果；唯一的错误通道仍是 ToolExecution::is_error。
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
                Ok(content) => ToolExecution::text(content),
                Err(error) => error_result(error),
            },
        }
    }
}

/// 工具执行上下文：工作目录、中断信号与流式输出回调。
pub(crate) struct ExecuteContext<'a> {
    pub cwd: &'a Path,
    pub signal: &'a CancellationToken,
    /// 流式进度回调接收 owned 文本：捕获方每次更新本就产生新字符串，
    /// 这里直接移交所有权，避免在借用边界回拷整段输出。
    pub on_update: Option<&'a mut dyn FnMut(String)>,
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

/// 模型可见的工具元数据：名称、一行简介、描述与 JSON Schema（parameters）。
/// 参数解析与执行分发不在本结构上，由 [`ToolRegistrySnapshot::preflight`] 与
/// [`PreparedTool::execute`] 承担。
#[derive(Debug, Clone)]
pub(crate) struct ToolSpec {
    pub name: &'static str,
    /// 系统提示词工具名单里跟随名称的一行简介（模型选工具的第一层依据；
    /// 完整约束在 description 随 schema 下发）。
    pub snippet: &'static str,
    pub description: &'static str,
    pub parameters: Value,
}

type ToolParser =
    fn(&singularity_core::skills::SkillCatalog, &Value) -> Result<PreparedTool, ToolExecution>;

/// 一次 turn 冻结的工具注册表快照；Default 注册默认工具集
/// （read/glob/grep/bash/edit/write/skill）。提示词名单、provider schema、参数
/// 校验和执行分发由本模块维护；PreparedTool 决定哪些调用可以并行。
#[derive(Debug)]
pub struct ToolRegistrySnapshot {
    tools: Vec<(ToolSpec, ToolParser)>,
    pub(crate) skills: singularity_core::skills::SkillCatalog,
}

impl Default for ToolRegistrySnapshot {
    /// 唯一的初始化定义：默认注册表就是内置工具集本身，因此“默认注册表”的
    /// 广告 schema、提示词名单与可执行分发含义一致。
    fn default() -> Self {
        Self {
            skills: Default::default(),
            tools: vec![
                (bash::spec(), |_, args| {
                    deserialize_args_or_error(args).map(PreparedTool::Bash)
                }),
                (edit::spec(), |_, args| {
                    deserialize_args_or_error(args).map(PreparedTool::Edit)
                }),
                (glob::spec(), |_, args| {
                    deserialize_args_or_error(args).map(PreparedTool::Glob)
                }),
                (grep::spec(), |_, args| {
                    deserialize_args_or_error(args).map(PreparedTool::Grep)
                }),
                (read::spec(), |_, args| {
                    deserialize_args_or_error(args).map(PreparedTool::Read)
                }),
                (write::spec(), |_, args| {
                    deserialize_args_or_error(args).map(PreparedTool::Write)
                }),
                (
                    ToolSpec {
                        name: "skill",
                        snippet: "Load a skill's complete instructions and resource directory.",
                        description: "Load a skill by exact name from the available skills catalog. Follow its instructions for the current task. This reads instructions; it does not run scripts automatically.",
                        parameters: serde_json::json!({"type":"object","properties":{"name":{"type":"string","description":"Exact skill name from the catalog"}},"required":["name"],"additionalProperties":false}),
                    },
                    |skills, args| {
                        #[derive(serde::Deserialize)]
                        #[serde(deny_unknown_fields)]
                        struct Args {
                            name: String,
                        }
                        deserialize_args_or_error::<Args>(args).and_then(|args| {
                            skills
                                .model_invocable()
                                .find(|s| s.name == args.name)
                                .cloned()
                                .map(PreparedTool::Skill)
                                .ok_or_else(|| {
                                    error_result(format!(
                                        "skill unavailable for model invocation: {}",
                                        args.name
                                    ))
                                })
                        })
                    },
                ),
            ],
        }
    }
}

impl ToolRegistrySnapshot {
    /// 系统提示词的工具名单：(名称, 一行简介)，确定性排序，与 provider
    /// schema 出自同一快照。
    pub fn prompt_lines(&self) -> Vec<(&'static str, &'static str)> {
        self.tools
            .iter()
            .map(|(spec, _)| (spec.name, spec.snippet))
            .collect()
    }

    /// 从当前注册表派生完整的 provider 请求 schema。
    pub fn provider_schemas(&self) -> Vec<ModelToolSchema> {
        self.tools
            .iter()
            .map(|(spec, _)| ModelToolSchema {
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
        let (_, parse) = self
            .tools
            .iter()
            .find(|(spec, _)| spec.name == name)
            .ok_or_else(|| error_result(format!("tool execution failed: unknown tool: {name}")))?;
        parse(&self.skills, args)
    }
}

/// 工具失败结果（is_error=true）的构造捷径。
pub(crate) fn error_result(message: impl Into<String>) -> ToolExecution {
    ToolExecution {
        content: message.into(),
        is_error: true,
        diff: None,
        duration_ms: None,
        read_source: None,
    }
}

/// 反序列化工具参数；失败时把错误文本包装为模型可见的 is_error 结果。
/// 直接以借用的 JSON 作为 Deserializer，不复制整棵中间 Value。调用方把返回的
/// 失败结果直接作为工具执行结果透传，例如：
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
    if !args.is_object() {
        return Err(error_result(
            "invalid tool arguments: expected a JSON object",
        ));
    }
    T::deserialize(args).map_err(|error| error_result(format!("invalid tool arguments: {error}")))
}
