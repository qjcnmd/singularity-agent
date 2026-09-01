//! 工具注册表快照：名称 → ToolSpec 的单一事实源，参数校验、schema 派生与
//! 恢复重放分类的共同 owner。

use std::collections::BTreeMap;
use std::path::Path;

use serde::de::DeserializeOwned;
use serde_json::Value;
use singularity_core::CancellationToken;
use singularity_model::{ModelToolSchema, ProviderProtocolContract};

use super::bash;
use super::edit;
use super::glob;
use super::grep;
use super::read;
use super::write;
pub use crate::session::ToolReplayClass;

/// 一次工具执行的模型可见结果。工具自身失败（路径不存在、参数非法、
/// 取消等）一律以 `is_error=true` 的结果表达，不进入任何错误通道。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolExecution {
    pub content: String,
    pub is_error: bool,
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
}

/// preflight 要么产出可执行工具，要么产出模型可见的拒绝执行；
/// 未知工具名同样以模型可见拒绝收尾，不进入任何错误通道。
#[derive(Debug)]
pub(crate) enum ToolPreflight {
    Ready(PreparedTool),
    Rejected(ToolExecution),
}

/// 工具执行上下文：参数、会话工作区（构造时绑定）、中断信号、流式输出回调。
pub struct ExecuteContext<'a> {
    pub cwd: &'a Path,
    pub signal: Option<&'a CancellationToken>,
    pub on_update: Option<&'a mut dyn FnMut(&str)>,
}

/// 取消时向模型可见的失败文案；全仓唯一来源，工具不得自行拼写。
pub(crate) const ABORTED_MESSAGE: &str = "Operation aborted";

impl ExecuteContext<'_> {
    /// 取消信号已触发时返回模型可见的 abort 失败结果；未触发返回 `None`。
    /// 工具在入口与耗时段落后统一调用它检查取消，避免各工具自行判断。
    pub(crate) fn abort_if_cancelled(&self) -> Option<ToolExecution> {
        self.signal
            .filter(|signal| signal.is_cancelled())
            .map(|_| ToolExecution {
                content: ABORTED_MESSAGE.to_string(),
                is_error: true,
            })
    }
}

/// 工具规格：模型可见的名称/描述/JSON Schema（parameters）、恢复重放分类，
/// 以及真实的参数解析+执行绑定（preflight 阶段 typed 解析一次）。
#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub name: &'static str,
    pub description: &'static str,
    pub parameters: Value,
    /// 恢复重放分类：`never` 调用在结果未知时绝不自动重放。
    pub replay: ToolReplayClass,
    pub(crate) prepare: fn(&Value) -> Result<PreparedTool, ToolExecution>,
}

/// 一次 turn 冻结的工具注册表快照；`new()` 注册默认工具集
/// （read/glob/grep/bash/edit/write）。提示词名单、provider schema、参数
/// 校验、执行分发与重放分类全部出自本快照，不存在第二处派生。
#[derive(Debug, Default)]
pub struct ToolRegistrySnapshot {
    tools: BTreeMap<&'static str, ToolSpec>,
}

impl ToolRegistrySnapshot {
    /// 创建注册表并注册默认工具（read/glob/grep/bash/edit/write）。
    pub fn new() -> Self {
        let mut registry = Self::default();
        registry.insert_fixed(read::spec());
        registry.insert_fixed(glob::spec());
        registry.insert_fixed(grep::spec());
        registry.insert_fixed(bash::spec());
        registry.insert_fixed(edit::spec());
        registry.insert_fixed(write::spec());
        registry
    }

    fn insert_fixed(&mut self, spec: ToolSpec) {
        self.tools.insert(spec.name, spec);
    }

    pub fn get(&self, name: &str) -> Option<&ToolSpec> {
        self.tools.get(name)
    }

    /// 已注册工具名（确定性排序）。
    pub fn names(&self) -> Vec<&'static str> {
        self.tools.keys().copied().collect()
    }

    /// 提示词工具名单（与 schema 同源，按注册表确定性排序）。
    pub fn prompt_tool_names(&self) -> Vec<String> {
        self.names().into_iter().map(str::to_string).collect()
    }

    /// provider 请求 schema 投影：按能力声明的工具数上限截断。
    pub fn provider_schemas(
        &self,
        capabilities: &ProviderProtocolContract,
    ) -> Vec<ModelToolSchema> {
        self.names()
            .into_iter()
            .filter_map(|name| self.get(name))
            .take(capabilities.max_tools_per_request as usize)
            .map(|spec| ModelToolSchema {
                name: spec.name.to_string(),
                description: spec.description.to_string(),
                parameters_schema: spec.parameters.clone(),
            })
            .collect()
    }

    /// 工具调用的恢复重放分类；未知工具名按 `never` 收敛（fail-closed）。
    pub fn replay_class(&self, name: &str) -> ToolReplayClass {
        self.tools
            .get(name)
            .map(|spec| spec.replay)
            .unwrap_or(ToolReplayClass::Never)
    }

    /// 查找并解析调用而不执行。Agent 批次在按模型给定 source order 执行
    /// 每个调用前使用本方法；typed 反序列化在此完成一次。未知工具名与
    /// 参数解析失败都以模型可见拒绝收尾。
    pub(crate) fn preflight(&self, name: &str, args: &Value) -> ToolPreflight {
        let Some(spec) = self.tools.get(name) else {
            return ToolPreflight::Rejected(ToolExecution {
                content: format!("tool execution failed: unknown tool: {name}"),
                is_error: true,
            });
        };
        match (spec.prepare)(args) {
            Ok(prepared) => ToolPreflight::Ready(prepared),
            Err(execution) => ToolPreflight::Rejected(execution),
        }
    }

    /// 执行一个已通过 [`Self::preflight`] 的调用。
    pub(crate) fn execute_prepared<'a>(
        &self,
        prepared: PreparedTool,
        ctx: ExecuteContext<'a>,
    ) -> ToolExecution {
        match prepared {
            PreparedTool::Read(args) => read::execute(&args, ctx),
            PreparedTool::Glob(args) => glob::execute(&args, ctx),
            PreparedTool::Grep(args) => grep::execute(&args, ctx),
            PreparedTool::Bash(args) => bash::execute(&args, ctx),
            PreparedTool::Edit(args) => edit::execute(&args, ctx),
            PreparedTool::Write(args) => write::execute(&args, ctx),
        }
    }
}

/// 工具失败结果（is_error=true）的构造捷径。
pub(crate) fn error_result(message: impl Into<String>) -> ToolExecution {
    ToolExecution {
        content: message.into(),
        is_error: true,
    }
}

/// 反序列化工具参数；失败时把错误文本包装为模型可见的 `is_error` 结果。
/// 调用方把返回的失败结果直接作为工具执行结果透传，例如：
///
/// ```ignore
/// let args = match deserialize_args_or_error::<MyArgs>(&raw_args) {
///     Ok(args) => args,
///     Err(execution) => return execution,
/// };
/// ```
pub(crate) fn deserialize_args_or_error<T: DeserializeOwned>(
    args: &Value,
) -> Result<T, ToolExecution> {
    serde_json::from_value(args.clone()).map_err(|error| ToolExecution {
        content: format!("invalid tool arguments: {error}"),
        is_error: true,
    })
}
