//! 工具清单、工具定义、参数检查和执行分发。

use std::future::Future;
use std::sync::Arc;

use serde::de::DeserializeOwned;
use serde_json::Value;
use singularity_core::skills::SkillCatalog;
use singularity_model::ModelToolSchema;
use tokio_util::sync::CancellationToken;

use super::bash;
use super::read;
use super::skill;
use super::view_image;

/// 工具执行的结果。若工具执行失败（如路径不存在或命令返回非零），通过 `is_error: true` 反馈给模型。
#[derive(Debug)]
pub struct ToolExecution {
    pub content: String,
    /// 本地展示信息；未设置时显示 content，不编码到模型请求。
    pub display: Option<Box<ToolDisplay>>,
    pub images: Vec<crate::image::InputImage>,
    pub is_error: bool,
    /// 工具执行耗时（毫秒，仅供本地展示与统计）
    pub duration_ms: Option<u64>,
    /// read 工具读到的源文件行号范围（供前端按行展示）
    pub read_source: Option<singularity_protocol::ReadSource>,
}

/// 工具结果的本地展示正文及关联的后台命令。
#[derive(Debug)]
pub struct ToolDisplay {
    pub output: String,
    pub background_task_id: Option<String>,
}

impl ToolExecution {
    /// 构造成功的纯文本结果。
    pub fn text(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            display: None,
            images: Vec::new(),
            is_error: false,
            duration_ms: None,
            read_source: None,
        }
    }

    pub fn with_read_source(mut self, source: singularity_protocol::ReadSource) -> Self {
        self.read_source = Some(source);
        self
    }
}

/// 工具执行时需要的工作目录、停止信号和后台命令管理器。
pub(crate) struct ToolContext {
    pub cwd: std::path::PathBuf,
    pub signal: CancellationToken,
    pub bash_tasks: Arc<bash::BashTasks>,
}

/// 参数已检查并解析好的工具调用。
pub(crate) enum PreparedTool {
    Read(read::ReadArgs),
    ViewImage(view_image::ViewImageArgs),
    Skill(skill::SkillArgs, Arc<SkillCatalog>),
    Question(super::question::QuestionArgs),
    Bash(bash::BashArgs),
    TaskList(bash::TaskListArgs),
    TaskOutput(bash::TaskOutputArgs),
    TaskStop(bash::TaskIdArgs),
    Mcp(singularity_mcp::McpTool, serde_json::Map<String, Value>),
}

impl PreparedTool {
    /// 执行工具；失败时返回 `is_error = true` 的结果，交给模型处理。
    pub(crate) async fn execute<F: Future<Output = ()> + Send>(
        self,
        context: ToolContext,
        on_update: impl FnMut(String) -> F + Send,
    ) -> ToolExecution {
        let ToolContext { cwd, signal, bash_tasks } = context;
        match self {
            Self::Mcp(tool, args) => super::mcp::execute(tool, args, &signal).await,
            Self::Bash(args) => bash_tasks.execute(args, cwd, signal, on_update).await,
            Self::TaskList(_) => bash_tasks.tool_list(),
            Self::TaskOutput(args) => {
                bash_tasks.output(&args.task_id, args.cursor, args.wait_ms, &signal).await
            }
            Self::TaskStop(args) => bash_tasks.stop(&args.task_id).await.unwrap_or_else(error_result),
            Self::Read(args) => tokio::task::spawn_blocking(move || read::execute(&args, &cwd, &signal))
                .await
                .expect("tool worker completes while the runtime is running"),
            Self::ViewImage(args) => {
                tokio::task::spawn_blocking(move || view_image::execute(&args, &cwd, &signal))
                    .await
                    .expect("tool worker completes while the runtime is running")
            }
            Self::Skill(args, catalog) => {
                tokio::task::spawn_blocking(move || skill::execute(&args, &catalog))
                    .await
                    .expect("tool worker completes while the runtime is running")
            }
            Self::Question(_) => unreachable!("questions execute on the turn control plane"),
        }
    }
}

/// 取消时给模型看的失败文案，各工具共用。
pub(crate) const ABORTED_MESSAGE: &str = "Operation aborted";

type ToolParser = fn(&Value) -> Result<PreparedTool, ToolExecution>;

/// 一个内置工具：发给模型的定义和参数检查。
pub(crate) type BuiltinTool = (ModelToolSchema, ToolParser);

pub(crate) fn bash_tool() -> BuiltinTool {
    (bash::spec(), |args| {
        deserialize_args_or_error::<bash::BashArgs>(args)?.validate().map(PreparedTool::Bash)
    })
}

/// 后台命令的查看、读取输出和停止；说明里与 bash 互相引用，两者一起提供。
pub(crate) fn task_tools() -> Vec<BuiltinTool> {
    vec![
        (bash::task_list_spec(), |args| deserialize_args_or_error(args).map(PreparedTool::TaskList)),
        (bash::task_output_spec(), |args| {
            deserialize_args_or_error::<bash::TaskOutputArgs>(args)?
                .validate()
                .map(PreparedTool::TaskOutput)
        }),
        (bash::task_stop_spec(), |args| deserialize_args_or_error(args).map(PreparedTool::TaskStop)),
    ]
}

pub(crate) fn read_tool() -> BuiltinTool {
    (read::spec(), |args| deserialize_args_or_error(args).map(PreparedTool::Read))
}

pub(crate) fn view_image_tool() -> BuiltinTool {
    (view_image::spec(), |args| deserialize_args_or_error(args).map(PreparedTool::ViewImage))
}

pub(crate) fn question_tool() -> BuiltinTool {
    (super::question::spec(), |args| {
        deserialize_args_or_error::<super::question::QuestionArgs>(args)?
            .validate()
            .map(PreparedTool::Question)
    })
}

/// 本轮可用的工具：本任务开着的内置工具，加上本轮发现的 MCP 工具。发给模型的工具定义、
/// 参数检查和执行分发都在这里。
#[derive(Default)]
pub(crate) struct ToolRegistrySnapshot {
    tools: Vec<BuiltinTool>,
    mcp: Vec<singularity_mcp::McpTool>,
    /// Skills 模块开着时持有本轮的技能目录，`skill` 工具按它加载。
    skills: Option<Arc<SkillCatalog>>,
}

impl ToolRegistrySnapshot {
    pub(crate) fn add_builtin(&mut self, tools: impl IntoIterator<Item = BuiltinTool>) {
        self.tools.extend(tools);
    }

    pub(crate) fn set_skills(&mut self, catalog: Arc<SkillCatalog>) {
        self.skills = Some(catalog);
    }

    pub(crate) fn add_mcp_tools(&mut self, tools: Vec<singularity_mcp::McpTool>) {
        self.mcp.extend(tools);
    }

    /// 发给模型的完整工具定义：内置工具在前，MCP 工具在后；`skill` 工具随 Skills 模块。
    pub fn provider_schemas(&self) -> Vec<ModelToolSchema> {
        self.tools
            .iter()
            .map(|(schema, _)| schema.clone())
            .chain(self.skills.iter().map(|_| skill::spec()))
            .chain(self.mcp.iter().map(mcp_schema))
            .collect()
    }

    /// 查找工具并解析参数，不执行。Agent 在执行前按模型给出的顺序逐个调用；
    /// 未知工具和参数错误都返回给模型看的错误结果。
    pub(crate) fn preflight(&self, name: &str, args: &Value) -> Result<PreparedTool, ToolExecution> {
        if let Some(tool) = self.mcp.iter().find(|tool| tool.name == name) {
            let args =
                args.as_object().ok_or_else(|| error_result("MCP tool arguments must be a JSON object"))?;
            return Ok(PreparedTool::Mcp(tool.clone(), args.clone()));
        }
        if name == "skill"
            && let Some(catalog) = &self.skills
        {
            return deserialize_args_or_error(args)
                .map(|args| PreparedTool::Skill(args, Arc::clone(catalog)));
        }
        let (_, parse) = self
            .tools
            .iter()
            .find(|(spec, _)| spec.name == name)
            .ok_or_else(|| error_result(format!("tool execution failed: unknown tool: {name}")))?;
        parse(args)
    }
}

fn mcp_schema(tool: &singularity_mcp::McpTool) -> ModelToolSchema {
    ModelToolSchema {
        name: tool.name.clone(),
        description: tool.description.clone(),
        parameters_schema: tool.input_schema.clone(),
    }
}

/// 构造工具失败结果。
pub(crate) fn error_result(message: impl Into<String>) -> ToolExecution {
    let mut execution = ToolExecution::text(message);
    execution.is_error = true;
    execution
}

/// 解析工具参数；失败时返回给模型看的错误结果，调用方直接把它当作工具结果。
pub(crate) fn deserialize_args_or_error<T: DeserializeOwned>(args: &Value) -> Result<T, ToolExecution> {
    T::deserialize(args).map_err(|error| error_result(format!("invalid tool arguments: {error}")))
}
