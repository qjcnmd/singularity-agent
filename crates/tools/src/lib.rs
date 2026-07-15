#![forbid(unsafe_code)]

//! 工具模式、工具代理器决策、工作区操作和公开工具结果投影。
//!
//! 工具代理器会在执行边界再次校验面向模型的输入；`WorkspaceTools` 则在任何文件系统副作用前
//! 强制执行工作区和受保护路径规则。

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::{File, Metadata, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use singularity_core::{CancellationToken, contains_sensitive_text};
pub use singularity_sandbox::{
    CommandEnvironmentPolicy, CommandExecutionStatus, CommandRequest, CommandResult,
    CommandSemanticStatus, DEFAULT_COMMAND_TIMEOUT_SECONDS, SandboxBackend,
    SandboxBackendEnforcement, SandboxCapabilities, SandboxFilesystemMode, SandboxNetworkMode,
    command_permission_resource,
};

const REDACTED_TOOL_OUTPUT: &str = "[redacted sensitive tool output]";
const UNKNOWN_TOOL_ERROR: &str = "unknown_tool";
const INVALID_TOOL_ARGUMENTS_ERROR: &str = "invalid_tool_arguments";
const TOOL_DENIED_ERROR: &str = "tool_denied";
const TOOL_APPROVAL_REQUIRED_ERROR: &str = "approval_required";
const TOOL_SANDBOX_UNAVAILABLE_ERROR: &str = "sandbox_unavailable";
const WORKSPACE_MUTATION_NOT_APPROVED: &str = "workspace mutation requires allowed tool decision";
const DUPLICATE_PATCH_TARGET: &str = "patch contains duplicate canonical target";
const MUTATION_TEMP_FILE_ATTEMPTS: usize = 64;
const DEFAULT_READ_MAX_CHARS: usize = 8_192;
const MAX_READ_MAX_CHARS: usize = 1_000_000;
const DEFAULT_LIST_MAX_ENTRIES: usize = 200;
const MAX_LIST_MAX_ENTRIES: usize = 10_000;
const DEFAULT_LIST_MAX_DEPTH: usize = 16;
const MAX_LIST_MAX_DEPTH: usize = 64;
const DEFAULT_GREP_MAX_MATCHES: usize = 200;
const MAX_GREP_MAX_MATCHES: usize = 10_000;
const MAX_COMMAND_TIMEOUT_SECONDS: u64 = 3_600;
const LARGE_OUTPUT_ARTIFACT_THRESHOLD: usize = 4_096;
const DEFAULT_RESULT_PREVIEW_MAX_CHARS: usize = 4_096;
const BINARY_CONTENT_PREVIEW: &str = "[binary content omitted]";
const DIFF_ARTIFACT_PREFIX: &str = "artifact://diff/";
const RESULT_ARTIFACT_PREFIX: &str = "artifact://result/";
const PROTECTED_PATH_EXACT_MARKERS: [&str; 13] = [
    ".aws",
    ".azure",
    ".git",
    ".gnupg",
    ".ssh",
    "credentials",
    "credentials.json",
    "id_dsa",
    "id_ecdsa",
    "id_ed25519",
    "id_rsa",
    "secret",
    "secrets",
];
const PROTECTED_PATH_PREFIXES: [&str; 3] = [".env", "credential", "private-key"];
const PROTECTED_PATH_SUFFIXES: [&str; 4] = [".key", ".pem", ".p12", ".pfx"];
const PROMPT_INJECTION_MARKERS: [&str; 4] = [
    "developer message",
    "ignore previous",
    "reveal hidden",
    "system prompt",
];
pub const BUILTIN_READ_TOOL: &str = "builtin_read";
pub const BUILTIN_LIST_TOOL: &str = "builtin_list";
pub const BUILTIN_GREP_TOOL: &str = "builtin_grep";
pub const BUILTIN_EDIT_TOOL: &str = "builtin_edit";
pub const BUILTIN_PATCH_TOOL: &str = "builtin_patch";
pub const BUILTIN_COMMAND_TOOL: &str = "builtin_command";
static COMMAND_ID_COUNTER: AtomicU64 = AtomicU64::new(0);
static MUTATION_TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// 工具调用可以与其他只读调用并行，还是必须独占运行。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ToolExecutionMode {
    ParallelRead,
    Exclusive,
}

/// 工具到达执行阶段前返回的结构化校验代码。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ToolInputValidationError {
    pub code: String,
}

impl ToolInputValidationError {
    pub fn new(code: impl Into<String>) -> Self {
        Self { code: code.into() }
    }
}

pub type ToolInputValidator = fn(&Value) -> Result<(), ToolInputValidationError>;

#[derive(Clone)]
struct ToolInputBinding {
    model_input: Value,
    execution_input: Value,
}

/// 面向模型提供方的模式，以及独立的校验逻辑和可选的精确输入绑定。
#[derive(Clone)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub execution_mode: ToolExecutionMode,
    model_input_validator: ToolInputValidator,
    execution_input_validator: ToolInputValidator,
    exact_input_bindings: Option<Vec<ToolInputBinding>>,
}

impl fmt::Debug for ToolSpec {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ToolSpec")
            .field("name", &self.name)
            .field("description", &self.description)
            .field("input_schema", &self.input_schema)
            .field("execution_mode", &self.execution_mode)
            .field("exact_model_inputs", &self.exact_model_inputs())
            .finish()
    }
}

impl ToolSpec {
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: Value,
        execution_mode: ToolExecutionMode,
        input_validator: ToolInputValidator,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            input_schema,
            execution_mode,
            model_input_validator: input_validator,
            execution_input_validator: input_validator,
            exact_input_bindings: None,
        }
    }

    pub fn with_execution_input_validator(mut self, validator: ToolInputValidator) -> Self {
        self.execution_input_validator = validator;
        self
    }

    pub fn prepare_model_input(&self, input: &Value) -> Result<Value, ToolInputValidationError> {
        (self.model_input_validator)(input)?;
        match &self.exact_input_bindings {
            Some(bindings) => bindings
                .iter()
                .find(|binding| binding.model_input == *input)
                .map(|binding| binding.execution_input.clone())
                .ok_or_else(|| ToolInputValidationError::new("input_not_allowed")),
            None => Ok(input.clone()),
        }
    }

    pub fn validate_execution_input(&self, input: &Value) -> Result<(), ToolInputValidationError> {
        (self.execution_input_validator)(input)?;
        if self.exact_input_bindings.as_ref().is_some_and(|bindings| {
            !bindings
                .iter()
                .any(|binding| binding.execution_input == *input)
        }) {
            return Err(ToolInputValidationError::new("input_not_allowed"));
        }
        Ok(())
    }

    pub fn restrict_to_exact_inputs(&mut self, inputs: Vec<Value>) -> Result<(), String> {
        self.restrict_to_input_bindings(
            inputs
                .into_iter()
                .map(|input| (input.clone(), input))
                .collect(),
        )
    }

    pub fn restrict_to_input_bindings(
        &mut self,
        bindings: Vec<(Value, Value)>,
    ) -> Result<(), String> {
        if self.exact_input_bindings.is_some() {
            return Err(format!(
                "tool {} input contract is already restricted",
                self.name
            ));
        }
        if bindings.is_empty() {
            return Err(format!(
                "tool {} exact input contract must not be empty",
                self.name
            ));
        }
        let mut unique_bindings: Vec<ToolInputBinding> = Vec::new();
        for (model_input, execution_input) in bindings {
            if !model_input.is_object() || !execution_input.is_object() {
                return Err(format!(
                    "tool {} exact model and execution inputs must be objects",
                    self.name
                ));
            }
            (self.model_input_validator)(&model_input).map_err(|error| {
                format!(
                    "tool {} exact model input violates its model contract: {}",
                    self.name, error.code
                )
            })?;
            (self.execution_input_validator)(&execution_input).map_err(|error| {
                format!(
                    "tool {} exact execution input violates its executable contract: {}",
                    self.name, error.code
                )
            })?;
            if let Some(existing) = unique_bindings
                .iter()
                .find(|binding| binding.model_input == model_input)
            {
                if existing.execution_input != execution_input {
                    return Err(format!(
                        "tool {} exact model input maps to multiple execution inputs",
                        self.name
                    ));
                }
                continue;
            }
            if unique_bindings
                .iter()
                .any(|binding| binding.execution_input == execution_input)
            {
                return Err(format!(
                    "tool {} exact execution input maps from multiple model inputs",
                    self.name
                ));
            }
            unique_bindings.push(ToolInputBinding {
                model_input,
                execution_input,
            });
        }
        let model_inputs = unique_bindings
            .iter()
            .map(|binding| binding.model_input.clone())
            .collect::<Vec<_>>();
        self.input_schema = exact_inputs_schema(&model_inputs);
        self.exact_input_bindings = Some(unique_bindings);
        Ok(())
    }

    pub fn exact_model_inputs(&self) -> Vec<Value> {
        self.exact_input_bindings
            .as_ref()
            .map(|bindings| {
                bindings
                    .iter()
                    .map(|binding| binding.model_input.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn to_schema_payload(&self) -> Value {
        json!({
            "name": self.name,
            "description": redact_public_text(&self.description),
            "input_schema": self.input_schema,
        })
    }
}

fn exact_inputs_schema(inputs: &[Value]) -> Value {
    let schemas = inputs.iter().map(exact_input_schema).collect::<Vec<_>>();
    if schemas.len() == 1 {
        schemas.into_iter().next().expect("one exact input schema")
    } else {
        json!({"oneOf": schemas})
    }
}

fn exact_input_schema(input: &Value) -> Value {
    let properties = input
        .as_object()
        .expect("executable tool input validators require an object");
    let required = properties.keys().cloned().collect::<Vec<_>>();
    let properties = properties
        .iter()
        .map(|(name, value)| {
            let mut property = serde_json::Map::new();
            property.insert("const".to_string(), value.clone());
            if let Some(value_type) = json_schema_type(value) {
                property.insert("type".to_string(), Value::String(value_type.to_string()));
            }
            (name.clone(), Value::Object(property))
        })
        .collect::<serde_json::Map<_, _>>();
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    })
}

fn json_schema_type(value: &Value) -> Option<&'static str> {
    match value {
        Value::Null => Some("null"),
        Value::Bool(_) => Some("boolean"),
        Value::Number(number) if number.is_i64() || number.is_u64() => Some("integer"),
        Value::Number(_) => Some("number"),
        Value::String(_) => Some("string"),
        Value::Array(_) => Some("array"),
        Value::Object(_) => Some("object"),
    }
}

fn validate_read_tool_input(input: &Value) -> Result<(), ToolInputValidationError> {
    let input: ReadToolInput = deserialize_tool_input(input, "read_input_schema_mismatch")?;
    input
        .validate()
        .map_err(|_| ToolInputValidationError::new("read_input_invalid"))
}

fn validate_list_tool_input(input: &Value) -> Result<(), ToolInputValidationError> {
    let input: ListToolInput = deserialize_tool_input(input, "list_input_schema_mismatch")?;
    input
        .validate()
        .map_err(|_| ToolInputValidationError::new("list_input_invalid"))
}

fn validate_grep_tool_input(input: &Value) -> Result<(), ToolInputValidationError> {
    let input: GrepToolInput = deserialize_tool_input(input, "grep_input_schema_mismatch")?;
    input
        .validate()
        .map_err(|_| ToolInputValidationError::new("grep_input_invalid"))
}

fn validate_edit_tool_input(input: &Value) -> Result<(), ToolInputValidationError> {
    let input: EditToolInput = deserialize_tool_input(input, "edit_input_schema_mismatch")?;
    input
        .validate()
        .map_err(|_| ToolInputValidationError::new("edit_input_invalid"))
}

fn validate_patch_tool_input(input: &Value) -> Result<(), ToolInputValidationError> {
    let input: WorkspacePatch = deserialize_tool_input(input, "patch_input_schema_mismatch")?;
    input
        .validate()
        .map_err(|_| ToolInputValidationError::new("patch_input_invalid"))
}

fn command_input_validation_code(input: &Value) -> &'static str {
    match input.get("argv") {
        None => "missing_argv",
        Some(Value::Array(_)) => "invalid_command_arguments",
        Some(_) => "argv_not_array",
    }
}

fn validate_command_model_input(input: &Value) -> Result<(), ToolInputValidationError> {
    let validation_code = command_input_validation_code(input);
    let input: CommandModelInput = deserialize_tool_input(input, validation_code)?;
    input
        .into_execution_input()
        .validate()
        .map_err(|_| ToolInputValidationError::new(validation_code))
}

fn validate_command_execution_input(input: &Value) -> Result<(), ToolInputValidationError> {
    let validation_code = command_input_validation_code(input);
    let input: CommandToolInput = deserialize_tool_input(input, validation_code)?;
    input
        .validate()
        .map_err(|_| ToolInputValidationError::new(validation_code))
}

fn deserialize_tool_input<T>(
    input: &Value,
    validation_code: &'static str,
) -> Result<T, ToolInputValidationError>
where
    T: for<'de> Deserialize<'de>,
{
    serde_json::from_value(input.clone())
        .map_err(|_| ToolInputValidationError::new(validation_code))
}

/// 返回内置的工作区读取、搜索、变更和命令工具定义。
pub fn workspace_tool_specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec::new(
            BUILTIN_READ_TOOL,
            "Read a bounded range from a workspace text file",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "minLength": 1},
                    "max_chars": {"type": "integer", "minimum": 1, "maximum": MAX_READ_MAX_CHARS},
                    "line_start": {"type": "integer", "minimum": 1},
                    "line_end": {"type": "integer", "minimum": 1}
                },
                "required": ["path"],
                "additionalProperties": false
            }),
            ToolExecutionMode::ParallelRead,
            validate_read_tool_input,
        ),
        ToolSpec::new(
            BUILTIN_LIST_TOOL,
            "List bounded workspace directory entries with optional recursion",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "minLength": 1},
                    "max_entries": {"type": "integer", "minimum": 1, "maximum": MAX_LIST_MAX_ENTRIES},
                    "recursive": {"type": "boolean"},
                    "max_depth": {"type": "integer", "minimum": 1, "maximum": MAX_LIST_MAX_DEPTH}
                },
                "required": [],
                "additionalProperties": false
            }),
            ToolExecutionMode::ParallelRead,
            validate_list_tool_input,
        ),
        ToolSpec::new(
            BUILTIN_GREP_TOOL,
            "Search bounded workspace text with deterministic ordering",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "minLength": 1},
                    "pattern": {"type": "string", "minLength": 1},
                    "max_matches": {"type": "integer", "minimum": 1, "maximum": MAX_GREP_MAX_MATCHES},
                    "case_sensitive": {"type": "boolean"}
                },
                "required": ["pattern"],
                "additionalProperties": false
            }),
            ToolExecutionMode::ParallelRead,
            validate_grep_tool_input,
        ),
        ToolSpec::new(
            BUILTIN_EDIT_TOOL,
            "Replace expected text in a workspace file",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "minLength": 1},
                    "expected": {"type": "string"},
                    "replacement": {"type": "string"}
                },
                "required": ["path", "expected", "replacement"],
                "additionalProperties": false
            }),
            ToolExecutionMode::Exclusive,
            validate_edit_tool_input,
        ),
        ToolSpec::new(
            BUILTIN_PATCH_TOOL,
            "Apply explicit workspace file changes",
            json!({
                "type": "object",
                "properties": {
                    "changes": {
                        "type": "array",
                        "minItems": 1,
                        "items": {
                            "type": "object",
                            "properties": {
                                "path": {"type": "string", "minLength": 1},
                                "expected": {"type": "string"},
                                "replacement": {"type": "string"}
                            },
                            "required": ["path", "replacement"],
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["changes"],
                "additionalProperties": false
            }),
            ToolExecutionMode::Exclusive,
            validate_patch_tool_input,
        ),
        ToolSpec::new(
            BUILTIN_COMMAND_TOOL,
            "Run a bounded sandboxed command",
            json!({
                "type": "object",
                "properties": {
                    "argv": {
                        "type": "array",
                        "items": {"type": "string"},
                        "minItems": 1
                    },
                    "cwd": {"type": "string", "minLength": 1},
                    "timeout_seconds": {"type": "integer", "minimum": 1, "maximum": MAX_COMMAND_TIMEOUT_SECONDS}
                },
                "required": ["argv"],
                "additionalProperties": false
            }),
            ToolExecutionMode::Exclusive,
            validate_command_model_input,
        )
        .with_execution_input_validator(validate_command_execution_input),
    ]
}

/// 负责管理向模型暴露且供工具代理器使用的工具注册表。
#[derive(Debug, Default, Clone)]
pub struct ToolRegistry {
    tools: BTreeMap<String, ToolSpec>,
}

impl ToolRegistry {
    pub fn register(&mut self, spec: ToolSpec) -> Result<(), String> {
        validate_tool_name(&spec.name)?;
        if self.tools.contains_key(&spec.name) {
            return Err(format!("tool already registered: {}", spec.name));
        }
        self.tools.insert(spec.name.clone(), spec);
        Ok(())
    }

    pub fn get(&self, name: &str) -> Option<&ToolSpec> {
        self.tools.get(name)
    }

    pub fn schema_payloads(&self) -> Vec<Value> {
        self.tools
            .values()
            .map(ToolSpec::to_schema_payload)
            .collect::<Vec<_>>()
    }

    pub fn prepare_model_input(
        &self,
        name: &str,
        input: &Value,
    ) -> Result<(ToolExecutionMode, Value), ToolInputValidationError> {
        let spec = self
            .get(name)
            .ok_or_else(|| ToolInputValidationError::new("tool_not_visible"))?;
        let execution_input = spec.prepare_model_input(input)?;
        Ok((spec.execution_mode, execution_input))
    }

    pub fn validate_execution_input(
        &self,
        name: &str,
        input: &Value,
    ) -> Result<ToolExecutionMode, ToolInputValidationError> {
        let spec = self
            .get(name)
            .ok_or_else(|| ToolInputValidationError::new("tool_not_visible"))?;
        spec.validate_execution_input(input)?;
        Ok(spec.execution_mode)
    }
}

/// 保留失败类别，使调用方能够区分输入、策略、沙箱和执行错误。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ToolFailureKind {
    Input,
    Visibility,
    Capability,
    Policy,
    PermissionProfile,
    WorkspaceBoundary,
    ProtectedPath,
    Approval,
    Sandbox,
    Backend,
    Infrastructure,
    Execution,
    Timeout,
    Cancelled,
}

/// 决定工具代理器执行、拒绝还是暂停调用的授权结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ToolBrokerDecision {
    Allow,
    Approved {
        approval_grant_id: String,
    },
    Deny {
        failure_kind: ToolFailureKind,
        reason: String,
    },
    Ask {
        approval_request_id: String,
        reason: String,
    },
}

impl ToolBrokerDecision {
    pub fn deny(reason: impl Into<String>) -> Self {
        Self::deny_with_kind(ToolFailureKind::Policy, reason)
    }

    pub fn deny_with_kind(failure_kind: ToolFailureKind, reason: impl Into<String>) -> Self {
        Self::Deny {
            failure_kind,
            reason: reason.into(),
        }
    }

    pub fn is_allowed(&self) -> bool {
        matches!(self, Self::Allow | Self::Approved { .. })
    }

    pub fn approved(approval_grant_id: impl Into<String>) -> Self {
        Self::Approved {
            approval_grant_id: approval_grant_id.into(),
        }
    }

    pub fn ask(approval_request_id: impl Into<String>, reason: impl Into<String>) -> Self {
        Self::Ask {
            approval_request_id: approval_request_id.into(),
            reason: reason.into(),
        }
    }
}

/// 执行边界；调用执行器闭包前会重新校验工具输入。
#[derive(Debug, Default, Clone)]
pub struct ToolBroker {
    registry: ToolRegistry,
}

impl ToolBroker {
    pub fn new(registry: ToolRegistry) -> Self {
        Self { registry }
    }

    pub fn register(&mut self, spec: ToolSpec) -> Result<(), String> {
        self.registry.register(spec)
    }

    pub fn get(&self, name: &str) -> Option<&ToolSpec> {
        self.registry.get(name)
    }

    pub fn tool_schema_payloads(&self) -> Vec<Value> {
        self.registry.schema_payloads()
    }

    pub fn prepare_model_input(
        &self,
        name: &str,
        input: &Value,
    ) -> Result<(ToolExecutionMode, Value), ToolInputValidationError> {
        self.registry.prepare_model_input(name, input)
    }

    pub fn validate_execution_input(
        &self,
        name: &str,
        input: &Value,
    ) -> Result<ToolExecutionMode, ToolInputValidationError> {
        self.registry.validate_execution_input(name, input)
    }

    /// 执行允许的调用；否则返回类型化结果且不调用闭包。
    pub fn execute<F>(
        &self,
        envelope: &ToolCallRequest,
        decision: ToolBrokerDecision,
        executor: F,
    ) -> ToolResult
    where
        F: FnOnce(&ToolCallRequest) -> ToolOutput,
    {
        if self.registry.get(&envelope.tool_name).is_none() {
            return ToolResult::failed_with_kind(
                envelope,
                ToolFailureKind::Visibility,
                UNKNOWN_TOOL_ERROR,
                "tool is not registered",
            );
        }
        if decision.is_allowed() {
            let input = match serde_json::from_str::<Value>(&envelope.raw_arguments) {
                Ok(input) => input,
                Err(_) => {
                    let output = ToolOutput::failure_with_kind(
                        ToolFailureKind::Input,
                        INVALID_TOOL_ARGUMENTS_ERROR,
                        json!({
                            "summary": "tool arguments failed executable input validation",
                            "validation_code": "invalid_json_arguments",
                        }),
                    );
                    return ToolResult::from_result(envelope, &output);
                }
            };
            if let Err(error) = self
                .registry
                .validate_execution_input(&envelope.tool_name, &input)
            {
                let output = ToolOutput::failure_with_kind(
                    ToolFailureKind::Input,
                    INVALID_TOOL_ARGUMENTS_ERROR,
                    json!({
                        "summary": "tool arguments failed executable input validation",
                        "validation_code": error.code,
                    }),
                );
                return ToolResult::from_result(envelope, &output);
            }
        }
        if let ToolBrokerDecision::Deny {
            failure_kind,
            reason,
        } = decision
        {
            return ToolResult::failed_with_kind(envelope, failure_kind, TOOL_DENIED_ERROR, reason);
        }
        if let ToolBrokerDecision::Ask { reason, .. } = decision {
            return ToolResult::approval_required(envelope, reason);
        }
        ToolResult::from_result(envelope, &executor(envelope))
    }
}

/// 从模型工具调用传给工具代理器和执行器的规范化封装结构。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ToolCallRequest {
    pub tool_call_id: String,
    pub tool_name: String,
    pub raw_arguments: String,
}

impl ToolCallRequest {
    pub fn new(
        tool_call_id: impl Into<String>,
        tool_name: impl Into<String>,
        raw_arguments: impl Into<String>,
    ) -> Self {
        Self {
            tool_call_id: tool_call_id.into(),
            tool_name: tool_name.into(),
            raw_arguments: raw_arguments.into(),
        }
    }
}

/// 工具代理器对其公开投影进行有界化和脱敏前的执行器原始输出。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ToolOutput {
    pub ok: bool,
    pub content: Value,
    pub error_code: Option<String>,
    pub failure_kind: Option<ToolFailureKind>,
    pub truncated: bool,
    pub metadata: Value,
}

impl ToolOutput {
    pub fn success(content: Value) -> Self {
        Self {
            ok: true,
            content,
            error_code: None,
            failure_kind: None,
            truncated: false,
            metadata: json!({}),
        }
    }

    pub fn failure(error_code: impl Into<String>, content: Value) -> Self {
        Self::failure_with_kind(ToolFailureKind::Execution, error_code, content)
    }

    pub fn failure_with_kind(
        failure_kind: ToolFailureKind,
        error_code: impl Into<String>,
        content: Value,
    ) -> Self {
        Self {
            ok: false,
            content,
            error_code: Some(error_code.into()),
            failure_kind: Some(failure_kind),
            truncated: false,
            metadata: json!({}),
        }
    }
}

/// 用于派生模型历史、追踪和完成证据的有界内部结果。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ToolResult {
    pub tool_call_id: String,
    pub tool_name: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
    pub error_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_kind: Option<ToolFailureKind>,
    pub artifact_refs: Vec<String>,
    #[serde(skip)]
    pub result_id: Option<String>,
    pub truncated: bool,
    #[serde(skip)]
    audit_metadata: Option<Value>,
}

impl ToolResult {
    pub fn summary(
        tool_call_id: impl Into<String>,
        tool_name: impl Into<String>,
        ok: bool,
        preview: impl Into<String>,
    ) -> Self {
        Self {
            tool_call_id: tool_call_id.into(),
            tool_name: tool_name.into(),
            ok,
            content: None,
            preview: Some(redact_public_text(&preview.into())),
            error_code: None,
            failure_kind: None,
            artifact_refs: Vec::new(),
            result_id: None,
            truncated: false,
            audit_metadata: None,
        }
    }

    pub fn with_audit(mut self, metadata: Value) -> Self {
        self.audit_metadata = Some(metadata);
        self
    }

    pub fn audit_metadata(&self) -> Option<&Value> {
        self.audit_metadata.as_ref()
    }

    pub fn from_result(envelope: &ToolCallRequest, result: &ToolOutput) -> Self {
        let result_content = result.content.to_string();
        let content_is_safe = redact_public_text(&result_content) == result_content;
        let (bounded_preview, preview_truncated) =
            bounded_text(&result_content, DEFAULT_RESULT_PREVIEW_MAX_CHARS);
        let preview = redact_public_text(&bounded_preview);
        let source_truncated = result.truncated
            || result
                .content
                .get("truncated")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            || result
                .content
                .get("output_truncated")
                .and_then(Value::as_bool)
                .unwrap_or(false);
        let truncated = source_truncated || preview_truncated;
        let artifact_refs = result_artifact_refs(&result.content, &result.metadata);
        let result_id = result_id(&result.content, &result.metadata);
        let mut tool_result = Self {
            error_code: result.error_code.clone(),
            failure_kind: result.failure_kind.clone(),
            truncated,
            ..Self::summary(
                envelope.tool_call_id.clone(),
                envelope.tool_name.clone(),
                result.ok,
                preview,
            )
        };
        if content_is_safe && !source_truncated && !preview_truncated {
            tool_result.content = Some(result.content.clone());
            tool_result.preview = None;
        }
        tool_result.artifact_refs = artifact_refs;
        tool_result.result_id = result_id;
        if truncated && !tool_result.artifact_refs.is_empty() {
            tool_result.content = None;
            tool_result.preview = None;
        }
        tool_result.audit_metadata = result.metadata.get("audit").cloned();
        tool_result
    }

    pub fn failed(
        envelope: &ToolCallRequest,
        error_code: impl Into<String>,
        preview: impl Into<String>,
    ) -> Self {
        Self::failed_with_kind(envelope, ToolFailureKind::Execution, error_code, preview)
    }

    pub fn failed_with_kind(
        envelope: &ToolCallRequest,
        failure_kind: ToolFailureKind,
        error_code: impl Into<String>,
        preview: impl Into<String>,
    ) -> Self {
        Self {
            error_code: Some(error_code.into()),
            failure_kind: Some(failure_kind),
            ..Self::summary(
                envelope.tool_call_id.clone(),
                envelope.tool_name.clone(),
                false,
                preview,
            )
        }
    }

    pub fn approval_required(envelope: &ToolCallRequest, reason: impl Into<String>) -> Self {
        Self::failed_with_kind(
            envelope,
            ToolFailureKind::Approval,
            TOOL_APPROVAL_REQUIRED_ERROR,
            reason,
        )
    }

    pub fn to_message_payload(&self) -> Value {
        let artifact_refs = self
            .artifact_refs
            .iter()
            .filter_map(|value| safe_reference(value))
            .collect::<Vec<_>>();
        let mut payload = json!({
            "ok": self.ok,
            "tool_name": self.tool_name,
            "tool_call_id": self.tool_call_id,
            "truncated": self.truncated,
        });
        if let Some(error_code) = self.error_code.as_deref() {
            payload["error_code"] = json!(error_code);
        }
        if let Some(failure_kind) = self.failure_kind.as_ref() {
            payload["failure_kind"] =
                serde_json::to_value(failure_kind).unwrap_or_else(|_| json!("execution"));
        }
        if !artifact_refs.is_empty() {
            payload["artifact_refs"] = json!(artifact_refs);
        }
        if let Some(content) = self.content.as_ref() {
            let serialized = content.to_string();
            let (bounded_preview, content_truncated) =
                bounded_text(&serialized, DEFAULT_RESULT_PREVIEW_MAX_CHARS);
            if !content_truncated && redact_public_text(&serialized) == serialized {
                payload["content"] = content.clone();
            } else {
                payload["preview"] = json!(redact_public_text(&bounded_preview));
                payload["truncated"] = json!(self.truncated || content_truncated);
            }
        } else if let Some(preview) = self.preview.as_deref() {
            let preview = redact_public_text(preview);
            payload["preview"] = json!(preview);
        }
        payload
    }
}

fn result_artifact_refs(content: &Value, metadata: &Value) -> Vec<String> {
    let mut refs = Vec::new();
    refs.extend(value_string(content.get("artifact_ref")));
    refs.extend(value_string(content.get("diff_ref")));
    refs.extend(value_string(metadata.get("artifact_ref")));
    refs.extend(value_string(metadata.get("diff_ref")));
    refs.extend(value_string_array(content.get("artifact_refs")));
    refs.extend(value_string_array(metadata.get("artifact_refs")));
    refs.sort();
    refs.dedup();
    refs
}

fn result_id(content: &Value, metadata: &Value) -> Option<String> {
    value_string(metadata.get("result_id"))
        .or_else(|| value_string(content.get("result_id")))
        .or_else(|| value_string(metadata.get("output_digest")))
        .or_else(|| value_string(content.get("output_digest")))
}

fn value_string(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string)
}

fn safe_reference(value: &str) -> Option<String> {
    let lowered = value.to_ascii_lowercase();
    if contains_sensitive_text(value)
        || PROMPT_INJECTION_MARKERS
            .iter()
            .any(|marker| lowered.contains(marker))
    {
        None
    } else {
        Some(value.to_string())
    }
}

fn value_string_array(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string)
        .collect()
}

/// 工作区工具返回的工作区边界、受保护路径、沙箱和变更错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceToolError {
    OutsideWorkspace(String),
    ProtectedPath(String),
    SandboxUnavailable,
    BinaryPattern,
    ReadFailed(String),
    RollbackFailed(String),
    ExpectedContentMissing(String),
    InvalidInput(String),
}

impl fmt::Display for WorkspaceToolError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutsideWorkspace(path) => write!(formatter, "path outside workspace: {path}"),
            Self::ProtectedPath(path) => {
                write!(formatter, "protected path requires approval: {path}")
            }
            Self::SandboxUnavailable => write!(formatter, "strict sandbox backend unavailable"),
            Self::BinaryPattern => write!(formatter, "grep pattern must be valid utf-8 text"),
            Self::ReadFailed(message) => write!(formatter, "workspace tool read failed: {message}"),
            Self::RollbackFailed(message) => {
                write!(formatter, "workspace mutation rollback failed: {message}")
            }
            Self::ExpectedContentMissing(path) => {
                write!(formatter, "expected content not found in {path}")
            }
            Self::InvalidInput(message) => {
                write!(formatter, "invalid workspace tool input: {message}")
            }
        }
    }
}

impl std::error::Error for WorkspaceToolError {}

/// 工作区工具接受的有界文件读取请求。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadToolInput {
    pub path: String,
    pub max_chars: Option<usize>,
    pub line_start: Option<usize>,
    pub line_end: Option<usize>,
}

impl ReadToolInput {
    fn validate(&self) -> Result<(), WorkspaceToolError> {
        validate_nonempty_path("path", &self.path)?;
        validate_limit(
            "max_chars",
            self.max_chars,
            DEFAULT_READ_MAX_CHARS,
            MAX_READ_MAX_CHARS,
        )?;
        validate_line_range(
            self.line_start.unwrap_or(1),
            self.line_end.unwrap_or(usize::MAX),
        )
    }
}

/// 工作区工具接受的有界目录列表请求。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListToolInput {
    pub path: Option<String>,
    pub max_entries: Option<usize>,
    #[serde(default)]
    pub recursive: bool,
    pub max_depth: Option<usize>,
}

impl ListToolInput {
    fn validate(&self) -> Result<(), WorkspaceToolError> {
        if let Some(path) = self.path.as_deref() {
            validate_nonempty_path("path", path)?;
        }
        validate_limit(
            "max_entries",
            self.max_entries,
            DEFAULT_LIST_MAX_ENTRIES,
            MAX_LIST_MAX_ENTRIES,
        )?;
        validate_limit(
            "max_depth",
            self.max_depth,
            DEFAULT_LIST_MAX_DEPTH,
            MAX_LIST_MAX_DEPTH,
        )?;
        Ok(())
    }
}

/// 工作区工具接受的有界文本搜索请求。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GrepToolInput {
    pub path: Option<String>,
    pub pattern: String,
    pub max_matches: Option<usize>,
    #[serde(default = "default_case_sensitive")]
    pub case_sensitive: bool,
}

impl GrepToolInput {
    fn validate(&self) -> Result<(), WorkspaceToolError> {
        if self.pattern.is_empty() {
            return Err(WorkspaceToolError::InvalidInput(
                "pattern must not be empty".to_string(),
            ));
        }
        if let Some(path) = self.path.as_deref() {
            validate_nonempty_path("path", path)?;
        }
        validate_limit(
            "max_matches",
            self.max_matches,
            DEFAULT_GREP_MAX_MATCHES,
            MAX_GREP_MAX_MATCHES,
        )?;
        Ok(())
    }
}

/// 工作区工具接受的单文件替换请求。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EditToolInput {
    pub path: String,
    pub expected: String,
    pub replacement: String,
}

impl EditToolInput {
    fn validate(&self) -> Result<(), WorkspaceToolError> {
        validate_nonempty_path("path", &self.path)
    }
}

/// 多文件变更；所有目标会在开始写入前完成校验。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkspacePatch {
    pub changes: Vec<WorkspacePatchChange>,
}

impl WorkspacePatch {
    fn validate(&self) -> Result<(), WorkspaceToolError> {
        if self.changes.is_empty() {
            return Err(WorkspaceToolError::InvalidInput(
                "patch must contain at least one change".to_string(),
            ));
        }
        for change in &self.changes {
            validate_nonempty_path("changes[].path", &change.path)?;
        }
        Ok(())
    }
}

/// 工作区补丁中一个带预期内容保护的变更。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkspacePatchChange {
    pub path: String,
    pub expected: Option<String>,
    pub replacement: String,
}

/// 绑定到根目录的工作区文件工具，以及为命令配置的严格沙箱后端。
#[derive(Clone)]
pub struct WorkspaceTools {
    workspace_root: PathBuf,
    sandbox_backend: Option<Arc<dyn SandboxBackend + Send + Sync>>,
    command_environment: CommandEnvironmentPolicy,
}

impl fmt::Debug for WorkspaceTools {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkspaceTools")
            .field("workspace_root", &self.workspace_root)
            .field(
                "sandbox_backend",
                &self.sandbox_backend.as_ref().map(|backend| backend.name()),
            )
            .finish()
    }
}

impl WorkspaceTools {
    pub fn new(workspace_root: impl Into<PathBuf>) -> Self {
        Self {
            workspace_root: workspace_root.into(),
            sandbox_backend: None,
            command_environment: CommandEnvironmentPolicy::default(),
        }
    }

    pub fn with_sandbox_backend(
        self,
        sandbox_backend: impl SandboxBackend + Send + Sync + 'static,
    ) -> Self {
        self.with_shared_sandbox_backend(Arc::new(sandbox_backend))
    }

    pub fn with_shared_sandbox_backend(
        mut self,
        sandbox_backend: Arc<dyn SandboxBackend + Send + Sync>,
    ) -> Self {
        self.sandbox_backend = Some(sandbox_backend);
        self
    }

    pub fn with_command_environment(mut self, environment: CommandEnvironmentPolicy) -> Self {
        self.command_environment = environment;
        self
    }

    /// 在执行或变更前校验输入，并解析每个被引用的路径。
    pub fn preflight(&self, tool_name: &str, input: &Value) -> Result<(), WorkspaceToolError> {
        match tool_name {
            BUILTIN_READ_TOOL => {
                let input: ReadToolInput = preflight_input(input)?;
                input.validate()?;
                self.resolve_workspace_path(&input.path, false)?;
            }
            BUILTIN_LIST_TOOL => {
                let input: ListToolInput = preflight_input(input)?;
                input.validate()?;
                self.resolve_workspace_path(input.path.as_deref().unwrap_or("."), false)?;
            }
            BUILTIN_GREP_TOOL => {
                let input: GrepToolInput = preflight_input(input)?;
                input.validate()?;
                self.resolve_workspace_path(
                    input.path.as_deref().unwrap_or("."),
                    input.path.is_none(),
                )?;
            }
            BUILTIN_EDIT_TOOL => {
                let input: EditToolInput = preflight_input(input)?;
                input.validate()?;
                self.resolve_workspace_path(&input.path, false)?;
            }
            BUILTIN_PATCH_TOOL => {
                let patch: WorkspacePatch = preflight_input(input)?;
                patch.validate()?;
                let mut targets = BTreeSet::new();
                for change in patch.changes {
                    let target = self.resolve_workspace_path(&change.path, false)?;
                    if !targets.insert(target) {
                        return Err(WorkspaceToolError::InvalidInput(
                            DUPLICATE_PATCH_TARGET.to_string(),
                        ));
                    }
                }
            }
            BUILTIN_COMMAND_TOOL => {
                let input: CommandToolInput = preflight_input(input)?;
                input.validate()?;
                let Some(backend) = &self.sandbox_backend else {
                    return Err(WorkspaceToolError::SandboxUnavailable);
                };
                if !backend.capabilities().supports_command_execution() {
                    return Err(WorkspaceToolError::SandboxUnavailable);
                }
                self.resolve_workspace_path(input.cwd.as_deref().unwrap_or("."), false)?;
            }
            _ => {
                return Err(WorkspaceToolError::InvalidInput(
                    "tool backend is unavailable".to_string(),
                ));
            }
        }
        Ok(())
    }

    pub fn read(&self, input: ReadToolInput) -> Result<ToolOutput, WorkspaceToolError> {
        input.validate()?;
        let max_chars = input.max_chars.unwrap_or(DEFAULT_READ_MAX_CHARS);
        let line_start = input.line_start.unwrap_or(1);
        let line_end = input.line_end.unwrap_or(usize::MAX);
        let target = self.resolve_workspace_path(&input.path, false)?;
        let relative = self.relative_path(&target);
        let file = File::open(&target).map_err(io_error)?;
        let mut reader = BufReader::new(file);
        let mut line = Vec::new();
        let mut preview = String::new();
        let mut preview_truncated = false;
        let mut actual_line_start = None;
        let mut actual_line_end = None;
        let mut total_lines = 0usize;
        let mut total_bytes = 0usize;
        let mut last_line_partial = false;

        loop {
            line.clear();
            let bytes_read = reader.read_until(b'\n', &mut line).map_err(io_error)?;
            if bytes_read == 0 {
                break;
            }
            total_lines = total_lines.saturating_add(1);
            total_bytes = total_bytes.saturating_add(bytes_read);
            if is_binary(&line) {
                return Ok(ToolOutput::success(json!({
                    "path": relative,
                    "binary": true,
                    "preview": BINARY_CONTENT_PREVIEW,
                    "truncated": true,
                    "line_start": Value::Null,
                    "line_end": Value::Null,
                    "total_lines": total_lines,
                    "artifact_ref": artifact_ref(RESULT_ARTIFACT_PREFIX, &relative),
                })));
            }
            let text = std::str::from_utf8(&line)
                .map(str::to_string)
                .map_err(|error| {
                    WorkspaceToolError::ReadFailed(format!(
                        "invalid utf-8 after binary check: {error}"
                    ))
                })?;
            if total_lines < line_start || total_lines > line_end {
                continue;
            }
            actual_line_start.get_or_insert(total_lines);
            let remaining = max_chars.saturating_sub(preview.chars().count());
            if remaining == 0 {
                preview_truncated = true;
                continue;
            }
            let (bounded, truncated) = bounded_text(&text, remaining);
            preview.push_str(&bounded);
            actual_line_end = Some(total_lines);
            if truncated {
                preview_truncated = true;
                last_line_partial = true;
            }
        }

        let next_line_start = actual_line_end.and_then(|line_end| {
            if last_line_partial {
                None
            } else if line_end < total_lines {
                line_end.checked_add(1)
            } else {
                None
            }
        });
        let artifact = if preview_truncated || total_bytes > LARGE_OUTPUT_ARTIFACT_THRESHOLD {
            Value::String(artifact_ref(RESULT_ARTIFACT_PREFIX, &relative))
        } else {
            Value::Null
        };
        let mut output = json!({
            "path": relative,
            "binary": false,
            "preview": preview,
            "truncated": preview_truncated,
            "line_start": actual_line_start,
            "line_end": actual_line_end,
            "total_lines": total_lines,
            "partial_line": last_line_partial,
            "artifact_ref": artifact,
        });
        if let Some(next_line_start) = next_line_start {
            output["next_line_start"] = json!(next_line_start);
        }
        Ok(ToolOutput::success(output))
    }

    pub fn list(&self, input: ListToolInput) -> Result<ToolOutput, WorkspaceToolError> {
        input.validate()?;
        let target = self.resolve_workspace_path(input.path.as_deref().unwrap_or("."), false)?;
        let max_entries = input.max_entries.unwrap_or(DEFAULT_LIST_MAX_ENTRIES);
        let max_depth = input.max_depth.unwrap_or(DEFAULT_LIST_MAX_DEPTH);
        let mut state = ListState {
            entries: Vec::new(),
            redacted_entries: 0,
            truncated: false,
            collection_limit: max_entries.saturating_add(1),
            recursive: input.recursive,
            max_depth,
        };
        self.collect_list_entries(&target, 0, &mut state)?;
        state
            .entries
            .sort_by(|left, right| left.relative.cmp(&right.relative));
        let truncated_by_count = state.entries.len() > max_entries;
        state.entries.truncate(max_entries);
        Ok(ToolOutput::success(json!({
            "entries": state
                .entries
                .into_iter()
                .map(|entry| json!({
                    "path": entry.relative,
                    "kind": if entry.is_dir { "directory" } else { "file" },
                }))
                .collect::<Vec<_>>(),
            "redacted_entries": state.redacted_entries,
            "truncated": state.truncated || truncated_by_count,
        })))
    }

    pub fn grep(&self, input: GrepToolInput) -> Result<ToolOutput, WorkspaceToolError> {
        input.validate()?;
        let root = self
            .resolve_workspace_path(input.path.as_deref().unwrap_or("."), input.path.is_none())?;
        let max_matches = input.max_matches.unwrap_or(DEFAULT_GREP_MAX_MATCHES);
        let mut matches = Vec::new();
        let collection_limit = max_matches.saturating_add(1);
        let truncated = self.grep_path(
            &root,
            &input.pattern,
            input.case_sensitive,
            collection_limit,
            &mut matches,
        )?;
        matches.truncate(max_matches);
        Ok(ToolOutput::success(json!({
            "matches": matches,
            "truncated": truncated,
        })))
    }

    pub fn edit(
        &self,
        input: EditToolInput,
        decision: &ToolBrokerDecision,
    ) -> Result<ToolOutput, WorkspaceToolError> {
        input.validate()?;
        self.patch(
            WorkspacePatch {
                changes: vec![WorkspacePatchChange {
                    path: input.path,
                    expected: Some(input.expected),
                    replacement: input.replacement,
                }],
            },
            decision,
        )
    }

    pub fn patch(
        &self,
        patch: WorkspacePatch,
        decision: &ToolBrokerDecision,
    ) -> Result<ToolOutput, WorkspaceToolError> {
        if !decision.is_allowed() {
            return Err(WorkspaceToolError::InvalidInput(
                WORKSPACE_MUTATION_NOT_APPROVED.to_string(),
            ));
        }
        patch.validate()?;
        let mut prepared = Vec::new();
        let mut targets = BTreeSet::new();
        for change in &patch.changes {
            let target = self.resolve_workspace_path(&change.path, false)?;
            let relative = self.relative_path(&target);
            if !targets.insert(target.clone()) {
                return Err(WorkspaceToolError::InvalidInput(format!(
                    "{DUPLICATE_PATCH_TARGET}: {relative}"
                )));
            }
            let original = existing_text_or_empty(&target)?;
            let updated = if let Some(expected) = &change.expected {
                if !original.contains(expected) {
                    return Err(WorkspaceToolError::ExpectedContentMissing(relative));
                }
                original.replacen(expected, &change.replacement, 1)
            } else {
                change.replacement.clone()
            };
            if updated == original {
                return Err(WorkspaceToolError::InvalidInput(format!(
                    "workspace mutation made no change: {relative}"
                )));
            }
            prepared.push((target, relative, original, updated));
        }
        let originals = prepared
            .iter()
            .map(|(path, relative, original, _updated)| {
                (
                    path.clone(),
                    relative.clone(),
                    original.clone(),
                    path.exists(),
                )
            })
            .collect::<Vec<_>>();
        for (path, _relative, _original, updated) in &prepared {
            if let Err(write_error) = atomic_write(path, updated) {
                if let Err(rollback_error) = rollback_originals(&originals) {
                    return Err(WorkspaceToolError::RollbackFailed(format!(
                        "write error: {write_error}; rollback error: {rollback_error}"
                    )));
                }
                return Err(write_error);
            }
        }
        let changed_files = prepared
            .iter()
            .map(|(_path, relative, _original, _updated)| relative.clone())
            .collect::<Vec<_>>();
        Ok(ToolOutput::success(json!({
            "changed_files": changed_files,
            "diff_ref": artifact_ref(DIFF_ARTIFACT_PREFIX, &changed_files.join(",")),
            "rolled_back": false,
        })))
    }

    pub fn command(&self, input: CommandToolInput) -> Result<ToolOutput, WorkspaceToolError> {
        self.command_cancellable(input, &CancellationToken::new())
    }

    /// 仅通过已配置的沙箱运行命令，并将取消传播给命令。
    pub fn command_cancellable(
        &self,
        input: CommandToolInput,
        cancellation: &CancellationToken,
    ) -> Result<ToolOutput, WorkspaceToolError> {
        input.validate()?;
        let Some(backend) = &self.sandbox_backend else {
            return Err(WorkspaceToolError::SandboxUnavailable);
        };
        let capabilities = backend.capabilities();
        if !capabilities.supports_command_execution() {
            return Err(WorkspaceToolError::SandboxUnavailable);
        }
        let filesystem_mode = input.sandbox_mode();
        let network_mode = input.network_access();
        let requested_cwd = input.cwd.as_deref().unwrap_or(".");
        let command_cwd = self.resolve_workspace_path(requested_cwd, false)?;
        let mut request = CommandRequest::project_verification(
            next_command_id(),
            input.argv,
            command_cwd.to_string_lossy().into_owned(),
            self.workspace_root.to_string_lossy().into_owned(),
        );
        request.filesystem.mode = filesystem_mode.clone();
        request.network.mode = network_mode.clone();
        request.environment = self.command_environment.clone();
        if let Some(timeout_seconds) = input.timeout_seconds {
            request.timeout_seconds = timeout_seconds;
        }
        let scope_digest = command_scope_digest(
            &request.argv,
            &request.cwd,
            request.timeout_seconds,
            &request.filesystem.mode,
            &request.network.mode,
        );
        let result = backend.execute_cancellable(&request, cancellation);
        let execution = result.sandbox.clone();
        let mut output = command_tool_output(result);
        output.metadata["result_id"] = json!(scope_digest);
        output.metadata["audit"] = json!({
            "cwd": request.cwd,
            "timeout_seconds": request.timeout_seconds,
            "sandbox_mode": filesystem_mode,
            "network_access": network_mode,
            "sandbox_backend": execution.backend,
            "sandbox_enforcement": execution.enforcement,
            "local_process_fallback": execution.local_process_fallback,
            "command_scope_digest": scope_digest,
            "command_provenance": "agent_requested",
        });
        Ok(output)
    }

    fn collect_list_entries(
        &self,
        directory: &Path,
        depth: usize,
        state: &mut ListState,
    ) -> Result<(), WorkspaceToolError> {
        if state.entries.len() >= state.collection_limit {
            state.truncated = true;
            return Ok(());
        }
        for entry in self.sorted_directory_entries(directory)? {
            if is_protected_path(&entry.relative) {
                state.redacted_entries = state.redacted_entries.saturating_add(1);
                continue;
            }
            if entry.is_symlink_or_reparse {
                continue;
            }
            state.entries.push(entry.clone());
            if state.entries.len() >= state.collection_limit {
                state.truncated = true;
                return Ok(());
            }
            if state.recursive && entry.is_dir {
                if depth < state.max_depth {
                    self.collect_list_entries(&entry.path, depth + 1, state)?;
                } else {
                    self.mark_depth_boundary(&entry.path, state)?;
                }
            }
        }
        Ok(())
    }

    fn mark_depth_boundary(
        &self,
        directory: &Path,
        state: &mut ListState,
    ) -> Result<(), WorkspaceToolError> {
        for entry in self.sorted_directory_entries(directory)? {
            if is_protected_path(&entry.relative) {
                state.redacted_entries = state.redacted_entries.saturating_add(1);
            } else if !entry.is_symlink_or_reparse {
                state.truncated = true;
            }
        }
        Ok(())
    }

    fn sorted_directory_entries(
        &self,
        directory: &Path,
    ) -> Result<Vec<DirectoryEntry>, WorkspaceToolError> {
        let mut entries = Vec::new();
        for entry in std::fs::read_dir(directory).map_err(io_error)? {
            let entry = entry.map_err(io_error)?;
            let path = entry.path();
            let metadata = std::fs::symlink_metadata(&path).map_err(io_error)?;
            entries.push(DirectoryEntry {
                relative: self.relative_path(&path),
                is_dir: metadata.is_dir(),
                is_symlink_or_reparse: metadata_is_symlink_or_reparse(&metadata),
                path,
            });
        }
        entries.sort_by(|left, right| left.relative.cmp(&right.relative));
        Ok(entries)
    }

    fn grep_path(
        &self,
        root: &Path,
        pattern: &str,
        case_sensitive: bool,
        collection_limit: usize,
        matches: &mut Vec<Value>,
    ) -> Result<bool, WorkspaceToolError> {
        if matches.len() >= collection_limit {
            return Ok(true);
        }
        let metadata = std::fs::symlink_metadata(root).map_err(io_error)?;
        if metadata_is_symlink_or_reparse(&metadata) {
            return Ok(false);
        }
        let relative = self.relative_path(root);
        if is_protected_path(&relative) {
            return Ok(false);
        }
        if metadata.is_dir() {
            for entry in self.sorted_directory_entries(root)? {
                if is_protected_path(&entry.relative) || entry.is_symlink_or_reparse {
                    continue;
                }
                if self.grep_path(
                    &entry.path,
                    pattern,
                    case_sensitive,
                    collection_limit,
                    matches,
                )? {
                    return Ok(true);
                }
            }
            return Ok(false);
        }
        if !metadata.is_file() {
            return Ok(false);
        }
        let file = File::open(root).map_err(io_error)?;
        let mut reader = BufReader::new(file);
        let mut raw_line = Vec::new();
        let mut file_matches = Vec::new();
        let folded_pattern = (!case_sensitive).then(|| pattern.to_lowercase());
        let mut line_number = 0usize;
        loop {
            raw_line.clear();
            let bytes_read = reader.read_until(b'\n', &mut raw_line).map_err(io_error)?;
            if bytes_read == 0 {
                break;
            }
            if is_binary(&raw_line) {
                return Ok(false);
            }
            let line = std::str::from_utf8(&raw_line)
                .map_err(|_error| WorkspaceToolError::BinaryPattern)?;
            line_number = line_number.saturating_add(1);
            let matches_pattern = folded_pattern.as_ref().map_or_else(
                || line.contains(pattern),
                |folded| line.to_lowercase().contains(folded),
            );
            if matches_pattern {
                let line = line.trim_end_matches(['\n', '\r']);
                let (preview, _) = bounded_text(line, DEFAULT_RESULT_PREVIEW_MAX_CHARS);
                file_matches.push(json!({
                    "path": relative,
                    "line": line_number,
                    "preview": preview,
                }));
                if matches.len().saturating_add(file_matches.len()) >= collection_limit {
                    matches.extend(
                        file_matches
                            .into_iter()
                            .take(collection_limit.saturating_sub(matches.len())),
                    );
                    return Ok(true);
                }
            }
        }
        matches.extend(file_matches);
        Ok(false)
    }

    /// 规范化请求路径，并拒绝越出工作区或包含受保护组件的路径。
    fn resolve_workspace_path(
        &self,
        path: &str,
        allow_protected: bool,
    ) -> Result<PathBuf, WorkspaceToolError> {
        let candidate = Path::new(path);
        let joined = if candidate.is_absolute() {
            candidate.to_path_buf()
        } else {
            self.workspace_root.join(candidate)
        };
        let normalized = normalize_path(&joined);
        let workspace = std::fs::canonicalize(&self.workspace_root).map_err(io_error)?;
        let resolved = canonicalize_existing_or_parent(&normalized)?;
        if !resolved.starts_with(&workspace) {
            return Err(WorkspaceToolError::OutsideWorkspace(path.to_string()));
        }
        let relative = resolved
            .strip_prefix(&workspace)
            .unwrap_or(resolved.as_path())
            .to_string_lossy()
            .replace('\\', "/");
        let intended_relative = normalized
            .strip_prefix(&workspace)
            .unwrap_or(normalized.as_path())
            .to_string_lossy()
            .replace('\\', "/");
        if !allow_protected
            && (is_protected_path(&relative) || is_protected_path(&intended_relative))
        {
            return Err(WorkspaceToolError::ProtectedPath(intended_relative));
        }
        Ok(resolved)
    }

    fn relative_path(&self, path: &Path) -> String {
        let workspace = std::fs::canonicalize(&self.workspace_root)
            .unwrap_or_else(|_| normalize_path(&self.workspace_root));
        normalize_path(path)
            .strip_prefix(&workspace)
            .unwrap_or(path)
            .to_string_lossy()
            .trim_start_matches(['/', '\\'])
            .replace('\\', "/")
    }
}

fn preflight_input<T>(input: &Value) -> Result<T, WorkspaceToolError>
where
    T: for<'de> Deserialize<'de>,
{
    serde_json::from_value(input.clone())
        .map_err(|_| WorkspaceToolError::InvalidInput("invalid tool input".to_string()))
}

#[derive(Debug, Clone)]
struct DirectoryEntry {
    path: PathBuf,
    relative: String,
    is_dir: bool,
    is_symlink_or_reparse: bool,
}

struct ListState {
    entries: Vec<DirectoryEntry>,
    redacted_entries: usize,
    truncated: bool,
    collection_limit: usize,
    recursive: bool,
    max_depth: usize,
}

fn default_case_sensitive() -> bool {
    true
}

fn validate_limit(
    name: &str,
    value: Option<usize>,
    default: usize,
    maximum: usize,
) -> Result<usize, WorkspaceToolError> {
    let value = value.unwrap_or(default);
    if value == 0 {
        return Err(WorkspaceToolError::InvalidInput(format!(
            "{name} must be greater than zero"
        )));
    }
    if value > maximum {
        return Err(WorkspaceToolError::InvalidInput(format!(
            "{name} must not exceed {maximum}"
        )));
    }
    Ok(value)
}

fn validate_line_range(line_start: usize, line_end: usize) -> Result<(), WorkspaceToolError> {
    if line_start == 0 {
        return Err(WorkspaceToolError::InvalidInput(
            "line_start must be greater than zero".to_string(),
        ));
    }
    if line_end == 0 {
        return Err(WorkspaceToolError::InvalidInput(
            "line_end must be greater than zero".to_string(),
        ));
    }
    if line_end < line_start {
        return Err(WorkspaceToolError::InvalidInput(
            "line_end must be greater than or equal to line_start".to_string(),
        ));
    }
    Ok(())
}

fn validate_nonempty_path(name: &str, path: &str) -> Result<(), WorkspaceToolError> {
    if path.trim().is_empty() {
        return Err(WorkspaceToolError::InvalidInput(format!(
            "{name} must not be empty"
        )));
    }
    Ok(())
}

fn metadata_is_symlink_or_reparse(metadata: &Metadata) -> bool {
    metadata.file_type().is_symlink() || metadata_is_reparse_point(metadata)
}

#[cfg(windows)]
fn metadata_is_reparse_point(metadata: &Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn metadata_is_reparse_point(_metadata: &Metadata) -> bool {
    false
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CommandModelInput {
    argv: Vec<String>,
    cwd: Option<String>,
    timeout_seconds: Option<u64>,
}

impl CommandModelInput {
    fn into_execution_input(self) -> CommandToolInput {
        CommandToolInput {
            argv: self.argv,
            cwd: self.cwd,
            timeout_seconds: self.timeout_seconds,
            sandbox_mode: None,
            network_access: None,
        }
    }
}

/// 面向模型的命令输入；沙箱和网络模式稍后由执行路径应用。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CommandToolInput {
    pub argv: Vec<String>,
    pub cwd: Option<String>,
    pub timeout_seconds: Option<u64>,
    pub sandbox_mode: Option<SandboxFilesystemMode>,
    pub network_access: Option<SandboxNetworkMode>,
}

impl CommandToolInput {
    fn validate(&self) -> Result<(), WorkspaceToolError> {
        if self.argv.is_empty() {
            return Err(WorkspaceToolError::InvalidInput(
                "argv must contain at least one argument".to_string(),
            ));
        }
        if self.argv[0].trim().is_empty() {
            return Err(WorkspaceToolError::InvalidInput(
                "argv[0] must not be empty".to_string(),
            ));
        }
        if self.cwd.as_deref().is_some_and(|cwd| cwd.trim().is_empty()) {
            return Err(WorkspaceToolError::InvalidInput(
                "cwd must not be empty".to_string(),
            ));
        }
        if self.timeout_seconds == Some(0) {
            return Err(WorkspaceToolError::InvalidInput(
                "timeout_seconds must be greater than zero".to_string(),
            ));
        }
        if self
            .timeout_seconds
            .is_some_and(|timeout| timeout > MAX_COMMAND_TIMEOUT_SECONDS)
        {
            return Err(WorkspaceToolError::InvalidInput(format!(
                "timeout_seconds must not exceed {MAX_COMMAND_TIMEOUT_SECONDS}"
            )));
        }
        Ok(())
    }

    pub fn effective_cwd(&self) -> &str {
        self.cwd.as_deref().unwrap_or(".")
    }

    pub fn effective_timeout_seconds(&self) -> u64 {
        self.timeout_seconds
            .unwrap_or(DEFAULT_COMMAND_TIMEOUT_SECONDS)
    }

    pub fn sandbox_mode(&self) -> SandboxFilesystemMode {
        self.sandbox_mode
            .clone()
            .unwrap_or(SandboxFilesystemMode::ReadOnly)
    }

    pub fn network_access(&self) -> SandboxNetworkMode {
        self.network_access
            .clone()
            .unwrap_or(SandboxNetworkMode::Denied)
    }
}

#[derive(Serialize)]
struct CommandScope<'a> {
    argv: &'a [String],
    cwd: &'a str,
    timeout_seconds: u64,
    sandbox_mode: &'a SandboxFilesystemMode,
    network_access: &'a SandboxNetworkMode,
}

impl<'a> CommandScope<'a> {
    fn new(
        argv: &'a [String],
        cwd: &'a str,
        timeout_seconds: u64,
        sandbox_mode: &'a SandboxFilesystemMode,
        network_access: &'a SandboxNetworkMode,
    ) -> Self {
        Self {
            argv,
            cwd,
            timeout_seconds,
            sandbox_mode,
            network_access,
        }
    }

    fn encoded(&self) -> String {
        serde_json::to_string(self).expect("command scope is serializable")
    }

    fn digest(&self) -> String {
        format!("sha256:{:x}", Sha256::digest(self.encoded().as_bytes()))
    }
}

/// 为命令及其生效执行范围构建可审计的权限资源标识。
pub fn command_scope_resource(
    argv: &[String],
    cwd: &str,
    timeout_seconds: u64,
    sandbox_mode: &SandboxFilesystemMode,
    network_access: &SandboxNetworkMode,
) -> String {
    let command = command_permission_resource(argv);
    if command.is_empty() {
        String::new()
    } else {
        let scope = CommandScope::new(argv, cwd, timeout_seconds, sandbox_mode, network_access);
        format!(
            "command:{command};scope:{};digest:{}",
            scope.encoded(),
            scope.digest()
        )
    }
}

/// 对命令、`cwd`、超时、文件系统模式和网络模式计算哈希，用于校验绑定。
pub fn command_scope_digest(
    argv: &[String],
    cwd: &str,
    timeout_seconds: u64,
    sandbox_mode: &SandboxFilesystemMode,
    network_access: &SandboxNetworkMode,
) -> String {
    CommandScope::new(argv, cwd, timeout_seconds, sandbox_mode, network_access).digest()
}

fn validate_tool_name(name: &str) -> Result<(), String> {
    let Some(tool) = name.strip_prefix("builtin_") else {
        return Err(format!("tool name must use builtin_<tool>: {name}"));
    };
    if tool.is_empty()
        || name.len() > 64
        || !tool
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
    {
        Err(format!("tool name is not provider-portable: {name}"))
    } else {
        Ok(())
    }
}

fn redact_public_text(text: &str) -> String {
    let lowered = text.to_ascii_lowercase();
    if contains_sensitive_text(text)
        || PROMPT_INJECTION_MARKERS
            .iter()
            .any(|marker| lowered.contains(marker))
    {
        REDACTED_TOOL_OUTPUT.to_string()
    } else {
        text.to_string()
    }
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

fn canonicalize_existing_or_parent(path: &Path) -> Result<PathBuf, WorkspaceToolError> {
    if path.exists() {
        return std::fs::canonicalize(path).map_err(io_error);
    }
    let mut missing_components = Vec::new();
    let mut ancestor = path;
    while !ancestor.exists() {
        let name = ancestor.file_name().ok_or_else(|| {
            WorkspaceToolError::ReadFailed(format!("path does not exist: {}", path.display()))
        })?;
        missing_components.push(name.to_owned());
        ancestor = ancestor.parent().ok_or_else(|| {
            WorkspaceToolError::ReadFailed(format!("path has no parent: {}", path.display()))
        })?;
    }
    let mut resolved = std::fs::canonicalize(ancestor).map_err(io_error)?;
    for component in missing_components.iter().rev() {
        resolved.push(component);
    }
    Ok(normalize_path(&resolved))
}

/// 判断规范化路径是否包含受保护或类似敏感信息的组件。
pub fn is_protected_path(path: &str) -> bool {
    path.replace('\\', "/")
        .split('/')
        .map(str::to_ascii_lowercase)
        .any(|component| is_protected_component(&component))
}

fn is_protected_component(component: &str) -> bool {
    PROTECTED_PATH_EXACT_MARKERS.contains(&component)
        || PROTECTED_PATH_PREFIXES.iter().any(|prefix| {
            component == *prefix
                || component
                    .strip_prefix(*prefix)
                    .is_some_and(|suffix| suffix.starts_with('.'))
        })
        || PROTECTED_PATH_SUFFIXES
            .iter()
            .any(|suffix| component.ends_with(suffix))
        || component.contains("secret")
}

fn is_binary(bytes: &[u8]) -> bool {
    bytes.contains(&0)
}

fn bounded_text(content: &str, max_chars: usize) -> (String, bool) {
    let preview = content.chars().take(max_chars).collect::<String>();
    let truncated = content.chars().count() > preview.chars().count();
    (preview, truncated)
}

fn artifact_ref(prefix: &str, path: &str) -> String {
    let sanitized = path
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '_' })
        .collect::<String>();
    format!("{prefix}{sanitized}")
}

/// 通过临时文件替换一个文件，使调用方能够回滚多文件变更。
fn atomic_write(path: &Path, content: &str) -> Result<(), WorkspaceToolError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(io_error)?;
    }
    let (temp_path, mut temp_file) = create_unique_temp_file(path)?;
    if let Err(error) = temp_file
        .write_all(content.as_bytes())
        .and_then(|()| temp_file.sync_all())
    {
        drop(temp_file);
        let _ = std::fs::remove_file(&temp_path);
        return Err(io_error(error));
    }
    drop(temp_file);
    std::fs::rename(&temp_path, path).map_err(|error| {
        let _ = std::fs::remove_file(&temp_path);
        io_error(error)
    })
}

fn create_unique_temp_file(path: &Path) -> Result<(PathBuf, File), WorkspaceToolError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("workspace-file");
    for _ in 0..MUTATION_TEMP_FILE_ATTEMPTS {
        let sequence = MUTATION_TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let temp_path = parent.join(format!(
            ".{file_name}.singularity-tmp-{}-{sequence}",
            std::process::id()
        ));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
        {
            Ok(file) => return Ok((temp_path, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(io_error(error)),
        }
    }
    Err(WorkspaceToolError::ReadFailed(format!(
        "failed to allocate unique temporary file for {}",
        path.display()
    )))
}

fn existing_text_or_empty(path: &Path) -> Result<String, WorkspaceToolError> {
    match std::fs::read_to_string(path) {
        Ok(content) => Ok(content),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(io_error(error)),
    }
}

fn rollback_originals(
    originals: &[(PathBuf, String, String, bool)],
) -> Result<(), WorkspaceToolError> {
    let mut failures = Vec::new();
    for (path, relative, original, existed) in originals {
        let result = if *existed {
            atomic_write(path, original)
        } else if path.exists() {
            std::fs::remove_file(path).map_err(io_error)
        } else {
            Ok(())
        };
        if let Err(error) = result {
            failures.push(format!("{relative}: {error}"));
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(WorkspaceToolError::RollbackFailed(failures.join("; ")))
    }
}

fn io_error(error: std::io::Error) -> WorkspaceToolError {
    WorkspaceToolError::ReadFailed(error.to_string())
}

fn next_command_id() -> String {
    let sequence = COMMAND_ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("command_{sequence}")
}

fn command_tool_output(result: CommandResult) -> ToolOutput {
    let ok = result.execution_status == CommandExecutionStatus::Completed
        && result.semantic_status == CommandSemanticStatus::Succeeded;
    let content = serde_json::to_value(&result).expect("command result serializes");
    if ok {
        ToolOutput::success(content)
    } else {
        ToolOutput::failure_with_kind(
            command_failure_kind(&result),
            command_error_code(&result),
            content,
        )
    }
}

fn command_failure_kind(result: &CommandResult) -> ToolFailureKind {
    match result.execution_status {
        CommandExecutionStatus::PolicyDenied => ToolFailureKind::Policy,
        CommandExecutionStatus::ReviewRequired => ToolFailureKind::Approval,
        CommandExecutionStatus::Unsupported => ToolFailureKind::Capability,
        CommandExecutionStatus::ExecutableUnavailable => ToolFailureKind::Capability,
        CommandExecutionStatus::TimedOut => ToolFailureKind::Timeout,
        CommandExecutionStatus::Cancelled => ToolFailureKind::Cancelled,
        CommandExecutionStatus::BackendError => ToolFailureKind::Backend,
        CommandExecutionStatus::Completed => match result.semantic_status {
            CommandSemanticStatus::Succeeded
            | CommandSemanticStatus::ExitNonzero
            | CommandSemanticStatus::TestsFailed
            | CommandSemanticStatus::BuildFailed => ToolFailureKind::Execution,
            CommandSemanticStatus::PolicyBlocked => ToolFailureKind::Policy,
            CommandSemanticStatus::Unsupported => ToolFailureKind::Capability,
            CommandSemanticStatus::TimedOut => ToolFailureKind::Timeout,
            CommandSemanticStatus::Cancelled => ToolFailureKind::Cancelled,
        },
    }
}

fn command_error_code(result: &CommandResult) -> &'static str {
    match result.execution_status {
        CommandExecutionStatus::PolicyDenied => "command_policy_denied",
        CommandExecutionStatus::ReviewRequired => "command_review_required",
        CommandExecutionStatus::Unsupported => "command_unsupported",
        CommandExecutionStatus::ExecutableUnavailable => "command_executable_unavailable",
        CommandExecutionStatus::TimedOut => "command_timed_out",
        CommandExecutionStatus::Cancelled => "command_cancelled",
        CommandExecutionStatus::BackendError => TOOL_SANDBOX_UNAVAILABLE_ERROR,
        CommandExecutionStatus::Completed => match result.semantic_status {
            CommandSemanticStatus::Succeeded => "command_succeeded",
            CommandSemanticStatus::ExitNonzero => "command_exit_nonzero",
            CommandSemanticStatus::TestsFailed => "command_tests_failed",
            CommandSemanticStatus::BuildFailed => "command_build_failed",
            CommandSemanticStatus::PolicyBlocked => "command_policy_blocked",
            CommandSemanticStatus::Unsupported => "command_unsupported",
            CommandSemanticStatus::TimedOut => "command_timed_out",
            CommandSemanticStatus::Cancelled => "command_cancelled",
        },
    }
}
