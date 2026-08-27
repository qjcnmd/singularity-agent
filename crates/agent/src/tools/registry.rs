//! 工具注册表：名称 → ToolSpec 的单一事实源，以及参数校验。

use std::collections::BTreeMap;
use std::fmt::{Display, Formatter};
use std::path::Path;

use serde::de::DeserializeOwned;
use serde_json::Value;
use singularity_core::CancellationToken;

use super::bash;
use super::edit;
use super::glob;
use super::grep;
use super::read;
use super::write;

/// 一次工具执行的模型可见结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolExecution {
    pub content: String,
    pub is_error: bool,
}

/// 工具批次开始前执行查找与参数解析 preflight 的结果。
#[derive(Clone)]
pub struct PreparedTool {
    execute: std::sync::Arc<PreparedExecute>,
}

impl std::fmt::Debug for PreparedTool {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("PreparedTool")
    }
}

/// 已解析参数与执行体的类型擦除容器。
type PreparedExecute =
    dyn for<'a> Fn(ExecuteContext<'a>) -> Result<ToolExecution, ToolError> + Send + Sync;

impl PreparedTool {
    /// 绑定已解析参数与执行函数：`prepare` 阶段 typed 反序列化一次，
    /// 执行阶段直接用解析结果，不再二次反序列化。
    pub(crate) fn from_parsed<A, F>(args: A, execute: F) -> Self
    where
        A: Send + Sync + 'static,
        F: for<'a> Fn(&A, ExecuteContext<'a>) -> Result<ToolExecution, ToolError>
            + Send
            + Sync
            + 'static,
    {
        Self {
            execute: std::sync::Arc::new(move |ctx| execute(&args, ctx)),
        }
    }
}

/// preflight 要么产出可执行工具，要么产出模型可见的拒绝。
/// 未知名称保持为注册表错误，使调用方能保留该边界。
#[derive(Debug)]
pub enum ToolPreflight {
    Ready(PreparedTool),
    Rejected(ToolExecution),
}

/// 注册表/内部层错误（如未知工具名）。工具自身的失败一律走 `ToolExecution::is_error`，
/// 不进入该错误通道。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolError(pub String);

impl Display for ToolError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for ToolError {}

/// 工具执行上下文：参数、会话工作区（构造时绑定）、中断信号、流式输出回调。
pub struct ExecuteContext<'a> {
    pub args: Value,
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

/// 工具规格：模型可见的名称/描述/JSON Schema（parameters），以及真实的
/// 参数解析+执行绑定（preflight 阶段 typed 解析一次）。
#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub name: &'static str,
    pub description: &'static str,
    pub parameters: Value,
    pub(crate) prepare: fn(&Value) -> Result<PreparedTool, ToolExecution>,
}

/// 名称 → 工具规格的注册表；`new()` 注册默认工具集（read/glob/grep/bash/edit/write）。
#[derive(Debug, Default)]
pub struct ToolRegistry {
    tools: BTreeMap<&'static str, ToolSpec>,
}

impl ToolRegistry {
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

    /// 注册工具。重复名称是编程错误，直接 panic（调用方必须使用唯一名称）。
    #[cfg(test)]
    pub fn register(&mut self, spec: ToolSpec) {
        if self.tools.contains_key(spec.name) {
            panic!("tool already registered: {}", spec.name);
        }
        self.tools.insert(spec.name, spec);
    }

    pub fn get(&self, name: &str) -> Option<&ToolSpec> {
        self.tools.get(name)
    }

    /// 已注册工具名（确定性排序）。
    pub fn names(&self) -> Vec<&'static str> {
        self.tools.keys().copied().collect()
    }

    /// 执行工具：先按 `parameters` JSON Schema 校验参数，校验失败返回 is_error；
    /// 未知工具名返回 `Err(ToolError)`。
    pub fn execute<'a>(
        &self,
        name: &str,
        ctx: ExecuteContext<'a>,
    ) -> Result<ToolExecution, ToolError> {
        match self.preflight(name, &ctx.args)? {
            ToolPreflight::Ready(prepared) => self.execute_prepared(prepared, ctx),
            ToolPreflight::Rejected(execution) => Ok(execution),
        }
    }

    /// 查找并解析调用而不执行。Agent 批次在按模型给定 source order 执行
    /// 每个调用前使用本方法；typed 反序列化在此完成一次。
    pub fn preflight(&self, name: &str, args: &Value) -> Result<ToolPreflight, ToolError> {
        let spec = self
            .tools
            .get(name)
            .ok_or_else(|| ToolError(format!("unknown tool: {name}")))?;
        match (spec.prepare)(args) {
            Ok(prepared) => Ok(ToolPreflight::Ready(prepared)),
            Err(execution) => Ok(ToolPreflight::Rejected(execution)),
        }
    }

    /// 执行一个已通过 [`Self::preflight`] 的调用。
    pub fn execute_prepared<'a>(
        &self,
        prepared: PreparedTool,
        ctx: ExecuteContext<'a>,
    ) -> Result<ToolExecution, ToolError> {
        (prepared.execute)(ctx)
    }
}

/// 工具失败结果（is_error=true）的构造捷径。
pub(crate) fn error_result(message: impl Into<String>) -> Result<ToolExecution, ToolError> {
    Ok(ToolExecution {
        content: message.into(),
        is_error: true,
    })
}

/// 相对路径解析绑定到当前工作区目录，绝对路径保持原样。
pub(crate) use super::path::resolve_path;

pub(crate) fn deserialize_args<T: DeserializeOwned>(args: &Value) -> Result<T, String> {
    serde_json::from_value(args.clone()).map_err(|error| format!("invalid tool arguments: {error}"))
}

/// 工具 `prepare` 的统一实现：preflight 阶段 typed 反序列化一次，把解析
/// 结果与执行函数绑定进 [`PreparedTool`]；失败时生成模型可见的拒绝。
pub(crate) fn prepare_typed<A, F>(raw: &Value, execute: F) -> Result<PreparedTool, ToolExecution>
where
    A: DeserializeOwned + Send + Sync + 'static,
    F: for<'a> Fn(&A, ExecuteContext<'a>) -> Result<ToolExecution, ToolError>
        + Send
        + Sync
        + 'static,
{
    let args = deserialize_args_or_error::<A>(raw)?;
    Ok(PreparedTool::from_parsed(args, execute))
}

/// 反序列化工具参数；失败时把错误文本包装为模型可见的 `is_error` 结果。
/// 调用方把返回的失败结果直接作为工具执行结果透传，例如：
///
/// ```ignore
/// let args = match deserialize_args_or_error::<MyArgs>(&raw_args) {
///     Ok(args) => args,
///     Err(execution) => return Ok(execution),
/// };
/// ```
pub(crate) fn deserialize_args_or_error<T: DeserializeOwned>(
    args: &Value,
) -> Result<T, ToolExecution> {
    deserialize_args::<T>(args).map_err(|message| ToolExecution {
        content: message,
        is_error: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::test_support::context;
    use serde_json::json;

    fn ping_spec() -> ToolSpec {
        ToolSpec {
            name: "ping",
            description: "custom test tool",
            parameters: json!({ "type": "object", "properties": {}, "required": [] }),
            prepare: |_| {
                Ok(PreparedTool::from_parsed((), |(), _ctx| {
                    Ok(ToolExecution {
                        content: "pong".to_string(),
                        is_error: false,
                    })
                }))
            },
        }
    }

    #[test]
    fn default_tools_include_read_bash_edit_write() {
        let registry = ToolRegistry::new();
        for expected in ["read", "bash", "edit", "write"] {
            assert!(registry.names().contains(&expected), "missing {expected}");
            assert!(
                registry.get(expected).is_some(),
                "missing spec for {expected}"
            );
        }
    }

    #[test]
    fn register_and_query_custom_tool() {
        let mut registry = ToolRegistry::new();
        registry.register(ping_spec());
        assert!(registry.get("ping").is_some());
        assert!(registry.names().contains(&"ping"));
        let result = registry
            .execute("ping", context(json!({}), Path::new(".")))
            .expect("execute");
        assert_eq!(result.content, "pong");
        assert!(!result.is_error);
    }

    #[test]
    fn unknown_tool_is_err() {
        let registry = ToolRegistry::new();
        assert!(
            registry
                .execute("nope", context(json!({}), Path::new(".")))
                .is_err()
        );
    }

    #[test]
    fn typed_preflight_rejects_zero_bash_timeout() {
        let registry = ToolRegistry::new();
        assert!(matches!(
            registry
                .preflight("bash", &json!({"command": "echo no", "timeout_ms": 0}))
                .expect("known tool"),
            ToolPreflight::Rejected(_)
        ));
    }
}
