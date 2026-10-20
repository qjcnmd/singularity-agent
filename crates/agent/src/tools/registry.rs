//! 工具注册表的快照：工具描述与 schema、参数预检和执行分派。

use std::future::Future;

use serde::de::DeserializeOwned;
use serde_json::Value;
use singularity_model::ModelToolSchema;
use tokio_util::sync::CancellationToken;

use super::bash;
use super::read;

/// 工具调用的执行结果。若工具执行失败（如路径不存在或命令返回非零），通过 `is_error: true` 反馈给模型。
#[derive(Debug, Clone)]
pub struct ToolExecution {
    pub content: String,
    pub images: Vec<crate::image::InputImage>,
    pub is_error: bool,
    /// 工具执行耗时（毫秒，仅供本地展示与统计）
    pub duration_ms: Option<u64>,
    /// read 工具读到的源文件行号范围（供前端按行展示）
    pub read_source: Option<singularity_protocol::ReadSource>,
}

impl ToolExecution {
    /// 构造成功的纯文本结果。
    pub fn text(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
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

/// 当前工具调用的目录、取消范围和后台任务归属。
pub(crate) struct ToolContext {
    pub cwd: std::path::PathBuf,
    pub signal: CancellationToken,
    pub bash_tasks: std::sync::Arc<bash::BashTasks>,
    pub turn_id: String,
    pub item_id: String,
}

/// 预检并解析参数后的待执行工具实例。
#[derive(Debug)]
pub(crate) enum PreparedTool {
    Read(read::ReadArgs),
    Question(super::question::QuestionArgs),
    Bash(bash::BashArgs),
    TaskList(bash::TaskListArgs),
    TaskOutput(bash::TaskOutputArgs),
    TaskStop(bash::TaskIdArgs),
    Mcp(singularity_mcp::McpTool, serde_json::Map<String, Value>),
}

impl PreparedTool {
    /// 独立的读文件、命令和任务管理调用共用调度器的并行位。
    pub(crate) fn supports_parallel(&self) -> bool {
        matches!(
            self,
            Self::Read(_) | Self::Bash(_) | Self::TaskList(_) | Self::TaskOutput(_) | Self::TaskStop(_)
        )
    }

    /// 执行预检完毕的工具调用，执行失败转换为 `ToolExecution::is_error = true`。
    pub(crate) async fn execute<F: Future<Output = ()> + Send>(
        self,
        context: ToolContext,
        on_update: impl FnMut(String) -> F + Send,
    ) -> ToolExecution {
        let ToolContext {
            cwd,
            signal,
            bash_tasks,
            turn_id,
            item_id,
        } = context;
        match self {
            Self::Mcp(tool, args) => super::mcp::execute(tool, args, &signal).await,
            Self::Bash(args) => bash_tasks.execute(args, cwd, turn_id, item_id, signal, on_update).await,
            Self::TaskList(_) => ToolExecution::text(
                serde_json::to_string(&bash_tasks.list()).expect("background tasks serialize"),
            ),
            Self::TaskOutput(args) => bash_tasks.output(&args.task_id, args.wait_ms, &signal).await,
            Self::TaskStop(args) => bash_tasks.stop(&args.task_id).await,
            Self::Read(args) => tokio::task::spawn_blocking(move || read::execute(&args, &cwd, &signal))
                .await
                .expect("tool worker completes while the runtime is running"),
            Self::Question(_) => unreachable!("questions execute on the turn control plane"),
        }
    }
}

/// 取消时给模型看的失败文案，各工具共用。
pub(crate) const ABORTED_MESSAGE: &str = "Operation aborted";

/// 模型可见的工具元数据；参数解析见 [`ToolRegistrySnapshot::preflight`]。
#[derive(Debug, Clone)]
pub(crate) struct ToolSpec {
    pub name: &'static str,
    /// 系统提示词的工具名单里跟在名称后面的一行简介，是模型选工具时的第一层依据；
    /// 完整约束放在 description 里，随 schema 一起下发。
    pub snippet: &'static str,
    pub description: &'static str,
    pub parameters: Value,
}

type ToolParser = fn(&Value) -> Result<PreparedTool, ToolExecution>;

/// 回合开始及压缩后更新、连续请求间复用的工具注册表快照；Default 注册默认工具集
/// （bash、后台任务管理、read、ask_user_question）。提示词名单、provider schema、参数
/// 校验和执行分发都由本模块维护；哪些调用可以并行由 PreparedTool 决定。
#[derive(Debug)]
pub(crate) struct ToolRegistrySnapshot {
    tools: Vec<(ToolSpec, ToolParser)>,
    mcp: Vec<singularity_mcp::McpTool>,
}

impl Default for ToolRegistrySnapshot {
    fn default() -> Self {
        Self {
            mcp: Vec::new(),
            tools: vec![
                (bash::spec(), |args| {
                    deserialize_args_or_error::<bash::BashArgs>(args)?.validate().map(PreparedTool::Bash)
                }),
                (bash::task_list_spec(), |args| deserialize_args_or_error(args).map(PreparedTool::TaskList)),
                (bash::task_output_spec(), |args| {
                    deserialize_args_or_error::<bash::TaskOutputArgs>(args)?
                        .validate()
                        .map(PreparedTool::TaskOutput)
                }),
                (bash::task_stop_spec(), |args| deserialize_args_or_error(args).map(PreparedTool::TaskStop)),
                (read::spec(), |args| deserialize_args_or_error(args).map(PreparedTool::Read)),
                (super::question::spec(), |args| {
                    deserialize_args_or_error::<super::question::QuestionArgs>(args)?
                        .validate()
                        .map(PreparedTool::Question)
                }),
            ],
        }
    }
}

impl ToolRegistrySnapshot {
    /// Developer 指令用的工具名单：(名称, 一行简介)。顺序确定，且与 provider schema
    /// 出自同一份快照。
    pub fn prompt_lines(&self) -> Vec<(&str, &str)> {
        self.tools
            .iter()
            .map(|(spec, _)| (spec.name, spec.snippet))
            .chain(self.mcp.iter().map(|tool| (tool.name.as_str(), "MCP server tool")))
            .collect()
    }

    /// 从当前注册表派生出发给 provider 的完整请求 schema。
    pub fn provider_schemas(&self) -> Vec<ModelToolSchema> {
        self.tools
            .iter()
            .map(|(spec, _)| ModelToolSchema {
                name: spec.name.to_string(),
                description: spec.description.to_string(),
                parameters_schema: spec.parameters.clone(),
            })
            .chain(self.mcp.iter().map(|tool| ModelToolSchema {
                name: tool.name.clone(),
                description: tool.description.clone(),
                parameters_schema: tool.input_schema.clone(),
            }))
            .collect()
    }

    /// 只查找并解析调用，不执行。Agent 在派发任务之前，按模型给出的
    /// source order 逐项调用它；带类型的反序列化在这里只做一次。未知工具名和
    /// 参数解析失败，都以模型可见的拒绝收尾。
    pub(crate) fn preflight(&self, name: &str, args: &Value) -> Result<PreparedTool, ToolExecution> {
        if let Some(tool) = self.mcp.iter().find(|tool| tool.name == name) {
            let args =
                args.as_object().ok_or_else(|| error_result("MCP tool arguments must be a JSON object"))?;
            return Ok(PreparedTool::Mcp(tool.clone(), args.clone()));
        }
        let (_, parse) =
            self.tools.iter().find(|(spec, _)| spec.name == name).ok_or_else(|| unknown_tool(name))?;
        parse(args)
    }

    pub(crate) fn set_mcp_tools(&mut self, tools: Vec<singularity_mcp::McpTool>) {
        self.mcp = tools;
    }
}

/// 构造工具失败结果（is_error=true）的捷径。
pub(crate) fn error_result(message: impl Into<String>) -> ToolExecution {
    let mut execution = ToolExecution::text(message);
    execution.is_error = true;
    execution
}

/// 固定定义和本轮目录中的未知工具使用同一种拒绝结果。
pub(crate) fn unknown_tool(name: &str) -> ToolExecution {
    error_result(format!("tool execution failed: unknown tool: {name}"))
}

/// 反序列化工具参数；失败时把错误文本包成模型可见的 is_error 结果，调用方把它当作
/// 工具执行结果直接透传。直接用借来的 JSON 作 Deserializer，不复制中间的 Value。
pub(crate) fn deserialize_args_or_error<T: DeserializeOwned>(args: &Value) -> Result<T, ToolExecution> {
    T::deserialize(args).map_err(|error| error_result(format!("invalid tool arguments: {error}")))
}
