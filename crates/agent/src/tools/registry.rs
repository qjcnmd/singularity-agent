//! 工具注册表：名称 → ToolSpec 的单一事实源，以及参数校验。

use std::collections::BTreeMap;
use std::fmt::{Display, Formatter};
use std::path::{Path, PathBuf};

use serde_json::Value;
use singularity_core::CancellationToken;

use super::bash;
use super::edit;
use super::read;
use super::write;

/// 一次工具执行的模型可见结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolExecution {
    pub content: String,
    pub is_error: bool,
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
    pub execute: for<'a> fn(ExecuteContext<'a>) -> Result<ToolExecution, ToolError>,
}

/// 名称 → 工具规格的注册表；`new()` 注册默认工具集（read/bash/edit/write）。
#[derive(Debug, Default)]
pub struct ToolRegistry {
    tools: BTreeMap<&'static str, ToolSpec>,
}

impl ToolRegistry {
    /// 创建注册表并注册默认工具（read/bash/edit/write）。
    pub fn new() -> Self {
        let mut registry = Self::default();
        registry.register(read::spec());
        registry.register(bash::spec());
        registry.register(edit::spec());
        registry.register(write::spec());
        registry
    }

    /// 注册工具。重复名称是编程错误，直接 panic（调用方必须使用唯一名称）。
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
        let spec = self
            .tools
            .get(name)
            .ok_or_else(|| ToolError(format!("unknown tool: {name}")))?;
        if let Err(message) = validate_arguments(&spec.parameters, &ctx.args) {
            return Ok(ToolExecution {
                content: format!("tool arguments failed validation: {message}"),
                is_error: true,
            });
        }
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

/// 相对路径绑定到会话工作区，绝对路径原样使用。
/// 本层不做 workspace 边界约束（Phase 3 的沙箱/权限范围）。
pub(crate) fn resolve_path(cwd: &Path, path: &str) -> PathBuf {
    let candidate = Path::new(path);
    if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        cwd.join(candidate)
    }
}

/// 按 JSON Schema（properties/required/type）做最小校验，对齐 Pi `validateToolArguments`
/// 语义：缺必填参数、类型不匹配、未知参数都视为参数校验失败。
fn validate_arguments(parameters: &Value, args: &Value) -> Result<(), String> {
    let Some(object) = args.as_object() else {
        return Err("tool arguments must be a JSON object".to_string());
    };
    let properties = parameters.get("properties").and_then(Value::as_object);
    if let Some(required) = parameters.get("required").and_then(Value::as_array) {
        for name in required.iter().filter_map(Value::as_str) {
            if !object.contains_key(name) {
                return Err(format!("missing required parameter \"{name}\""));
            }
        }
    }
    for (name, value) in object {
        let Some(schema) = properties.and_then(|properties| properties.get(name)) else {
            return Err(format!("unknown parameter \"{name}\""));
        };
        let expected = schema.get("type").and_then(Value::as_str).unwrap_or("any");
        if !json_value_matches_type(value, expected) {
            return Err(format!("parameter \"{name}\" must be of type {expected}"));
        }
    }
    Ok(())
}

fn json_value_matches_type(value: &Value, expected: &str) -> bool {
    match expected {
        "string" => value.is_string(),
        "number" => value.is_number(),
        "integer" => value.is_i64() || value.is_u64(),
        "boolean" => value.is_boolean(),
        "object" => value.is_object(),
        "array" => value.is_array(),
        "null" => value.is_null(),
        _ => true,
    }
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
            execute: ping_execute,
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
    #[should_panic(expected = "tool already registered")]
    fn duplicate_registration_is_rejected() {
        let mut registry = ToolRegistry::new();
        registry.register(ping_spec());
        registry.register(ping_spec());
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
    fn missing_required_parameter_is_error() {
        let registry = ToolRegistry::new();
        let result = registry
            .execute("bash", context(json!({}), Path::new(".")))
            .expect("execute");
        assert!(result.is_error);
        assert!(
            result
                .content
                .contains("missing required parameter \"command\"")
        );
    }

    #[test]
    fn wrong_parameter_type_is_error() {
        let registry = ToolRegistry::new();
        let result = registry
            .execute("bash", context(json!({ "command": 42 }), Path::new(".")))
            .expect("execute");
        assert!(result.is_error);
        assert!(
            result
                .content
                .contains("parameter \"command\" must be of type string")
        );
    }

    #[test]
    fn unknown_parameter_is_error() {
        let registry = ToolRegistry::new();
        let result = registry
            .execute(
                "read",
                context(json!({ "path": "a.txt", "bogus": 1 }), Path::new(".")),
            )
            .expect("execute");
        assert!(result.is_error);
        assert!(result.content.contains("unknown parameter \"bogus\""));
    }

    #[test]
    fn non_object_arguments_are_error() {
        let registry = ToolRegistry::new();
        let result = registry
            .execute("bash", context(json!("oops"), Path::new(".")))
            .expect("execute");
        assert!(result.is_error);
        assert!(result.content.contains("must be a JSON object"));
    }
}
