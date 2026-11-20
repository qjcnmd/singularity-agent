//! 工具清单、工具定义、参数检查和执行分发。

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Arc;

use serde::de::DeserializeOwned;
use serde_json::Value;
use singularity_core::ToolDefinition;
use singularity_core::skills::SkillCatalog;
use tokio_util::sync::CancellationToken;

use super::bash;
use super::edit;
use super::mcp;
use super::read;
use super::skill;
use super::view_image;
use super::write;

pub(crate) const DEFAULT_MAX_LINES: usize = 2000;
pub(crate) const DEFAULT_MAX_BYTES: usize = 50 * 1024;

/// 展示上限的 KB 数值，工具描述与截断提示共用。
pub(crate) const fn default_max_kb() -> usize {
    DEFAULT_MAX_BYTES / 1024
}

/// `read` 与 `bash` 工具说明共用的上限描述。
pub(crate) fn default_cap_summary() -> String {
    format!("{DEFAULT_MAX_LINES} lines or {}KB", default_max_kb())
}

/// 工具执行的结果。若工具执行失败（如路径不存在或命令返回非零），通过 `is_error: true` 反馈给模型。
#[derive(Debug)]
pub(crate) struct ToolExecution {
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
pub(crate) struct ToolDisplay {
    pub output: String,
    pub background_task_id: Option<String>,
}

impl ToolExecution {
    pub(crate) fn text(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            display: None,
            images: Vec::new(),
            is_error: false,
            duration_ms: None,
            read_source: None,
        }
    }

    pub(crate) fn with_read_source(mut self, source: singularity_protocol::ReadSource) -> Self {
        self.read_source = Some(source);
        self
    }
}

/// 工具执行时需要的工作目录、会话输出目录、停止信号和后台命令管理器。
pub(crate) struct ToolContext {
    pub cwd: std::path::PathBuf,
    pub output_directory: std::path::PathBuf,
    pub signal: CancellationToken,
    pub bash_tasks: Arc<bash::BashTasks>,
}

/// 参数已检查并解析好的工具调用。
pub(crate) enum PreparedTool {
    Edit(edit::EditArgs),
    Write(write::WriteArgs),
    Read(read::ReadArgs),
    ViewImage(view_image::ViewImageArgs),
    Skill(skill::SkillArgs, Arc<SkillCatalog>),
    Question(super::question::QuestionArgs),
    Bash(bash::BashArgs),
    TaskList(bash::TaskListArgs),
    TaskOutput(bash::TaskIdArgs),
    TaskStop(bash::TaskIdArgs),
    McpSchema(Vec<singularity_mcp::McpTool>),
    Mcp(singularity_mcp::McpTool, serde_json::Map<String, Value>),
}

impl PreparedTool {
    /// 执行工具；失败时返回 `is_error = true` 的结果，交给模型处理。
    pub(crate) async fn execute<F: Future<Output = ()> + Send>(
        self,
        context: ToolContext,
        on_update: impl FnMut(String) -> F + Send,
    ) -> ToolExecution {
        let ToolContext {
            cwd,
            output_directory,
            signal,
            bash_tasks,
        } = context;
        match self {
            Self::Edit(args) => Box::pin(edit::execute(args, cwd, signal)).await,
            Self::Write(args) => Box::pin(write::execute(args, cwd, signal)).await,
            Self::McpSchema(tools) => mcp::execute_schema(&tools),
            Self::Mcp(tool, args) => Box::pin(mcp::execute(tool, args, output_directory, &signal)).await,
            Self::Bash(args) => {
                Box::pin(bash_tasks.execute(args, cwd, output_directory, signal, on_update)).await
            }
            Self::TaskList(_) => bash_tasks.tool_list(),
            Self::TaskOutput(args) => bash_tasks.output(&args.task_id, &signal),
            Self::TaskStop(args) => Box::pin(bash_tasks.tool_stop(&args.task_id)).await,
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
pub(crate) type BuiltinTool = (ToolDefinition, ToolParser);

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
            deserialize_args_or_error(args).map(PreparedTool::TaskOutput)
        }),
        (bash::task_stop_spec(), |args| deserialize_args_or_error(args).map(PreparedTool::TaskStop)),
    ]
}

pub(crate) fn read_tool() -> BuiltinTool {
    (read::spec(), |args| deserialize_args_or_error(args).map(PreparedTool::Read))
}

pub(crate) fn edit_tool() -> BuiltinTool {
    (edit::spec(), |args| deserialize_args_or_error(args).map(PreparedTool::Edit))
}

pub(crate) fn write_tool() -> BuiltinTool {
    (write::spec(), |args| deserialize_args_or_error(args).map(PreparedTool::Write))
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

/// 本轮可用的工具：本任务开着的内置工具，以及按服务器归类、统一按工具粒度延迟加载的 MCP 工具。
/// 发给模型的工具定义、参数检查和执行分发都在这里。
#[derive(Default)]
pub(crate) struct ToolRegistrySnapshot {
    tools: Vec<BuiltinTool>,
    /// 本轮已连接且有可用工具的 MCP 服务器，统一通过 `mcp_schema` / `mcp_call` 按需访问。
    mcp_servers: BTreeMap<String, Vec<singularity_mcp::McpTool>>,
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

    pub(crate) fn add_mcp_server(&mut self, server_id: String, tools: Vec<singularity_mcp::McpTool>) {
        if !tools.is_empty() {
            self.mcp_servers.insert(server_id, tools);
        }
    }

    /// 发给模型的完整工具定义：内置工具在前；存在可用 MCP 服务器时固定附带 `mcp_schema` / `mcp_call` 代理工具。
    pub(crate) fn provider_schemas(&self) -> Vec<ToolDefinition> {
        let mcp_proxies = (!self.mcp_servers.is_empty())
            .then(|| [mcp::schema_spec(), mcp::call_spec()])
            .into_iter()
            .flatten();
        self.tools
            .iter()
            .map(|(schema, _)| schema.clone())
            .chain(self.skills.iter().map(|_| skill::spec()))
            .chain(mcp_proxies)
            .collect()
    }

    /// 查找工具并解析参数，不执行。Agent 在执行前按模型给出的顺序逐个调用；
    /// 未知工具和参数错误都返回给模型看的错误结果。
    pub(crate) fn preflight(&self, name: &str, args: &Value) -> Result<PreparedTool, ToolExecution> {
        if !self.mcp_servers.is_empty() {
            if name == mcp::MCP_SCHEMA_TOOL_NAME {
                let args: mcp::McpSchemaArgs = deserialize_args_or_error(args)?;
                let tools = mcp::lookup_tools(&self.mcp_servers, &args.server, &args.tools)?;
                return Ok(PreparedTool::McpSchema(tools));
            }
            if name == mcp::MCP_CALL_TOOL_NAME {
                let args: mcp::McpCallArgs = deserialize_args_or_error(args)?;
                let tool = mcp::lookup_tool(&self.mcp_servers, &args.server, &args.tool)?;
                return Ok(PreparedTool::Mcp(tool.clone(), args.arguments));
            }
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

pub(crate) fn error_result(message: impl Into<String>) -> ToolExecution {
    let mut execution = ToolExecution::text(message);
    execution.is_error = true;
    execution
}

/// 解析工具参数；失败时返回给模型看的错误结果，调用方直接把它当作工具结果。
pub(crate) fn deserialize_args_or_error<T: DeserializeOwned>(args: &Value) -> Result<T, ToolExecution> {
    T::deserialize(args).map_err(|error| error_result(format!("invalid tool arguments: {error}")))
}
