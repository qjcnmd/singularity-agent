//! 工具注册表的快照：工具描述与 schema、参数预检和执行分派。

use std::path::Path;

use serde::de::DeserializeOwned;
use serde_json::Value;
use singularity_model::ModelToolSchema;
use tokio_util::sync::CancellationToken;

use super::bash;
use super::read;

/// 一次工具执行的模型可见结果。工具自身的失败（路径不存在、参数非法、被取消等）
/// 一律用 is_error=true 的结果表达，不走任何错误通道。
#[derive(Debug, Clone)]
pub struct ToolExecution {
    pub content: String,
    pub images: Vec<crate::image::InputImage>,
    pub is_error: bool,
    /// 由派发者计量的墙钟耗时，不发给模型。
    pub duration_ms: Option<u64>,
    /// read 实际读到的源文件范围；只有 read 的成功结果会带，展示层据此编号。
    pub read_source: Option<singularity_protocol::ReadSource>,
}

impl ToolExecution {
    /// 构造成功的纯文本结果；耗时由派发者结算。
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

/// 执行前完成查找与参数解析（preflight）的结果。用静态枚举派发，闭包不分配堆内存。
#[derive(Debug)]
pub(crate) enum PreparedTool {
    Read(read::ReadArgs),
    Question(super::question::QuestionArgs),
    Bash(bash::BashArgs),
    Mcp(singularity_mcp::McpTool, serde_json::Map<String, Value>),
}

impl PreparedTool {
    /// 只有只读工具可以重叠执行；变更类工具和 shell 命令是屏障。
    pub(crate) fn supports_parallel(&self) -> bool {
        matches!(self, Self::Read(_))
    }

    /// 执行 ToolRegistrySnapshot::preflight 准备好的调用。失败也作为模型可见的
    /// 结果返回；唯一的错误通道仍然是 ToolExecution::is_error。
    pub(crate) async fn execute(
        self,
        cwd: std::path::PathBuf,
        signal: CancellationToken,
        mut on_update: impl FnMut(String) + Send + 'static,
    ) -> ToolExecution {
        if let Self::Mcp(tool, args) = self {
            return super::mcp::execute(tool, args, &signal).await;
        }
        tokio::task::spawn_blocking(move || {
            let ctx = ExecuteContext {
                cwd: &cwd,
                signal: &signal,
                on_update: &mut on_update,
            };
            if let Some(aborted) = ctx.abort_if_cancelled() {
                return aborted;
            }
            match &self {
                Self::Read(args) => read::execute(args, ctx),
                Self::Bash(args) => bash::execute(args, ctx),
                Self::Question(_) => unreachable!("questions execute on the turn control plane"),
                Self::Mcp(..) => unreachable!("MCP tools execute asynchronously"),
            }
        })
        .await
        .expect("tool worker completes while the runtime is running")
    }
}

/// 工具执行的上下文：工作目录、中断信号和流式输出回调。
pub(crate) struct ExecuteContext<'a> {
    pub cwd: &'a Path,
    pub signal: &'a CancellationToken,
    /// 流式进度回调接收 owned 文本。捕获方每次更新本来就会产生新字符串，这里直接
    /// 移交所有权，省掉在借用边界上回拷整段输出。
    pub on_update: &'a mut dyn FnMut(String),
}

/// 取消时给模型看的失败文案。全仓只有这一处来源，各工具不得自己拼一份。
pub(crate) const ABORTED_MESSAGE: &str = "Operation aborted";

impl ExecuteContext<'_> {
    /// 取消信号已触发时返回模型可见的 abort 失败结果，未触发则返回 None。
    /// 各工具在入口和耗时步骤之后统一调用它检查取消，不必自己判断。
    pub(crate) fn abort_if_cancelled(&self) -> Option<ToolExecution> {
        self.signal.is_cancelled().then(|| error_result(ABORTED_MESSAGE))
    }
}

/// 模型可见的工具元数据：名称、一行简介、描述和 JSON Schema（parameters）。
/// 参数解析与执行分发不在这个结构上，分别由 [`ToolRegistrySnapshot::preflight`]
/// 和 [`PreparedTool::execute`] 承担。
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
/// （bash/read）。提示词名单、provider schema、参数
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
                (bash::spec(), |args| deserialize_args_or_error(args).map(PreparedTool::Bash)),
                (read::spec(), |args| deserialize_args_or_error(args).map(PreparedTool::Read)),
            ],
        }
    }
}

impl ToolRegistrySnapshot {
    pub(crate) fn enable_questions(&mut self) {
        self.tools.push((super::question::spec(), |args| {
            deserialize_args_or_error::<super::question::QuestionArgs>(args)?
                .validate()
                .map(PreparedTool::Question)
        }));
    }

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
        let (_, parse) = self
            .tools
            .iter()
            .find(|(spec, _)| spec.name == name)
            .ok_or_else(|| error_result(format!("tool execution failed: unknown tool: {name}")))?;
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

/// 反序列化工具参数；失败时把错误文本包成模型可见的 is_error 结果，调用方把它当作
/// 工具执行结果直接透传。直接用借来的 JSON 作 Deserializer，不复制中间的 Value。
pub(crate) fn deserialize_args_or_error<T: DeserializeOwned>(args: &Value) -> Result<T, ToolExecution> {
    T::deserialize(args).map_err(|error| error_result(format!("invalid tool arguments: {error}")))
}
