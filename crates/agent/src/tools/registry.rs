//! 工具注册表：名称 → ToolSpec 的单一事实源，以及参数校验。

use std::collections::BTreeMap;
use std::fmt::{Display, Formatter};
use std::path::{Path, PathBuf};

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

/// Result of the lookup and argument-validation preflight performed before a
/// tool batch starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PreparedTool {
    pub(crate) name: &'static str,
}

/// Preflight either produces an executable tool or a model-visible rejection.
/// Unknown names remain registry errors so callers can preserve that boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
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

/// 工具规格：模型可见的名称/描述/JSON Schema（parameters），以及真实执行函数。
#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub name: &'static str,
    pub description: &'static str,
    pub parameters: Value,
    pub(crate) validate: fn(&Value) -> Result<(), String>,
    pub execute: for<'a> fn(ExecuteContext<'a>) -> Result<ToolExecution, ToolError>,
}

/// 名称 → 工具规格的注册表；`new()` 注册默认工具集（read/bash/edit/write）。
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

    /// Lookup and validate a call without executing it. Agent batches use this
    /// before executing each call in model-given source order.
    pub fn preflight(&self, name: &str, args: &Value) -> Result<ToolPreflight, ToolError> {
        let spec = self
            .tools
            .get(name)
            .ok_or_else(|| ToolError(format!("unknown tool: {name}")))?;
        if let Err(message) = (spec.validate)(args) {
            return Ok(ToolPreflight::Rejected(ToolExecution {
                content: format!("tool arguments failed validation: {message}"),
                is_error: true,
            }));
        }
        Ok(ToolPreflight::Ready(PreparedTool { name: spec.name }))
    }

    /// Execute a call that has already passed [`Self::preflight`].
    pub fn execute_prepared<'a>(
        &self,
        prepared: PreparedTool,
        ctx: ExecuteContext<'a>,
    ) -> Result<ToolExecution, ToolError> {
        let spec = self
            .tools
            .get(prepared.name)
            .ok_or_else(|| ToolError(format!("unknown tool: {}", prepared.name)))?;
        (spec.execute)(ctx)
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
pub(crate) fn resolve_path(cwd: &Path, path: &str) -> PathBuf {
    let candidate = Path::new(path);
    if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        cwd.join(candidate)
    }
}

pub(crate) fn deserialize_args<T: DeserializeOwned>(args: &Value) -> Result<T, String> {
    serde_json::from_value(args.clone()).map_err(|error| format!("invalid tool arguments: {error}"))
}

pub(crate) fn validate_args<T: DeserializeOwned>(args: &Value) -> Result<(), String> {
    deserialize_args::<T>(args).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::test_support::context;
    use serde_json::json;

    fn ping_execute(_ctx: ExecuteContext<'_>) -> Result<ToolExecution, ToolError> {
        Ok(ToolExecution {
            content: "pong".to_string(),
            is_error: false,
        })
    }

    fn ping_spec() -> ToolSpec {
        ToolSpec {
            name: "ping",
            description: "custom test tool",
            parameters: json!({ "type": "object", "properties": {}, "required": [] }),
            validate: validate_args::<EmptyArgs>,
            execute: ping_execute,
        }
    }

    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct EmptyArgs {}

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
