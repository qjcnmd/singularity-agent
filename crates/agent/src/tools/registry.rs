//! 工具注册表的快照：工具描述与 schema、参数预检和执行分派。

use std::path::Path;

use serde::de::DeserializeOwned;
use serde_json::Value;
use singularity_model::ModelToolSchema;
use tokio_util::sync::CancellationToken;

use super::bash;
use super::edit;
use super::glob;
use super::grep;
use super::read;
use super::write;

/// 一次工具执行的模型可见结果。工具自身的失败（路径不存在、参数非法、被取消等）
/// 一律用 is_error=true 的结果表达，不走任何错误通道。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolExecution {
    pub content: String,
    /// 实际的文件改动，供展示和历史使用；不进入模型输入。
    pub diff: Option<String>,
    pub is_error: bool,
    /// 由批次所有者计量的墙钟耗时，不发给模型。
    pub duration_ms: Option<u64>,
    /// read 实际读到的源文件范围；只有 read 的成功结果会带，展示层据此编号。
    pub read_source: Option<singularity_protocol::ReadSource>,
}

impl ToolExecution {
    /// 构造成功的纯文本结果；耗时由工具批次统一结算。
    pub fn text(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
            diff: None,
            duration_ms: None,
            read_source: None,
        }
    }

    /// 附上文件差异。
    pub fn with_diff(mut self, diff: String) -> Self {
        self.diff = Some(diff);
        self
    }

    pub fn with_read_source(mut self, source: singularity_protocol::ReadSource) -> Self {
        self.read_source = Some(source);
        self
    }
}

/// 工具批次开始前完成查找与参数解析（preflight）的结果。用静态枚举派发，闭包不分配堆内存。
#[derive(Debug, Clone)]
pub(crate) enum PreparedTool {
    Read(read::ReadArgs),
    Glob(glob::GlobArgs),
    Grep(grep::GrepArgs),
    Bash(bash::BashArgs),
    Edit(edit::EditArgs),
    Write(write::WriteArgs),
}

impl PreparedTool {
    /// 只有只读工具可以重叠执行；变更类工具和 shell 命令是屏障。
    pub(crate) fn supports_parallel(&self) -> bool {
        match self {
            Self::Read(_) | Self::Glob(_) | Self::Grep(_) => true,
            Self::Bash(_) | Self::Edit(_) | Self::Write(_) => false,
        }
    }

    /// 执行 ToolRegistrySnapshot::preflight 准备好的调用。失败也作为模型可见的
    /// 结果返回；唯一的错误通道仍然是 ToolExecution::is_error。
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
        }
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
        self.signal
            .is_cancelled()
            .then(|| error_result(ABORTED_MESSAGE))
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

/// 一次 turn 内冻结的工具注册表快照；Default 会注册默认工具集
/// （read/glob/grep/bash/edit/write）。提示词名单、provider schema、参数
/// 校验和执行分发都由本模块维护；哪些调用可以并行由 PreparedTool 决定。
#[derive(Debug)]
pub struct ToolRegistrySnapshot {
    tools: Vec<(ToolSpec, ToolParser)>,
    pub(crate) skills: singularity_core::skills::SkillCatalog,
}

impl Default for ToolRegistrySnapshot {
    /// 唯一的初始化定义：默认注册表就是内置工具集本身，所以「默认注册表」广告出去的
    /// schema、提示词名单和可执行的分发三者含义一致。
    fn default() -> Self {
        Self {
            skills: Default::default(),
            tools: vec![
                (bash::spec(), |args| {
                    deserialize_args_or_error(args).map(PreparedTool::Bash)
                }),
                (edit::spec(), |args| {
                    deserialize_args_or_error(args).map(PreparedTool::Edit)
                }),
                (glob::spec(), |args| {
                    deserialize_args_or_error(args).map(PreparedTool::Glob)
                }),
                (grep::spec(), |args| {
                    deserialize_args_or_error(args).map(PreparedTool::Grep)
                }),
                (read::spec(), |args| {
                    deserialize_args_or_error(args).map(PreparedTool::Read)
                }),
                (write::spec(), |args| {
                    deserialize_args_or_error(args).map(PreparedTool::Write)
                }),
            ],
        }
    }
}

impl ToolRegistrySnapshot {
    /// Developer 指令用的工具名单：(名称, 一行简介)。顺序确定，且与 provider schema
    /// 出自同一份快照。
    pub fn prompt_lines(&self) -> Vec<(&'static str, &'static str)> {
        self.tools
            .iter()
            .map(|(spec, _)| (spec.name, spec.snippet))
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
            .collect()
    }

    /// 只查找并解析调用，不执行。Agent 批次在派发 worker 之前，按模型给出的
    /// source order 逐项调用它；带类型的反序列化在这里只做一次。未知工具名和
    /// 参数解析失败，都以模型可见的拒绝收尾。
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
        parse(args)
    }
}

/// 构造工具失败结果（is_error=true）的捷径。
pub(crate) fn error_result(message: impl Into<String>) -> ToolExecution {
    ToolExecution {
        content: message.into(),
        is_error: true,
        diff: None,
        duration_ms: None,
        read_source: None,
    }
}

/// 反序列化工具参数；失败时把错误文本包成模型可见的 is_error 结果，调用方把它当作
/// 工具执行结果直接透传。直接用借来的 JSON 作 Deserializer，不复制中间的 Value。
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
