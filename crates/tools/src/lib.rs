#![forbid(unsafe_code)]

//! tool 模式、tool 代理器决策、工作区操作和公开 tool 结果投影。
//!
//! tool 代理器会在执行边界再次校验面向模型的输入；`WorkspaceTools` 则在任何文件系统副作用前
//! 强制执行工作区和受保护路径规则。

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io::{Read, Write};
#[cfg(windows)]
use std::os::windows::fs::{MetadataExt as StdMetadataExt, OpenOptionsExt as StdOpenOptionsExt};
#[cfg(windows)]
use std::path::Prefix;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use cap_fs_ext::{DirEntryExt as _, DirExt as _, FollowSymlinks, OpenOptionsFollowExt as _};
use cap_std::ambient_authority;
use cap_std::fs::{
    Dir as CapabilityDir, File as CapabilityFile, Metadata as CapabilityMetadata,
    OpenOptions as CapabilityOpenOptions, Permissions as CapabilityPermissions,
};
#[cfg(windows)]
use cap_std::fs::{MetadataExt as _, OpenOptionsExt as _};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
pub use singularity_core::is_protected_path;
use singularity_core::{CancellationToken, contains_sensitive_text};
pub use singularity_policy::{
    CommandScopeDigest, PermissionOperation, PermissionResource, ToolId, WorkspaceRelativePath,
};
pub use singularity_sandbox::{
    CommandEnvironmentPolicy, CommandExecutionStatus, CommandRequest, CommandResult,
    CommandScriptRequest, CommandSemanticStatus, DEFAULT_COMMAND_TIMEOUT_SECONDS, SandboxBackend,
    SandboxBackendEnforcement, SandboxCapabilities, SandboxFilesystemMode, SandboxNetworkMode,
    WorkspaceMutation,
};

const REDACTED_TOOL_OUTPUT: &str = "[redacted sensitive tool output]";
const UNKNOWN_TOOL_ERROR: &str = "unknown_tool";
const INVALID_TOOL_ARGUMENTS_ERROR: &str = "invalid_tool_arguments";
const TOOL_DENIED_ERROR: &str = "tool_denied";
const TOOL_APPROVAL_REQUIRED_ERROR: &str = "approval_required";
const TOOL_SANDBOX_UNAVAILABLE_ERROR: &str = "sandbox_unavailable";
const TOOL_CONTRACT_INVALID_ERROR: &str = "tool_contract_invalid";
const WORKSPACE_MUTATION_NOT_APPROVED: &str = "workspace mutation requires allowed tool decision";
const WORKSPACE_OBSERVATION_METADATA: &str = "workspace_observation";
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
const MAX_COMMAND_SCRIPT_CHARS: usize = 8_000;
const DEFAULT_RESULT_PREVIEW_MAX_CHARS: usize = 4_096;
const APPROXIMATE_ASCII_CHARS_PER_TOKEN: usize = 4;
const FILE_READ_CHUNK_SIZE: usize = 8 * 1024;
const BINARY_CONTENT_PREVIEW: &str = "[binary content omitted]";
const ARTIFACT_REFERENCE_OMITTED: &str = "[artifact reference omitted]";
const TRUNCATED_OUTPUT_OMITTED: &str = "[truncated output omitted]";
const MAX_TRUNCATED_SUMMARY_STRING_CHARS: usize = 512;
const TRUNCATED_RAW_OUTPUT_KEYS: [&str; 14] = [
    "api_key",
    "authorization",
    "body",
    "content",
    "data",
    "full_output",
    "output",
    "password",
    "raw",
    "raw_output",
    "secret",
    "stderr",
    "stdout",
    "token",
];
const PROMPT_INJECTION_MARKERS: [&str; 4] = [
    "developer message",
    "ignore previous",
    "reveal hidden",
    "system prompt",
];
#[cfg(windows)]
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
#[cfg(windows)]
const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
#[cfg(windows)]
const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
#[cfg(windows)]
const FILE_SHARE_READ: u32 = 0x0000_0001;
#[cfg(windows)]
const FILE_SHARE_WRITE: u32 = 0x0000_0002;
#[cfg(unix)]
const ERROR_TOO_MANY_SYMLINKS: i32 = 40;
#[cfg(windows)]
const ERROR_STOPPED_ON_SYMLINK: i32 = 681;
/// 核心 read tool 名称。
pub const READ_TOOL: &str = "read";
/// 核心 list tool 名称。
pub const LIST_TOOL: &str = "list";
/// 核心 grep tool 名称。
pub const GREP_TOOL: &str = "grep";
/// 核心 edit tool 名称。
pub const EDIT_TOOL: &str = "edit";
/// 核心 patch tool 名称。
pub const PATCH_TOOL: &str = "patch";
/// 核心 command tool 名称。
pub const COMMAND_TOOL: &str = "command";
static COMMAND_ID_COUNTER: AtomicU64 = AtomicU64::new(0);
static MUTATION_TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// tool call 可以与其他只读调用并行，还是必须独占运行。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ToolExecutionMode {
    ParallelRead,
    Exclusive,
}

/// 运行时可用于能力协商与 Evaluation 要求映射的稳定能力标识。
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ToolCapability {
    WorkspaceRead,
    WorkspaceSearch,
    WorkspaceWrite,
    CommandExecution,
    PlanManagement,
}

/// 工作区执行器的类型化入口；名称到执行器的绑定只存在于注册表。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceToolExecutor {
    Read,
    List,
    Grep,
    Edit,
    Patch,
    Command,
}

/// Agent 内部状态执行器的类型化入口。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AgentControlToolExecutor {
    UpdatePlan,
}

/// 注册表绑定的真实执行器。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", content = "executor", rename_all = "snake_case")]
pub enum ToolExecutor {
    Workspace(WorkspaceToolExecutor),
    AgentControl(AgentControlToolExecutor),
}

/// 注册表绑定的授权投影合同。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ToolAuthorization {
    WorkspaceRead,
    WorkspaceWrite,
    Command,
    AgentControl,
}

/// 工具是否投影到模型边界。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ToolExposure {
    Model,
    Internal,
}

/// 每个 typed executor 的稳定能力、授权和并发合同。
#[derive(Debug, Clone, Copy)]
struct ToolExecutorContract {
    capability: ToolCapability,
    authorization: ToolAuthorization,
    execution_mode: ToolExecutionMode,
}

impl ToolExecutor {
    fn contract(self) -> ToolExecutorContract {
        match self {
            Self::Workspace(WorkspaceToolExecutor::Read | WorkspaceToolExecutor::List) => {
                ToolExecutorContract {
                    capability: ToolCapability::WorkspaceRead,
                    authorization: ToolAuthorization::WorkspaceRead,
                    execution_mode: ToolExecutionMode::ParallelRead,
                }
            }
            Self::Workspace(WorkspaceToolExecutor::Grep) => ToolExecutorContract {
                capability: ToolCapability::WorkspaceSearch,
                authorization: ToolAuthorization::WorkspaceRead,
                execution_mode: ToolExecutionMode::ParallelRead,
            },
            Self::Workspace(WorkspaceToolExecutor::Edit | WorkspaceToolExecutor::Patch) => {
                ToolExecutorContract {
                    capability: ToolCapability::WorkspaceWrite,
                    authorization: ToolAuthorization::WorkspaceWrite,
                    execution_mode: ToolExecutionMode::Exclusive,
                }
            }
            Self::Workspace(WorkspaceToolExecutor::Command) => ToolExecutorContract {
                capability: ToolCapability::CommandExecution,
                authorization: ToolAuthorization::Command,
                execution_mode: ToolExecutionMode::Exclusive,
            },
            Self::AgentControl(AgentControlToolExecutor::UpdatePlan) => ToolExecutorContract {
                capability: ToolCapability::PlanManagement,
                authorization: ToolAuthorization::AgentControl,
                execution_mode: ToolExecutionMode::Exclusive,
            },
        }
    }
}

/// tool 到达执行阶段前返回的结构化校验代码。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ToolInputValidationError {
    pub code: String,
}

impl ToolInputValidationError {
    /// 创建带稳定校验代码的输入错误。
    pub fn new(code: impl Into<String>) -> Self {
        Self { code: code.into() }
    }
}

/// tool 输入的本地校验函数签名。
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
    /// 创建同时用于模型 schema 和执行校验的 tool 定义。
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

    /// 替换执行边界使用的输入校验器。
    pub fn with_execution_input_validator(mut self, validator: ToolInputValidator) -> Self {
        self.execution_input_validator = validator;
        self
    }

    /// 校验并投影模型提交的输入。
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

    /// 在执行器闸门重新校验可执行输入。
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

    /// 将 tool 限制为一组模型与执行输入完全相同的值。
    pub fn restrict_to_exact_inputs(&mut self, inputs: Vec<Value>) -> Result<(), String> {
        self.restrict_to_input_bindings(
            inputs
                .into_iter()
                .map(|input| (input.clone(), input))
                .collect(),
        )
    }

    /// 设置模型输入到执行输入的显式绑定。
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

/// 单一工具事实源：定义、能力、模型暴露、授权投影和执行器绑定。
#[derive(Debug, Clone)]
pub struct ToolEntry {
    pub id: ToolId,
    pub version: u32,
    pub capability: ToolCapability,
    pub exposure: ToolExposure,
    pub authorization: ToolAuthorization,
    pub executor: ToolExecutor,
    pub spec: ToolSpec,
}

/// 模型输入经注册表和工作区边界绑定后的可执行调用与授权资源。
#[derive(Debug, Clone, PartialEq)]
pub struct BoundToolCall {
    pub tool_id: ToolId,
    pub execution_mode: ToolExecutionMode,
    pub executor: ToolExecutor,
    pub operation: PermissionOperation,
    pub arguments: Value,
    pub resources: Vec<PermissionResource>,
    sensitive_resources: BTreeSet<PermissionResource>,
}

impl BoundToolCall {
    /// 判断某个资源是否在工作区边界被标记为受保护资源。
    pub fn resource_is_sensitive(&self, resource: &PermissionResource) -> bool {
        self.sensitive_resources.contains(resource)
    }
}

impl ToolEntry {
    /// 创建一个模型可见且拥有真实执行器的版本化工具条目。
    pub fn model(
        spec: ToolSpec,
        version: u32,
        capability: ToolCapability,
        authorization: ToolAuthorization,
        executor: ToolExecutor,
    ) -> Result<Self, String> {
        Self::with_exposure(
            spec,
            version,
            ToolExposure::Model,
            capability,
            authorization,
            executor,
        )
    }

    /// 创建仅供显式内部调用使用的版本化工具条目。
    pub fn internal(
        spec: ToolSpec,
        version: u32,
        capability: ToolCapability,
        authorization: ToolAuthorization,
        executor: ToolExecutor,
    ) -> Result<Self, String> {
        Self::with_exposure(
            spec,
            version,
            ToolExposure::Internal,
            capability,
            authorization,
            executor,
        )
    }

    fn with_exposure(
        spec: ToolSpec,
        version: u32,
        exposure: ToolExposure,
        capability: ToolCapability,
        authorization: ToolAuthorization,
        executor: ToolExecutor,
    ) -> Result<Self, String> {
        let id = ToolId::new(spec.name.clone())?;
        let entry = Self {
            id,
            version,
            capability,
            exposure,
            authorization,
            executor,
            spec,
        };
        entry.validate_consistency()?;
        Ok(entry)
    }

    fn validate_consistency(&self) -> Result<(), String> {
        if self.version == 0 {
            return Err(format!(
                "tool {} version must be positive",
                self.id.as_str()
            ));
        }
        if self.spec.name != self.id.as_str() {
            return Err("tool entry id and schema name differ".to_string());
        }

        let expected = self.executor.contract();
        if self.capability != expected.capability {
            return Err(format!(
                "tool {} capability does not match its executor contract",
                self.id.as_str()
            ));
        }
        if self.authorization != expected.authorization {
            return Err(format!(
                "tool {} authorization does not match its executor contract",
                self.id.as_str()
            ));
        }
        if self.spec.execution_mode != expected.execution_mode {
            return Err(format!(
                "tool {} execution mode does not match its executor contract",
                self.id.as_str()
            ));
        }
        Ok(())
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
    match input.get("command") {
        None => "missing_command",
        Some(Value::String(_)) => "invalid_command_arguments",
        Some(_) => "command_not_string",
    }
}

fn validate_command_model_input(input: &Value) -> Result<(), ToolInputValidationError> {
    let validation_code = command_input_validation_code(input);
    let input: CommandModelInput = deserialize_tool_input(input, validation_code)?;
    input
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

/// 返回工作区工具的完整注册表条目；schema、能力、授权与执行器在同一处声明。
pub fn workspace_tool_entries() -> Vec<ToolEntry> {
    vec![
        workspace_tool_entry(
            ToolSpec::new(
                READ_TOOL,
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
            ToolCapability::WorkspaceRead,
            ToolAuthorization::WorkspaceRead,
            WorkspaceToolExecutor::Read,
        ),
        workspace_tool_entry(
            ToolSpec::new(
                LIST_TOOL,
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
            ToolCapability::WorkspaceRead,
            ToolAuthorization::WorkspaceRead,
            WorkspaceToolExecutor::List,
        ),
        workspace_tool_entry(
            ToolSpec::new(
                GREP_TOOL,
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
            ToolCapability::WorkspaceSearch,
            ToolAuthorization::WorkspaceRead,
            WorkspaceToolExecutor::Grep,
        ),
        workspace_tool_entry(
            ToolSpec::new(
                EDIT_TOOL,
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
            ToolCapability::WorkspaceWrite,
            ToolAuthorization::WorkspaceWrite,
            WorkspaceToolExecutor::Edit,
        ),
        workspace_tool_entry(
            ToolSpec::new(
                PATCH_TOOL,
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
            ToolCapability::WorkspaceWrite,
            ToolAuthorization::WorkspaceWrite,
            WorkspaceToolExecutor::Patch,
        ),
        workspace_tool_entry(
            ToolSpec::new(
                COMMAND_TOOL,
                "Run a bounded sandboxed command",
                json!({
                    "type": "object",
                    "properties": {
                        "command": {"type": "string", "minLength": 1, "maxLength": MAX_COMMAND_SCRIPT_CHARS},
                        "cwd": {"type": "string", "minLength": 1},
                        "timeout_seconds": {"type": "integer", "minimum": 1, "maximum": MAX_COMMAND_TIMEOUT_SECONDS}
                    },
                    "required": ["command"],
                    "additionalProperties": false
                }),
                ToolExecutionMode::Exclusive,
                validate_command_model_input,
            )
            .with_execution_input_validator(validate_command_execution_input),
            ToolCapability::CommandExecution,
            ToolAuthorization::Command,
            WorkspaceToolExecutor::Command,
        )
    ]
}

/// 返回内置工作区工具的模型定义；视图从完整注册表条目投影。
pub fn workspace_tool_specs() -> Vec<ToolSpec> {
    workspace_tool_entries()
        .into_iter()
        .map(|entry| entry.spec)
        .collect()
}

fn workspace_tool_entry(
    spec: ToolSpec,
    capability: ToolCapability,
    authorization: ToolAuthorization,
    executor: WorkspaceToolExecutor,
) -> ToolEntry {
    ToolEntry::model(
        spec,
        1,
        capability,
        authorization,
        ToolExecutor::Workspace(executor),
    )
    .expect("built-in workspace tool entry is valid")
}

/// 负责管理模型暴露、能力、授权投影和执行器绑定的唯一工具注册表。
#[derive(Debug, Default, Clone)]
pub struct ToolRegistry {
    tools: BTreeMap<String, ToolEntry>,
}

impl ToolRegistry {
    /// 注册一个新的 tool 定义。
    pub fn register(&mut self, entry: ToolEntry) -> Result<(), String> {
        validate_tool_name(entry.id.as_str())?;
        entry.validate_consistency()?;
        if self.tools.contains_key(entry.id.as_str()) {
            return Err(format!("tool already registered: {}", entry.id.as_str()));
        }
        self.tools.insert(entry.id.as_str().to_string(), entry);
        Ok(())
    }

    /// 按名称查找完整 tool 条目。
    pub fn entry(&self, name: &str) -> Option<&ToolEntry> {
        self.tools.get(name)
    }

    /// 按名称查找 tool 定义。
    pub fn get(&self, name: &str) -> Option<&ToolSpec> {
        self.entry(name).map(|entry| &entry.spec)
    }

    /// 返回全部模型可见的 schema payload。
    pub fn schema_payloads(&self) -> Vec<Value> {
        self.tools
            .values()
            .filter(|entry| entry.exposure == ToolExposure::Model)
            .map(|entry| entry.spec.to_schema_payload())
            .collect::<Vec<_>>()
    }

    /// 返回满足版本化能力要求的模型可见条目。
    pub fn entries_for_capability(
        &self,
        capability: ToolCapability,
        minimum_version: u32,
    ) -> Vec<&ToolEntry> {
        self.tools
            .values()
            .filter(|entry| {
                entry.exposure == ToolExposure::Model
                    && entry.capability == capability
                    && entry.version >= minimum_version
            })
            .collect()
    }

    /// 校验并准备指定 tool 的模型输入。
    pub fn prepare_model_input(
        &self,
        name: &str,
        input: &Value,
    ) -> Result<(ToolExecutionMode, Value), ToolInputValidationError> {
        let entry = self
            .entry(name)
            .filter(|entry| entry.exposure == ToolExposure::Model)
            .ok_or_else(|| ToolInputValidationError::new("tool_not_visible"))?;
        let execution_input = entry.spec.prepare_model_input(input)?;
        Ok((entry.spec.execution_mode, execution_input))
    }

    /// 校验指定 tool 的执行输入。
    pub fn validate_execution_input(
        &self,
        name: &str,
        input: &Value,
    ) -> Result<ToolExecutionMode, ToolInputValidationError> {
        let entry = self
            .entry(name)
            .ok_or_else(|| ToolInputValidationError::new("tool_not_visible"))?;
        entry.spec.validate_execution_input(input)?;
        Ok(entry.spec.execution_mode)
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

/// 决定 tool 代理器执行、拒绝还是暂停调用的授权结果。
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
    /// 构造策略拒绝决策。
    pub fn deny(reason: impl Into<String>) -> Self {
        Self::deny_with_kind(ToolFailureKind::Policy, reason)
    }

    /// 构造带失败类别的拒绝决策。
    pub fn deny_with_kind(failure_kind: ToolFailureKind, reason: impl Into<String>) -> Self {
        Self::Deny {
            failure_kind,
            reason: reason.into(),
        }
    }

    /// 判断决策是否允许进入执行边界。
    pub fn is_allowed(&self) -> bool {
        matches!(self, Self::Allow | Self::Approved { .. })
    }

    /// 构造 approval 已授予的决策。
    pub fn approved(approval_grant_id: impl Into<String>) -> Self {
        Self::Approved {
            approval_grant_id: approval_grant_id.into(),
        }
    }

    /// 构造等待 approval 的决策。
    pub fn ask(approval_request_id: impl Into<String>, reason: impl Into<String>) -> Self {
        Self::Ask {
            approval_request_id: approval_request_id.into(),
            reason: reason.into(),
        }
    }
}

/// 执行边界；调用执行器闭包前会重新校验 tool 输入。
#[derive(Debug, Default, Clone)]
pub struct ToolBroker {
    registry: ToolRegistry,
}

impl ToolBroker {
    /// 用已注册的 tool 表创建执行边界。
    pub fn new(registry: ToolRegistry) -> Self {
        Self { registry }
    }

    /// 注册 tool 并保持 broker 的名称约束。
    pub fn register(&mut self, entry: ToolEntry) -> Result<(), String> {
        self.registry.register(entry)
    }

    /// 按名称查找 tool 定义。
    pub fn get(&self, name: &str) -> Option<&ToolSpec> {
        self.registry.get(name)
    }

    /// 按名称查找完整工具条目。
    pub fn entry(&self, name: &str) -> Option<&ToolEntry> {
        self.registry.entry(name)
    }

    /// 返回满足版本化能力要求的注册条目。
    pub fn entries_for_capability(
        &self,
        capability: ToolCapability,
        minimum_version: u32,
    ) -> Vec<&ToolEntry> {
        self.registry
            .entries_for_capability(capability, minimum_version)
    }

    /// 返回 broker 暴露给模型的 schema。
    pub fn tool_schema_payloads(&self) -> Vec<Value> {
        self.registry.schema_payloads()
    }

    /// 准备模型提交的 tool 输入。
    pub fn prepare_model_input(
        &self,
        name: &str,
        input: &Value,
    ) -> Result<(ToolExecutionMode, Value), ToolInputValidationError> {
        self.registry.prepare_model_input(name, input)
    }

    /// 由注册表绑定执行器，并由工作区边界投影类型化授权资源。
    pub fn bind_authorization(
        &self,
        name: &str,
        input: Value,
        workspace_tools: Option<&WorkspaceTools>,
        filesystem: SandboxFilesystemMode,
        network: SandboxNetworkMode,
    ) -> Result<BoundToolCall, WorkspaceToolError> {
        let entry = self.registry.entry(name).ok_or_else(|| {
            WorkspaceToolError::InvalidInput("tool is not registered".to_string())
        })?;
        entry
            .validate_consistency()
            .map_err(WorkspaceToolError::InvalidInput)?;
        match entry.executor {
            ToolExecutor::Workspace(executor) => {
                let workspace_tools = workspace_tools.ok_or_else(|| {
                    WorkspaceToolError::InvalidInput(
                        "workspace tool backend is unavailable".to_string(),
                    )
                })?;
                workspace_tools.bind_tool_call(entry, executor, input, filesystem, network)
            }
            ToolExecutor::AgentControl(_) => Ok(BoundToolCall {
                tool_id: entry.id.clone(),
                execution_mode: entry.spec.execution_mode,
                executor: entry.executor,
                operation: PermissionOperation::Read,
                arguments: input,
                resources: vec![PermissionResource::Tool(entry.id.clone())],
                sensitive_resources: BTreeSet::new(),
            }),
        }
    }

    /// 在执行前校验 tool 输入。
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
        F: FnOnce(ToolExecutor, &ToolCallRequest) -> ToolOutput,
    {
        let Some(entry) = self.registry.entry(&envelope.tool_name) else {
            return ToolResult::failed_with_kind(
                envelope,
                ToolFailureKind::Visibility,
                UNKNOWN_TOOL_ERROR,
                "tool is not registered",
            );
        };
        if entry.validate_consistency().is_err() {
            return ToolResult::failed_with_kind(
                envelope,
                ToolFailureKind::Infrastructure,
                TOOL_CONTRACT_INVALID_ERROR,
                "tool registration contract is invalid",
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
        ToolResult::from_result(envelope, &executor(entry.executor, envelope))
    }
}

/// 从模型 tool call 传给 tool 代理器和执行器的规范化封装结构。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ToolCallRequest {
    pub tool_call_id: String,
    pub tool_name: String,
    pub raw_arguments: String,
}

impl ToolCallRequest {
    /// 创建规范化的 tool call 封装。
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

/// tool 代理器对其公开投影进行有界化和脱敏前的执行器原始输出。
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
    /// 构造成功输出。
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

    /// 构造默认执行失败输出。
    pub fn failure(error_code: impl Into<String>, content: Value) -> Self {
        Self::failure_with_kind(ToolFailureKind::Execution, error_code, content)
    }

    /// 构造带稳定失败类别的输出。
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
    #[serde(skip)]
    pub result_id: Option<String>,
    #[serde(skip)]
    #[schemars(skip)]
    context_token_count: Option<u32>,
    pub truncated: bool,
    #[serde(skip)]
    audit_metadata: Option<Value>,
    #[serde(skip)]
    #[schemars(skip)]
    workspace_observation: Option<WorkspaceObservation>,
}

impl ToolResult {
    /// 构造仅含有界预览的结果。
    pub fn summary(
        tool_call_id: impl Into<String>,
        tool_name: impl Into<String>,
        ok: bool,
        preview: impl Into<String>,
    ) -> Self {
        let mut result = Self {
            tool_call_id: tool_call_id.into(),
            tool_name: tool_name.into(),
            ok,
            content: None,
            preview: Some(redact_public_text(&preview.into())),
            error_code: None,
            failure_kind: None,
            result_id: None,
            context_token_count: None,
            truncated: false,
            audit_metadata: None,
            workspace_observation: None,
        };
        result.context_token_count = Some(approximate_token_count(
            &result.to_message_payload().to_string(),
        ));
        result
    }

    /// 附加内部审计 metadata，不进入模型公开内容。
    pub fn with_audit(mut self, metadata: Value) -> Self {
        self.audit_metadata = Some(metadata);
        self
    }

    /// 返回内部审计 metadata。
    pub fn audit_metadata(&self) -> Option<&Value> {
        self.audit_metadata.as_ref()
    }

    /// 返回由脱敏安全结果派生的内部上下文 accounting，不进入公开 payload。
    pub fn context_token_count(&self) -> Option<u32> {
        self.context_token_count
    }

    /// 从未截断且仍保留完整安全 `content` 的结果重建可信 accounting。
    pub fn reconstruct_context_token_count(&self) -> Option<u32> {
        if self.truncated || self.preview.is_some() {
            return None;
        }
        let content = self.content.as_ref()?;
        let payload = self.to_message_payload();
        if payload.get("content") != Some(content) {
            return None;
        }
        Some(safe_context_token_count(content))
    }

    /// 返回当前安全模型投影可证明的 accounting 下界。
    pub fn context_token_count_lower_bound(&self) -> u32 {
        let payload = self.to_message_payload();
        let accounted_value = payload
            .get("content")
            .or_else(|| payload.get("preview"))
            .unwrap_or(&payload);
        serde_json::to_string(accounted_value)
            .map_or(u32::MAX, |serialized| approximate_token_count(&serialized))
    }

    /// 从 approval checkpoint 恢复内部上下文 accounting。
    pub fn with_context_token_count(mut self, token_count: u32) -> Self {
        self.context_token_count = Some(token_count);
        self
    }

    /// 返回只供 completion/checkpoint 使用的 workspace observation。
    pub fn workspace_observation(&self) -> Option<&WorkspaceObservation> {
        self.workspace_observation.as_ref()
    }

    /// 绑定内部 workspace observation；该字段不会序列化到模型 payload。
    pub fn with_workspace_observation(mut self, observation: WorkspaceObservation) -> Self {
        self.workspace_observation = Some(observation);
        self
    }

    /// 将执行器输出投影为模型和 trace 可用结果。
    pub fn from_result(envelope: &ToolCallRequest, result: &ToolOutput) -> Self {
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
        let sanitized_content = sanitize_public_value(result.content.clone());
        let context_token_count = safe_context_token_count(&sanitized_content);
        let sanitized_content_chars = sanitized_content.to_string().chars().count();
        let public_content_summarized =
            source_truncated || sanitized_content_chars > DEFAULT_RESULT_PREVIEW_MAX_CHARS;
        let public_content = if public_content_summarized {
            summarize_truncated_value(sanitized_content)
        } else {
            sanitized_content
        };
        let result_content = public_content.to_string();
        let content_is_safe = redact_public_text(&result_content) == result_content;
        let (bounded_preview, preview_truncated) =
            bounded_text(&result_content, DEFAULT_RESULT_PREVIEW_MAX_CHARS);
        let preview = redact_public_text(&bounded_preview);
        let truncated = public_content_summarized || preview_truncated;
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
        tool_result.context_token_count = Some(context_token_count);
        if content_is_safe && !source_truncated && !preview_truncated {
            tool_result.content = Some(public_content);
            tool_result.preview = None;
        }
        tool_result.result_id = result_id;
        tool_result.audit_metadata = result.metadata.get("audit").cloned();
        tool_result.workspace_observation = result
            .metadata
            .get(WORKSPACE_OBSERVATION_METADATA)
            .and_then(|value| serde_json::from_value(value.clone()).ok());
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
        if let Some(content) = self.content.as_ref() {
            let content = sanitize_public_value(content.clone());
            let serialized = content.to_string();
            let (bounded_preview, content_truncated) =
                bounded_text(&serialized, DEFAULT_RESULT_PREVIEW_MAX_CHARS);
            if !content_truncated && redact_public_text(&serialized) == serialized {
                payload["content"] = content;
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

fn sanitize_public_value(value: Value) -> Value {
    match value {
        Value::Array(values) => {
            Value::Array(values.into_iter().map(sanitize_public_value).collect())
        }
        Value::Object(entries) => Value::Object(
            entries
                .into_iter()
                .filter_map(|(key, value)| {
                    if is_artifact_reference_key(&key) {
                        None
                    } else {
                        Some((key, sanitize_public_value(value)))
                    }
                })
                .collect(),
        ),
        Value::String(value) if contains_artifact_reference(&value) => {
            Value::String(ARTIFACT_REFERENCE_OMITTED.to_string())
        }
        other => other,
    }
}

fn summarize_truncated_value(value: Value) -> Value {
    match value {
        Value::Array(values) => {
            Value::Array(values.into_iter().map(summarize_truncated_value).collect())
        }
        Value::Object(entries) => Value::Object(
            entries
                .into_iter()
                .filter_map(|(key, value)| {
                    if is_truncated_raw_output_key(&key) {
                        None
                    } else {
                        Some((key, summarize_truncated_value(value)))
                    }
                })
                .collect(),
        ),
        Value::String(value) if value.chars().count() > MAX_TRUNCATED_SUMMARY_STRING_CHARS => {
            Value::String(TRUNCATED_OUTPUT_OMITTED.to_string())
        }
        other => other,
    }
}

fn is_truncated_raw_output_key(key: &str) -> bool {
    TRUNCATED_RAW_OUTPUT_KEYS.contains(&key.to_ascii_lowercase().as_str())
}

fn is_artifact_reference_key(key: &str) -> bool {
    matches!(
        key.to_ascii_lowercase().as_str(),
        "artifact_ref" | "artifact_refs" | "diff_ref" | "diff_refs"
    )
}

fn contains_artifact_reference(value: &str) -> bool {
    value.to_ascii_lowercase().contains("artifact://")
}

/// 只用递归脱敏后的值计算内部 accounting；原始结果不会从该函数返回或持久化。
fn safe_context_token_count(value: &Value) -> u32 {
    let accounting_value = redact_context_value(value.clone());
    serde_json::to_string(&accounting_value)
        .map_or(u32::MAX, |serialized| approximate_token_count(&serialized))
}

/// 为 accounting 递归移除 artifact key，并把敏感文本替换为固定安全摘要。
fn redact_context_value(value: Value) -> Value {
    match value {
        Value::Array(values) => {
            Value::Array(values.into_iter().map(redact_context_value).collect())
        }
        Value::Object(entries) => Value::Object(
            entries
                .into_iter()
                .filter_map(|(key, value)| {
                    if is_artifact_reference_key(&key) {
                        None
                    } else {
                        Some((key, redact_context_value(value)))
                    }
                })
                .collect(),
        ),
        Value::String(value) => Value::String(redact_public_text(&value)),
        other => other,
    }
}

/// 按当前 provider request budget 使用的保守字符规则估算 token 数。
pub fn approximate_token_count(content: &str) -> u32 {
    let mut ascii_chars = 0usize;
    let mut non_ascii_chars = 0usize;
    for character in content.chars() {
        if character.is_ascii() {
            ascii_chars = ascii_chars.saturating_add(1);
        } else {
            non_ascii_chars = non_ascii_chars.saturating_add(1);
        }
    }
    let ascii_tokens = ascii_chars.saturating_add(APPROXIMATE_ASCII_CHARS_PER_TOKEN - 1)
        / APPROXIMATE_ASCII_CHARS_PER_TOKEN;
    let estimated = ascii_tokens.saturating_add(non_ascii_chars);
    u32::try_from(estimated.max(1)).unwrap_or(u32::MAX)
}

/// 工作区 tool 返回的工作区边界、受保护路径、沙箱和变更错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceToolError {
    OutsideWorkspace(String),
    ProtectedPath(String),
    SandboxUnavailable,
    Cancelled,
    BinaryPattern,
    ConcurrentMutation(String),
    HardLinkRejected(String),
    PathIdentityUnsupported(String),
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
            Self::Cancelled => write!(formatter, "workspace tool execution cancelled"),
            Self::BinaryPattern => write!(formatter, "grep pattern must be valid utf-8 text"),
            Self::ConcurrentMutation(path) => {
                write!(
                    formatter,
                    "workspace target changed during mutation: {path}"
                )
            }
            Self::HardLinkRejected(path) => {
                write!(formatter, "workspace hard-linked file is rejected: {path}")
            }
            Self::PathIdentityUnsupported(path) => {
                write!(formatter, "workspace path identity is unsupported: {path}")
            }
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

/// 工作区 tool 接受的有界文件读取请求。
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

/// 工作区 tool 接受的有界目录列表请求。
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

/// 工作区 tool 接受的有界文本搜索请求。
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

/// 工作区 tool 接受的单文件替换请求。
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

/// 绑定到一个工作区执行生命周期的单调 revision；只在内部 verification/checkpoint 中使用。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct WorkspaceRevision(u64);

impl WorkspaceRevision {
    /// 返回新绑定工作区的初始 revision。
    pub fn initial() -> Self {
        Self(0)
    }

    /// 返回下一个 revision；溢出时返回 `None`，调用方必须 fail closed。
    pub fn next(self) -> Option<Self> {
        self.0.checked_add(1).map(Self)
    }
}

/// 一次 tool 结果实际观察到的工作区 revision 与变化事实。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceObservation {
    revision: Option<WorkspaceRevision>,
    mutation: WorkspaceMutation,
}

impl WorkspaceObservation {
    /// 构造一次未改变工作区的观察。
    pub fn unchanged(revision: WorkspaceRevision) -> Self {
        Self {
            revision: Some(revision),
            mutation: WorkspaceMutation::Unchanged,
        }
    }

    /// 构造一次已改变工作区的观察。
    pub fn changed(revision: WorkspaceRevision) -> Self {
        Self {
            revision: Some(revision),
            mutation: WorkspaceMutation::Changed,
        }
    }

    /// 构造无法可靠判断工作区变化的观察。
    pub fn unknown() -> Self {
        Self {
            revision: None,
            mutation: WorkspaceMutation::Unknown,
        }
    }

    /// 返回该观察绑定的 revision；未知观察没有可用 revision。
    pub fn revision(&self) -> Option<WorkspaceRevision> {
        self.revision
    }

    /// 返回 backend/WorkspaceTools 报告的变化事实。
    pub fn mutation(&self) -> WorkspaceMutation {
        self.mutation
    }
}

/// 绑定到根目录的工作区文件 tool，以及为命令配置的严格沙箱 backend。
#[derive(Clone)]
pub struct WorkspaceTools {
    workspace_root: PathBuf,
    workspace_capability: Arc<CapabilityDir>,
    #[cfg(windows)]
    workspace_namespace_guards: Arc<Vec<std::fs::File>>,
    sandbox_backend: Option<Arc<dyn SandboxBackend + Send + Sync>>,
    command_environment: CommandEnvironmentPolicy,
    workspace_revision: Arc<AtomicU64>,
}

impl fmt::Debug for WorkspaceTools {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut debug = formatter.debug_struct("WorkspaceTools");
        debug
            .field("workspace_root", &self.workspace_root)
            .field("workspace_capability_bound", &true);
        #[cfg(windows)]
        debug.field(
            "workspace_namespace_guards",
            &self.workspace_namespace_guards.len(),
        );
        debug
            .field(
                "sandbox_backend",
                &self.sandbox_backend.as_ref().map(|backend| backend.name()),
            )
            .finish()
    }
}

impl WorkspaceTools {
    /// 将工作区根目录绑定到文件和 command tool。
    ///
    /// 构造成功时会从稳定平台 anchor 逐组件打开 root directory capability；后续
    /// 文件操作只使用该 capability 的相对路径；绑定失败会作为 typed error 返回，
    /// 不会构造一个延迟失败或退化为 ambient path 的工具实例。
    pub fn new(workspace_root: impl Into<PathBuf>) -> Result<Self, WorkspaceToolError> {
        let workspace_root = absolute_workspace_root(workspace_root.into())?;
        let capability = bind_workspace_root(&workspace_root)?;
        let metadata = capability.dir_metadata().map_err(io_error)?;
        if !metadata.is_dir() || metadata_is_symlink_or_reparse(&metadata) {
            return Err(WorkspaceToolError::ReadFailed(
                "workspace root is not a regular directory".to_string(),
            ));
        }
        #[cfg(windows)]
        let (workspace_root_display, workspace_namespace_guards) =
            bind_workspace_namespace(&workspace_root, &capability)?;
        #[cfg(not(windows))]
        let workspace_root_display = {
            let canonical = std::fs::canonicalize(&workspace_root).map_err(io_error)?;
            let namespace = bind_workspace_root(&canonical)?;
            let capability_identity = directory_object_identity_key(&capability)
                .map_err(|error| map_capability_error(error, "."))?;
            let namespace_identity = directory_object_identity_key(&namespace)
                .map_err(|error| map_capability_error(error, "."))?;
            if capability_identity != namespace_identity {
                return Err(WorkspaceToolError::OutsideWorkspace(
                    "workspace root changed while it was being bound".to_string(),
                ));
            }
            canonical
        };
        Ok(Self {
            workspace_root: workspace_root_display,
            workspace_capability: Arc::new(capability),
            #[cfg(windows)]
            workspace_namespace_guards: Arc::new(workspace_namespace_guards),
            sandbox_backend: None,
            command_environment: CommandEnvironmentPolicy::default(),
            workspace_revision: Arc::new(AtomicU64::new(WorkspaceRevision::initial().0)),
        })
    }

    /// 返回与当前目录 capability 身份一致的工作区显示路径。
    pub fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    /// 绑定一个严格 sandbox backend。
    pub fn with_sandbox_backend(
        self,
        sandbox_backend: impl SandboxBackend + Send + Sync + 'static,
    ) -> Self {
        self.with_shared_sandbox_backend(Arc::new(sandbox_backend))
    }

    /// 绑定可共享的 sandbox backend。
    pub fn with_shared_sandbox_backend(
        mut self,
        sandbox_backend: Arc<dyn SandboxBackend + Send + Sync>,
    ) -> Self {
        self.sandbox_backend = Some(sandbox_backend);
        self
    }

    /// 设置命令子进程环境策略。
    pub fn with_command_environment(mut self, environment: CommandEnvironmentPolicy) -> Self {
        self.command_environment = environment;
        self
    }

    /// 将新的 `WorkspaceTools` 实例绑定到 approval checkpoint 的内部 revision。
    ///
    /// 恢复只允许填充尚未执行命令的初始计数，或确认已经相同的计数；不会覆盖一个
    /// 已经观察到不同 revision 的执行器状态。该绑定不进入模型 payload。
    pub fn bind_checkpoint_workspace_revision(
        &self,
        revision: Option<WorkspaceRevision>,
    ) -> Result<(), WorkspaceToolError> {
        let expected = revision.unwrap_or_else(WorkspaceRevision::initial).0;
        let current = self.workspace_revision.load(Ordering::SeqCst);
        if current == expected {
            return Ok(());
        }
        if current != WorkspaceRevision::initial().0 {
            return Err(WorkspaceToolError::InvalidInput(
                "workspace revision differs from approval checkpoint".to_string(),
            ));
        }
        self.workspace_revision
            .compare_exchange(current, expected, Ordering::SeqCst, Ordering::SeqCst)
            .map(|_| ())
            .map_err(|_| {
                WorkspaceToolError::InvalidInput(
                    "workspace revision changed while restoring approval checkpoint".to_string(),
                )
            })
    }

    fn current_workspace_revision(&self) -> WorkspaceRevision {
        WorkspaceRevision(self.workspace_revision.load(Ordering::SeqCst))
    }

    fn advance_workspace_revision(&self) -> Result<WorkspaceRevision, WorkspaceToolError> {
        let previous = self
            .workspace_revision
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |revision| {
                revision.checked_add(1)
            })
            .map_err(|_| {
                WorkspaceToolError::InvalidInput("workspace revision exhausted".to_string())
            })?;
        previous
            .checked_add(1)
            .map(WorkspaceRevision)
            .ok_or_else(|| {
                WorkspaceToolError::InvalidInput("workspace revision exhausted".to_string())
            })
    }

    fn attach_workspace_observation(
        output: &mut ToolOutput,
        observation: &WorkspaceObservation,
    ) -> Result<(), WorkspaceToolError> {
        output.metadata[WORKSPACE_OBSERVATION_METADATA] =
            serde_json::to_value(observation).map_err(serialization_error)?;
        Ok(())
    }

    /// 规范化执行输入，并从同一次工作区解析结果投影类型化授权资源。
    fn bind_tool_call(
        &self,
        entry: &ToolEntry,
        executor: WorkspaceToolExecutor,
        input: Value,
        filesystem: SandboxFilesystemMode,
        network: SandboxNetworkMode,
    ) -> Result<BoundToolCall, WorkspaceToolError> {
        let (operation, arguments, resources) = match executor {
            WorkspaceToolExecutor::Read => {
                ensure_authorization(entry, ToolAuthorization::WorkspaceRead)?;
                let mut input: ReadToolInput = preflight_input(&input)?;
                input.validate()?;
                let path = self.bound_workspace_path(&input.path, false)?;
                input.path = path.as_str().to_string();
                (
                    PermissionOperation::Read,
                    serde_json::to_value(input).map_err(serialization_error)?,
                    vec![PermissionResource::WorkspacePath(path)],
                )
            }
            WorkspaceToolExecutor::List => {
                ensure_authorization(entry, ToolAuthorization::WorkspaceRead)?;
                let mut input: ListToolInput = preflight_input(&input)?;
                input.validate()?;
                let path =
                    self.bound_workspace_path(input.path.as_deref().unwrap_or("."), false)?;
                input.path = Some(path.as_str().to_string());
                (
                    PermissionOperation::Read,
                    serde_json::to_value(input).map_err(serialization_error)?,
                    vec![PermissionResource::WorkspacePath(path)],
                )
            }
            WorkspaceToolExecutor::Grep => {
                ensure_authorization(entry, ToolAuthorization::WorkspaceRead)?;
                let mut input: GrepToolInput = preflight_input(&input)?;
                input.validate()?;
                let path = self.bound_workspace_path(
                    input.path.as_deref().unwrap_or("."),
                    input.path.is_none(),
                )?;
                input.path = Some(path.as_str().to_string());
                (
                    PermissionOperation::Read,
                    serde_json::to_value(input).map_err(serialization_error)?,
                    vec![PermissionResource::WorkspacePath(path)],
                )
            }
            WorkspaceToolExecutor::Edit => {
                ensure_authorization(entry, ToolAuthorization::WorkspaceWrite)?;
                let mut input: EditToolInput = preflight_input(&input)?;
                input.validate()?;
                let path = self.bound_workspace_path(&input.path, false)?;
                input.path = path.as_str().to_string();
                (
                    PermissionOperation::Write,
                    serde_json::to_value(input).map_err(serialization_error)?,
                    vec![PermissionResource::WorkspacePath(path)],
                )
            }
            WorkspaceToolExecutor::Patch => {
                ensure_authorization(entry, ToolAuthorization::WorkspaceWrite)?;
                let mut input: WorkspacePatch = preflight_input(&input)?;
                input.validate()?;
                let mut resources = Vec::with_capacity(input.changes.len());
                let mut unique = BTreeSet::new();
                for change in &mut input.changes {
                    let path = self.bound_workspace_path(&change.path, false)?;
                    if !unique.insert(path.clone()) {
                        return Err(WorkspaceToolError::InvalidInput(
                            DUPLICATE_PATCH_TARGET.to_string(),
                        ));
                    }
                    change.path = path.as_str().to_string();
                    resources.push(PermissionResource::WorkspacePath(path));
                }
                (
                    PermissionOperation::Write,
                    serde_json::to_value(input).map_err(serialization_error)?,
                    resources,
                )
            }
            WorkspaceToolExecutor::Command => {
                ensure_authorization(entry, ToolAuthorization::Command)?;
                let mut input: CommandToolInput = preflight_input(&input)?;
                input.validate()?;
                let Some(backend) = &self.sandbox_backend else {
                    return Err(WorkspaceToolError::SandboxUnavailable);
                };
                if !backend.capabilities().supports_command_execution() {
                    return Err(WorkspaceToolError::SandboxUnavailable);
                }
                let cwd = self.bound_workspace_path(input.effective_cwd(), false)?;
                input.cwd = Some(cwd.as_str().to_string());
                let digest = CommandScopeDigest::new(command_script_scope_digest_with_policy(
                    &input.command,
                    cwd.as_str(),
                    input.effective_timeout_seconds(),
                    filesystem,
                    network,
                ))
                .map_err(WorkspaceToolError::InvalidInput)?;
                (
                    PermissionOperation::Execute,
                    serde_json::to_value(input).map_err(serialization_error)?,
                    vec![PermissionResource::CommandScope(digest)],
                )
            }
        };
        let sensitive_resources = resources
            .iter()
            .filter(|resource| match resource {
                PermissionResource::WorkspacePath(path) => is_protected_path(path.as_str()),
                PermissionResource::CommandScope(_) | PermissionResource::Tool(_) => false,
            })
            .cloned()
            .collect();
        Ok(BoundToolCall {
            tool_id: entry.id.clone(),
            execution_mode: entry.spec.execution_mode,
            executor: entry.executor,
            operation,
            arguments,
            resources,
            sensitive_resources,
        })
    }

    fn bound_workspace_path(
        &self,
        path: &str,
        allow_protected: bool,
    ) -> Result<WorkspaceRelativePath, WorkspaceToolError> {
        let resolved = self.resolve_workspace_path(path, allow_protected)?;
        WorkspaceRelativePath::from_canonical(resolved.display)
            .map_err(WorkspaceToolError::InvalidInput)
    }

    /// 在执行或变更前校验输入，并解析每个被引用的路径。
    pub fn preflight(&self, tool_name: &str, input: &Value) -> Result<(), WorkspaceToolError> {
        match tool_name {
            READ_TOOL => {
                let input: ReadToolInput = preflight_input(input)?;
                input.validate()?;
                let target = self.resolve_workspace_path(&input.path, false)?;
                self.validate_existing_path(&target, false)?;
            }
            LIST_TOOL => {
                let input: ListToolInput = preflight_input(input)?;
                input.validate()?;
                let target = self.resolve_optional_workspace_path(input.path.as_deref(), false)?;
                self.validate_existing_path(&target, true)?;
            }
            GREP_TOOL => {
                let input: GrepToolInput = preflight_input(input)?;
                input.validate()?;
                let target = self.resolve_optional_workspace_path(input.path.as_deref(), true)?;
                self.validate_existing_path(&target, false)?;
            }
            EDIT_TOOL => {
                let input: EditToolInput = preflight_input(input)?;
                input.validate()?;
                let target = self.resolve_workspace_path(&input.path, false)?;
                self.validate_existing_path(&target, false)?;
            }
            PATCH_TOOL => {
                let patch: WorkspacePatch = preflight_input(input)?;
                patch.validate()?;
                let mut targets = BTreeSet::new();
                for change in patch.changes {
                    let target = self.resolve_workspace_path(&change.path, false)?;
                    self.validate_existing_path(&target, false)?;
                    if !targets.insert(self.duplicate_target_key(&target)?) {
                        return Err(WorkspaceToolError::InvalidInput(
                            DUPLICATE_PATCH_TARGET.to_string(),
                        ));
                    }
                }
            }
            COMMAND_TOOL => {
                let input: CommandToolInput = preflight_input(input)?;
                input.validate()?;
                let Some(backend) = &self.sandbox_backend else {
                    return Err(WorkspaceToolError::SandboxUnavailable);
                };
                if !backend.capabilities().supports_command_execution() {
                    return Err(WorkspaceToolError::SandboxUnavailable);
                }
                let target = self.resolve_optional_workspace_path(input.cwd.as_deref(), false)?;
                self.validate_existing_path(&target, true)?;
            }
            _ => {
                return Err(WorkspaceToolError::InvalidInput(
                    "tool backend is unavailable".to_string(),
                ));
            }
        }
        Ok(())
    }

    /// 在工作区内读取有界文件内容。
    pub fn read(&self, input: ReadToolInput) -> Result<ToolOutput, WorkspaceToolError> {
        self.read_cancellable(input, &CancellationToken::new())
    }

    /// 在工作区内读取有界文件内容，并在文件系统边界传播取消。
    pub fn read_cancellable(
        &self,
        input: ReadToolInput,
        cancellation: &CancellationToken,
    ) -> Result<ToolOutput, WorkspaceToolError> {
        self.read_with_cancellation_check(input, &|| cancellation.is_cancelled())
    }

    fn read_with_cancellation_check(
        &self,
        input: ReadToolInput,
        cancellation: &dyn Fn() -> bool,
    ) -> Result<ToolOutput, WorkspaceToolError> {
        check_cancelled(cancellation)?;
        input.validate()?;
        check_cancelled(cancellation)?;
        let max_chars = input.max_chars.unwrap_or(DEFAULT_READ_MAX_CHARS);
        let line_start = input.line_start.unwrap_or(1);
        let line_end = input.line_end.unwrap_or(usize::MAX);
        let target = self.resolve_workspace_path(&input.path, false)?;
        check_cancelled(cancellation)?;
        let relative = target.display.clone();
        check_cancelled(cancellation)?;
        let file = self.open_file_at(&target)?;
        check_cancelled(cancellation)?;
        let mut reader = CancellableLineReader::new(file);
        let mut line = Vec::new();
        let mut preview = String::new();
        let mut preview_truncated = false;
        let mut actual_line_start = None;
        let mut actual_line_end = None;
        let mut total_lines = 0usize;
        let mut last_line_partial = false;

        loop {
            check_cancelled(cancellation)?;
            line.clear();
            let bytes_read = reader.read_until(b'\n', &mut line, cancellation)?;
            check_cancelled(cancellation)?;
            if bytes_read == 0 {
                break;
            }
            total_lines = total_lines.saturating_add(1);
            if is_binary(&line) {
                check_cancelled(cancellation)?;
                return Ok(ToolOutput::success(json!({
                    "path": relative,
                    "binary": true,
                    "preview": BINARY_CONTENT_PREVIEW,
                    "truncated": true,
                    "line_start": Value::Null,
                    "line_end": Value::Null,
                    "total_lines": total_lines,
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

        check_cancelled(cancellation)?;
        let next_line_start = actual_line_end.and_then(|line_end| {
            if last_line_partial {
                None
            } else if line_end < total_lines {
                line_end.checked_add(1)
            } else {
                None
            }
        });
        let mut output = json!({
            "path": relative,
            "binary": false,
            "preview": preview,
            "truncated": preview_truncated,
            "line_start": actual_line_start,
            "line_end": actual_line_end,
            "total_lines": total_lines,
            "partial_line": last_line_partial,
        });
        if let Some(next_line_start) = next_line_start {
            output["next_line_start"] = json!(next_line_start);
        }
        check_cancelled(cancellation)?;
        Ok(ToolOutput::success(output))
    }

    /// 列出工作区内的有界目录内容。
    pub fn list(&self, input: ListToolInput) -> Result<ToolOutput, WorkspaceToolError> {
        self.list_cancellable(input, &CancellationToken::new())
    }

    /// 列出工作区内的有界目录内容，并在目录递归边界传播取消。
    pub fn list_cancellable(
        &self,
        input: ListToolInput,
        cancellation: &CancellationToken,
    ) -> Result<ToolOutput, WorkspaceToolError> {
        self.list_with_cancellation_check(input, &|| cancellation.is_cancelled())
    }

    fn list_with_cancellation_check(
        &self,
        input: ListToolInput,
        cancellation: &dyn Fn() -> bool,
    ) -> Result<ToolOutput, WorkspaceToolError> {
        check_cancelled(cancellation)?;
        input.validate()?;
        check_cancelled(cancellation)?;
        let target = self.resolve_optional_workspace_path(input.path.as_deref(), false)?;
        check_cancelled(cancellation)?;
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
        self.with_directory_at(&target, |directory| {
            self.collect_list_entries(directory, &target.display, 0, &mut state, cancellation)
        })?;
        check_cancelled(cancellation)?;
        state
            .entries
            .sort_by(|left, right| left.relative.cmp(&right.relative));
        let truncated_by_count = state.entries.len() > max_entries;
        state.entries.truncate(max_entries);
        check_cancelled(cancellation)?;
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

    /// 在工作区内执行有界文本搜索。
    pub fn grep(&self, input: GrepToolInput) -> Result<ToolOutput, WorkspaceToolError> {
        self.grep_cancellable(input, &CancellationToken::new())
    }

    /// 在工作区内执行有界文本搜索，并在递归和文件读取边界传播取消。
    pub fn grep_cancellable(
        &self,
        input: GrepToolInput,
        cancellation: &CancellationToken,
    ) -> Result<ToolOutput, WorkspaceToolError> {
        self.grep_with_cancellation_check(input, &|| cancellation.is_cancelled())
    }

    fn grep_with_cancellation_check(
        &self,
        input: GrepToolInput,
        cancellation: &dyn Fn() -> bool,
    ) -> Result<ToolOutput, WorkspaceToolError> {
        check_cancelled(cancellation)?;
        input.validate()?;
        check_cancelled(cancellation)?;
        let root =
            self.resolve_optional_workspace_path(input.path.as_deref(), input.path.is_none())?;
        check_cancelled(cancellation)?;
        let max_matches = input.max_matches.unwrap_or(DEFAULT_GREP_MAX_MATCHES);
        let mut matches = Vec::new();
        let collection_limit = max_matches.saturating_add(1);
        let metadata = self.metadata_at(&root)?;
        let truncated = if metadata.is_dir() {
            self.with_directory_at(&root, |directory| {
                self.grep_directory(
                    directory,
                    &root.display,
                    &input.pattern,
                    input.case_sensitive,
                    collection_limit,
                    &mut matches,
                    cancellation,
                )
            })?
        } else {
            let file = self.open_file_at(&root)?;
            self.grep_file(
                file,
                &root.display,
                &input.pattern,
                input.case_sensitive,
                collection_limit,
                &mut matches,
                cancellation,
            )?
        };
        check_cancelled(cancellation)?;
        matches.truncate(max_matches);
        check_cancelled(cancellation)?;
        Ok(ToolOutput::success(json!({
            "matches": matches,
            "truncated": truncated,
        })))
    }

    /// 以单文件 patch 语义执行受保护的替换。
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

    /// 先整批预检再原子写入多个文件变更。
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
            let relative = target.display.clone();
            if !targets.insert(self.duplicate_target_key(&target)?) {
                return Err(WorkspaceToolError::InvalidInput(format!(
                    "{DUPLICATE_PATCH_TARGET}: {relative}"
                )));
            }
            let (original, original_identity) = self.existing_text_or_empty(&target)?;
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
            prepared.push(PreparedMutation {
                path: target,
                relative,
                original,
                updated,
                original_identity,
            });
        }
        let mut created_directories = Vec::new();
        for mutation in &prepared {
            if let Err(error) =
                self.ensure_parent_directories(&mutation.path, &mut created_directories)
            {
                return match self.remove_created_directories(&mut created_directories) {
                    Ok(()) => Err(error),
                    Err(cleanup_error) => Err(WorkspaceToolError::RollbackFailed(format!(
                        "directory preparation error: {error}; cleanup error: {cleanup_error}"
                    ))),
                };
            }
        }
        let mut published = Vec::new();
        for mutation in &prepared {
            match self.atomic_write(
                &mutation.path,
                &mutation.updated,
                mutation.original_identity.as_deref(),
            ) {
                Ok(published_identity) => published.push(PublishedMutation {
                    prepared: mutation.clone(),
                    published_identity,
                }),
                Err(write_failure) => {
                    let AtomicWriteFailure {
                        error: write_error,
                        published_identity,
                    } = write_failure;
                    if let Some(published_identity) = published_identity {
                        published.push(PublishedMutation {
                            prepared: mutation.clone(),
                            published_identity,
                        });
                    }
                    let file_rollback = self.rollback_published(&published);
                    let directory_rollback =
                        self.remove_created_directories(&mut created_directories);
                    if let Err(rollback_error) = file_rollback.and(directory_rollback) {
                        return Err(WorkspaceToolError::RollbackFailed(format!(
                            "write error: {write_error}; rollback error: {rollback_error}"
                        )));
                    }
                    return Err(write_error);
                }
            }
        }
        let changed_files = prepared
            .iter()
            .map(|mutation| mutation.relative.clone())
            .collect::<Vec<_>>();
        let revision = self.advance_workspace_revision()?;
        let mut output = ToolOutput::success(json!({
            "changed_files": changed_files,
            "rolled_back": false,
        }));
        Self::attach_workspace_observation(&mut output, &WorkspaceObservation::changed(revision))?;
        Ok(output)
    }

    /// 在严格 sandbox backend 中执行命令字符串。
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
        let command_cwd = self.resolve_optional_workspace_path(input.cwd.as_deref(), false)?;
        let expected_scope = CommandScopeDigest::new(command_script_scope_digest_with_policy(
            &input.command,
            &command_cwd.display,
            input.effective_timeout_seconds(),
            SandboxFilesystemMode::ReadOnly,
            SandboxNetworkMode::Denied,
        ))
        .map_err(WorkspaceToolError::InvalidInput)?;
        self.command_cancellable_with_policy(
            input,
            SandboxFilesystemMode::ReadOnly,
            SandboxNetworkMode::Denied,
            &expected_scope,
            cancellation,
        )
    }

    /// 按 Agent/Policy 已绑定的范围执行模型 command string。
    pub fn command_cancellable_with_policy(
        &self,
        input: CommandToolInput,
        filesystem: SandboxFilesystemMode,
        network: SandboxNetworkMode,
        expected_scope: &CommandScopeDigest,
        cancellation: &CancellationToken,
    ) -> Result<ToolOutput, WorkspaceToolError> {
        input.validate()?;
        // A failed root binding must not degrade into an ambient command cwd.
        self.workspace_capability()?;
        let Some(backend) = &self.sandbox_backend else {
            return Err(WorkspaceToolError::SandboxUnavailable);
        };
        let capabilities = backend.capabilities();
        if !capabilities.supports_command_execution() {
            return Err(WorkspaceToolError::SandboxUnavailable);
        }
        if matches!(&filesystem, SandboxFilesystemMode::WorkspaceWrite)
            && !capabilities.change_detection
        {
            return Err(WorkspaceToolError::SandboxUnavailable);
        }
        let command_cwd = self.resolve_optional_workspace_path(input.cwd.as_deref(), false)?;
        self.validate_existing_path(&command_cwd, true)?;
        let actual_scope = command_script_scope_digest_with_policy(
            &input.command,
            &command_cwd.display,
            input.effective_timeout_seconds(),
            filesystem.clone(),
            network.clone(),
        );
        if actual_scope != expected_scope.as_str() {
            return Err(WorkspaceToolError::InvalidInput(
                "command authorization scope does not match execution input".to_string(),
            ));
        }
        let requested_filesystem = filesystem.clone();
        let bound_command_cwd = self.bind_command_cwd(&command_cwd)?;
        let mut request = CommandScriptRequest::agent_requested_with_policy(
            next_command_id(),
            input.command,
            bound_command_cwd.path.to_string_lossy().into_owned(),
            self.workspace_root.to_string_lossy().into_owned(),
            filesystem,
            network,
        );
        request.environment = self.command_environment.clone();
        if let Some(timeout_seconds) = input.timeout_seconds {
            request.timeout_seconds = timeout_seconds;
        }
        let result = backend.execute_script_cancellable(&request, cancellation);
        drop(bound_command_cwd);
        let mutation = result.workspace_mutation;
        let execution = result.sandbox.clone();
        let mut output = command_tool_output(result);
        let observation = match (&requested_filesystem, mutation) {
            (SandboxFilesystemMode::WorkspaceWrite, WorkspaceMutation::Unchanged) => {
                WorkspaceObservation::unchanged(self.current_workspace_revision())
            }
            (SandboxFilesystemMode::WorkspaceWrite, WorkspaceMutation::Changed) => {
                WorkspaceObservation::changed(self.advance_workspace_revision()?)
            }
            (SandboxFilesystemMode::WorkspaceWrite, WorkspaceMutation::Unknown) => {
                output.ok = false;
                output.failure_kind = Some(ToolFailureKind::Backend);
                output.error_code = Some("workspace_change_unknown".to_string());
                WorkspaceObservation::unknown()
            }
            (SandboxFilesystemMode::ReadOnly, WorkspaceMutation::Changed) => {
                output.ok = false;
                output.failure_kind = Some(ToolFailureKind::Backend);
                output.error_code = Some("workspace_changed_in_read_only_command".to_string());
                WorkspaceObservation::changed(self.advance_workspace_revision()?)
            }
            (
                SandboxFilesystemMode::ReadOnly,
                WorkspaceMutation::Unchanged | WorkspaceMutation::Unknown,
            ) => WorkspaceObservation::unchanged(self.current_workspace_revision()),
        };
        Self::attach_workspace_observation(&mut output, &observation)?;
        output.metadata["result_id"] = json!(expected_scope.as_str());
        output.metadata["audit"] = json!({
            "cwd": request.cwd,
            "timeout_seconds": request.timeout_seconds,
            "sandbox_mode": request.filesystem.mode,
            "network_access": request.network.mode,
            "sandbox_backend": execution.backend,
            "sandbox_enforcement": execution.enforcement,
            "local_process_fallback": execution.local_process_fallback,
            "command_scope_digest": expected_scope.as_str(),
            "command_provenance": "agent_requested",
        });
        Ok(output)
    }

    fn workspace_capability(&self) -> Result<&CapabilityDir, WorkspaceToolError> {
        Ok(self.workspace_capability.as_ref())
    }

    fn bind_command_cwd(
        &self,
        path: &CapabilityRelativePath,
    ) -> Result<BoundCommandCwd, WorkspaceToolError> {
        let (actual_relative, capability_guard) = if path.relative == Path::new(".") {
            (
                ".".to_string(),
                self.workspace_capability.try_clone().map_err(io_error)?,
            )
        } else {
            let parent = self
                .open_parent_directory(&path.relative, false)
                .map_err(|error| map_capability_error(error, &path.display))?;
            let directory = open_directory_component(parent.dir(), &parent.name, false)
                .map_err(|error| map_capability_error(error, &path.display))?;
            let actual_relative = self
                .actual_relative_for_directory(&directory, &path.display)
                .map_err(|error| map_capability_error(error, &path.display))?;
            (actual_relative, directory)
        };
        if is_protected_path(&actual_relative) {
            return Err(WorkspaceToolError::ProtectedPath(actual_relative));
        }
        let capability_identity = directory_object_identity_key(&capability_guard)
            .map_err(|error| map_capability_error(error, &path.display))?;
        let command_path = normalize_path(&self.workspace_root.join(&actual_relative));
        #[cfg(windows)]
        let namespace_guard = {
            let namespace_guard = open_workspace_namespace_guard(&command_path)?;
            let guard_identity = standard_file_object_identity_key(&namespace_guard)
                .map_err(|error| map_capability_error(error, &path.display))?;
            if capability_identity != guard_identity {
                return Err(WorkspaceToolError::OutsideWorkspace(
                    "command cwd changed while it was being bound".to_string(),
                ));
            }
            namespace_guard
        };
        #[cfg(not(windows))]
        let namespace_guard = {
            // The product backend is explicitly unavailable off Windows today. Still bind the
            // ambient cwd to the same object so a future platform adapter cannot silently bypass
            // the workspace capability boundary.
            let namespace_guard = bind_workspace_root(&command_path)?;
            let guard_identity = directory_object_identity_key(&namespace_guard)
                .map_err(|error| map_capability_error(error, &path.display))?;
            if capability_identity != guard_identity {
                return Err(WorkspaceToolError::OutsideWorkspace(
                    "command cwd changed while it was being bound".to_string(),
                ));
            }
            namespace_guard
        };
        Ok(BoundCommandCwd {
            path: command_path,
            _capability_guard: capability_guard,
            _namespace_guard: namespace_guard,
        })
    }

    fn open_file_at(
        &self,
        path: &CapabilityRelativePath,
    ) -> Result<CapabilityFile, WorkspaceToolError> {
        self.open_existing_file(path)
            .map_err(|error| map_capability_error(error, &path.display))?
            .ok_or_else(|| {
                WorkspaceToolError::ReadFailed("workspace path is unavailable".to_string())
            })
    }

    fn metadata_at(
        &self,
        path: &CapabilityRelativePath,
    ) -> Result<CapabilityMetadata, WorkspaceToolError> {
        if path.relative == Path::new(".") {
            return self
                .workspace_capability()?
                .dir_metadata()
                .map_err(io_error);
        }
        self.validate_existing_path(path, false)?;
        let parent = self
            .open_parent_directory(&path.relative, false)
            .map_err(|error| map_capability_error(error, &path.display))?;
        parent
            .dir()
            .symlink_metadata(&parent.name)
            .map_err(|error| map_capability_error(classify_io_error(error), &path.display))
    }

    fn with_directory_at<T>(
        &self,
        path: &CapabilityRelativePath,
        operation: impl FnOnce(&CapabilityDir) -> Result<T, WorkspaceToolError>,
    ) -> Result<T, WorkspaceToolError> {
        if path.relative == Path::new(".") {
            return operation(self.workspace_capability()?);
        }
        let parent = self
            .open_parent_directory(&path.relative, false)
            .map_err(|error| map_capability_error(error, &path.display))?;
        let directory = open_directory_component(parent.dir(), &parent.name, false)
            .map_err(|error| map_capability_error(error, &path.display))?;
        let actual_path = self
            .actual_relative_for_directory(&directory, &path.display)
            .map_err(|error| map_capability_error(error, &path.display))?;
        if is_protected_path(&actual_path) {
            return Err(WorkspaceToolError::ProtectedPath(actual_path));
        }
        operation(&directory)
    }

    fn open_parent_directory<'a>(
        &'a self,
        relative: &Path,
        create_missing: bool,
    ) -> Result<ParentDirectory<'a>, CapabilityAccessError> {
        let mut components = relative.components().collect::<Vec<_>>();
        let final_component = components.pop().ok_or(CapabilityAccessError::Unsafe)?;
        let final_name = normal_component(final_component)?.to_os_string();
        let root = self.workspace_capability.as_ref();
        let mut current = None;
        let mut requested_relative = String::new();
        let mut actual_relative = String::new();
        for component in components {
            let name = normal_component(component)?;
            let parent = current
                .as_ref()
                .map_or(root, |directory: &CapabilityDir| directory);
            let directory = open_directory_component(parent, name, create_missing)?;
            requested_relative = join_relative_path(&requested_relative, name);
            actual_relative =
                self.actual_relative_for_directory(&directory, &requested_relative)?;
            if is_protected_path(&actual_relative) {
                return Err(CapabilityAccessError::Protected(actual_relative));
            }
            current = Some(directory);
        }
        let directory = match current {
            Some(directory) => ParentDirectoryKind::Opened(directory),
            None => ParentDirectoryKind::Root(root),
        };
        Ok(ParentDirectory {
            directory,
            name: final_name,
            actual_relative,
        })
    }

    fn actual_relative_for_directory(
        &self,
        directory: &CapabilityDir,
        _fallback: &str,
    ) -> Result<String, CapabilityAccessError> {
        #[cfg(windows)]
        {
            let root = self.workspace_capability.as_ref();
            relative_path_from_handles(root, directory)
        }
        #[cfg(not(windows))]
        {
            Ok(_fallback.to_string())
        }
    }

    fn actual_relative_for_file(
        &self,
        file: &CapabilityFile,
        _fallback: &str,
    ) -> Result<String, CapabilityAccessError> {
        #[cfg(windows)]
        {
            let root = self.workspace_capability.as_ref();
            relative_path_from_file_handle(root, file)
        }
        #[cfg(not(windows))]
        {
            Ok(_fallback.to_string())
        }
    }

    fn validate_existing_path(
        &self,
        path: &CapabilityRelativePath,
        require_directory: bool,
    ) -> Result<(), WorkspaceToolError> {
        if path.relative == Path::new(".") {
            if require_directory {
                let metadata = self
                    .workspace_capability()?
                    .dir_metadata()
                    .map_err(io_error)?;
                if !metadata.is_dir() {
                    return Err(WorkspaceToolError::ReadFailed(
                        "workspace path is not a directory".to_string(),
                    ));
                }
            }
            return Ok(());
        }
        let Some(opened) = self
            .open_existing_path(path)
            .map_err(|error| map_capability_error(error, &path.display))?
        else {
            return Ok(());
        };
        if require_directory && !opened.is_directory {
            return Err(WorkspaceToolError::ReadFailed(
                "workspace path is not a directory".to_string(),
            ));
        }
        Ok(())
    }

    fn existing_text_or_empty(
        &self,
        path: &CapabilityRelativePath,
    ) -> Result<(String, Option<String>), WorkspaceToolError> {
        if path.relative == Path::new(".") {
            return Err(WorkspaceToolError::ReadFailed(
                "workspace path is not a regular file".to_string(),
            ));
        }
        let Some(mut file) = self
            .open_existing_file(path)
            .map_err(|error| map_capability_error(error, &path.display))?
        else {
            return Ok((String::new(), None));
        };
        let identity = file_object_identity_key(&file)
            .map_err(|error| map_capability_error(error, &path.display))?;
        let mut content = String::new();
        file.read_to_string(&mut content).map_err(io_error)?;
        Ok((content, Some(identity)))
    }

    fn duplicate_target_key(
        &self,
        path: &CapabilityRelativePath,
    ) -> Result<String, WorkspaceToolError> {
        match self
            .open_existing_file(path)
            .map_err(|error| map_capability_error(error, &path.display))?
        {
            Some(file) => file_object_identity_key(&file)
                .map_err(|error| map_capability_error(error, &path.display)),
            None => Ok(path.key.clone()),
        }
    }

    fn open_existing_file(
        &self,
        path: &CapabilityRelativePath,
    ) -> Result<Option<CapabilityFile>, CapabilityAccessError> {
        match self.open_existing_path(path)? {
            Some(opened) if opened.is_directory => Err(CapabilityAccessError::NotRegularFile),
            Some(opened) => opened
                .file
                .map(Some)
                .ok_or(CapabilityAccessError::Unsupported),
            None => Ok(None),
        }
    }

    fn open_existing_path(
        &self,
        path: &CapabilityRelativePath,
    ) -> Result<Option<OpenedWorkspacePath>, CapabilityAccessError> {
        if path.relative == Path::new(".") {
            return Ok(Some(OpenedWorkspacePath {
                is_directory: true,
                file: None,
            }));
        }
        let mut components = path.relative.components().collect::<Vec<_>>();
        let root = self.workspace_capability.as_ref();
        let mut current = None;
        let mut requested_relative = String::new();
        while let Some(component) = components.first().copied() {
            components.remove(0);
            let name = normal_component(component)?;
            let parent = current
                .as_ref()
                .map_or(root, |directory: &CapabilityDir| directory);
            let metadata = match parent.symlink_metadata(name) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(classify_io_error(error)),
            };
            if metadata_is_symlink_or_reparse(&metadata) {
                return Err(CapabilityAccessError::Unsafe);
            }
            let is_final = components.is_empty();
            requested_relative = join_relative_path(&requested_relative, name);
            if metadata.is_dir() {
                let directory = open_directory_component(parent, name, false)?;
                let actual_relative =
                    self.actual_relative_for_directory(&directory, &requested_relative)?;
                if is_protected_path(&actual_relative) {
                    return Err(CapabilityAccessError::Protected(actual_relative));
                }
                if is_final {
                    return Ok(Some(OpenedWorkspacePath {
                        is_directory: true,
                        file: None,
                    }));
                }
                current = Some(directory);
            } else if metadata.is_file() {
                let file = open_file_from_parent(parent, name)?;
                let actual_relative = self.actual_relative_for_file(&file, &requested_relative)?;
                if is_protected_path(&actual_relative) {
                    return Err(CapabilityAccessError::Protected(actual_relative));
                }
                if !is_final {
                    return Err(CapabilityAccessError::NotDirectory);
                }
                return Ok(Some(OpenedWorkspacePath {
                    is_directory: false,
                    file: Some(file),
                }));
            } else {
                return Err(CapabilityAccessError::NotRegularFile);
            }
        }
        Err(CapabilityAccessError::Unsupported)
    }

    fn ensure_parent_directories(
        &self,
        path: &CapabilityRelativePath,
        created: &mut Vec<CreatedDirectory>,
    ) -> Result<(), WorkspaceToolError> {
        let mut components = path.relative.components().collect::<Vec<_>>();
        components
            .pop()
            .ok_or_else(|| WorkspaceToolError::OutsideWorkspace(path.display.clone()))?;
        let root = self.workspace_capability.as_ref();
        let mut current = None;
        let mut requested_relative = String::new();
        for component in components {
            let name = normal_component(component)
                .map_err(|error| map_capability_error(error, &path.display))?;
            let parent = current
                .as_ref()
                .map_or(root, |directory: &CapabilityDir| directory);
            let was_created = match parent.symlink_metadata(name) {
                Ok(metadata) => {
                    if metadata_is_symlink_or_reparse(&metadata) || !metadata.is_dir() {
                        return Err(WorkspaceToolError::OutsideWorkspace(path.display.clone()));
                    }
                    false
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    match parent.create_dir(name) {
                        Ok(()) => true,
                        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => false,
                        Err(error) => return Err(io_error(error)),
                    }
                }
                Err(error) => return Err(io_error(error)),
            };
            let directory = open_directory_component(parent, name, false)
                .map_err(|error| map_capability_error(error, &path.display))?;
            requested_relative = join_relative_path(&requested_relative, name);
            if was_created {
                let relative = PathBuf::from(&requested_relative);
                let identity = directory_object_identity_key(&directory)
                    .map_err(|error| map_capability_error(error, &requested_relative))?;
                let guard = directory.try_clone().map_err(io_error)?;
                created.push(CreatedDirectory {
                    path: CapabilityRelativePath {
                        relative: relative.clone(),
                        display: requested_relative.clone(),
                        key: relative_path_key(&relative),
                    },
                    identity,
                    _guard: Some(guard),
                });
            }
            let actual_relative = self
                .actual_relative_for_directory(&directory, &requested_relative)
                .map_err(|error| map_capability_error(error, &path.display))?;
            if is_protected_path(&actual_relative) {
                return Err(WorkspaceToolError::ProtectedPath(actual_relative));
            }
            current = Some(directory);
        }
        Ok(())
    }

    fn atomic_write(
        &self,
        path: &CapabilityRelativePath,
        content: &str,
        expected_identity: Option<&str>,
    ) -> Result<String, AtomicWriteFailure> {
        self.atomic_write_with_hook(path, content, expected_identity, |_| {})
    }

    fn atomic_write_with_hook(
        &self,
        path: &CapabilityRelativePath,
        content: &str,
        expected_identity: Option<&str>,
        before_rename: impl FnOnce(&OsStr),
    ) -> Result<String, AtomicWriteFailure> {
        self.atomic_write_with_hooks(path, content, expected_identity, before_rename, || Ok(()))
    }

    fn atomic_write_with_hooks(
        &self,
        path: &CapabilityRelativePath,
        content: &str,
        expected_identity: Option<&str>,
        before_rename: impl FnOnce(&OsStr),
        after_rename: impl FnOnce() -> Result<(), WorkspaceToolError>,
    ) -> Result<String, AtomicWriteFailure> {
        let parent = self
            .open_parent_directory(&path.relative, false)
            .map_err(|error| map_capability_error(error, &path.display))?;
        let initial_target = self
            .atomic_target_state(parent.dir(), &parent.actual_relative, &parent.name)
            .map_err(|error| map_capability_error(error, &path.display))?;
        if initial_target.as_ref().map(|state| state.identity.as_str()) != expected_identity {
            return Err(WorkspaceToolError::ConcurrentMutation(path.display.clone()).into());
        }
        let original_permissions = initial_target.map(|state| state.permissions);
        let (temporary_name, mut temporary_file) = create_unique_temp_file(parent.dir())
            .map_err(|error| map_capability_error(error, &path.display))?;
        let temporary_identity = file_object_identity_key(&temporary_file)
            .map_err(|error| map_capability_error(error, &path.display))?;
        let write_result = temporary_file.write_all(content.as_bytes()).and_then(|()| {
            if let Some(permissions) = original_permissions {
                temporary_file.set_permissions(permissions)?;
            }
            temporary_file.sync_all()
        });
        if let Err(error) = write_result {
            drop(temporary_file);
            return Err(cleanup_owned_file(
                parent.dir(),
                &temporary_name,
                &temporary_identity,
                io_error(error),
            )
            .into());
        }
        before_rename(&temporary_name);
        let source_identity = open_file_from_parent(parent.dir(), &temporary_name)
            .and_then(|file| file_object_identity_key(&file))
            .map_err(|error| map_capability_error(error, &path.display))?;
        if source_identity != temporary_identity {
            drop(temporary_file);
            return Err(WorkspaceToolError::ConcurrentMutation(path.display.clone()).into());
        }
        let current_identity = self
            .atomic_target_state(parent.dir(), &parent.actual_relative, &parent.name)
            .map_err(|error| map_capability_error(error, &path.display))?;
        if current_identity
            .as_ref()
            .map(|state| state.identity.as_str())
            != expected_identity
        {
            drop(temporary_file);
            return Err(cleanup_owned_file(
                parent.dir(),
                &temporary_name,
                &temporary_identity,
                WorkspaceToolError::ConcurrentMutation(path.display.clone()),
            )
            .into());
        }
        if let Err(error) = parent
            .dir()
            .rename(&temporary_name, parent.dir(), &parent.name)
        {
            drop(temporary_file);
            return Err(cleanup_owned_file(
                parent.dir(),
                &temporary_name,
                &temporary_identity,
                io_error(error),
            )
            .into());
        }
        drop(temporary_file);
        after_rename()
            .map_err(|error| AtomicWriteFailure::published(error, temporary_identity.clone()))?;
        let published_state = self
            .atomic_target_state(parent.dir(), &parent.actual_relative, &parent.name)
            .map_err(|error| {
                AtomicWriteFailure::published(
                    map_capability_error(error, &path.display),
                    temporary_identity.clone(),
                )
            })?
            .ok_or_else(|| {
                AtomicWriteFailure::published(
                    WorkspaceToolError::ConcurrentMutation(path.display.clone()),
                    temporary_identity.clone(),
                )
            })?;
        if published_state.identity != temporary_identity {
            return Err(AtomicWriteFailure::published(
                WorkspaceToolError::ConcurrentMutation(path.display.clone()),
                temporary_identity,
            ));
        }
        Ok(published_state.identity)
    }

    fn atomic_target_state(
        &self,
        parent: &CapabilityDir,
        parent_relative: &str,
        name: &OsStr,
    ) -> Result<Option<AtomicTargetState>, CapabilityAccessError> {
        match parent.symlink_metadata(name) {
            Ok(metadata) => {
                if metadata_is_symlink_or_reparse(&metadata) {
                    return Err(CapabilityAccessError::Unsafe);
                }
                if !metadata.is_file() {
                    return Err(CapabilityAccessError::NotRegularFile);
                }
                let file = open_file_from_parent(parent, name)?;
                let actual = self
                    .actual_relative_for_file(&file, &join_relative_path(parent_relative, name))?;
                if is_protected_path(&actual) {
                    return Err(CapabilityAccessError::Protected(actual));
                }
                let identity = file_object_identity_key(&file)?;
                let permissions = file
                    .metadata()
                    .map_err(CapabilityAccessError::Io)?
                    .permissions();
                Ok(Some(AtomicTargetState {
                    identity,
                    permissions,
                }))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(classify_io_error(error)),
        }
    }

    fn rollback_published(
        &self,
        published: &[PublishedMutation],
    ) -> Result<(), WorkspaceToolError> {
        let mut failures = Vec::new();
        for mutation in published.iter().rev() {
            let result = if mutation.prepared.original_identity.is_some() {
                self.atomic_write(
                    &mutation.prepared.path,
                    &mutation.prepared.original,
                    Some(&mutation.published_identity),
                )
                .map(|_| ())
                .map_err(|failure| failure.error)
            } else {
                self.remove_created_file(&mutation.prepared.path, &mutation.published_identity)
            };
            if let Err(error) = result {
                failures.push(format!("{}: {error}", mutation.prepared.relative));
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(WorkspaceToolError::RollbackFailed(failures.join("; ")))
        }
    }

    fn remove_created_file(
        &self,
        path: &CapabilityRelativePath,
        expected_identity: &str,
    ) -> Result<(), WorkspaceToolError> {
        let parent = match self.open_parent_directory(&path.relative, false) {
            Ok(parent) => parent,
            Err(CapabilityAccessError::Missing) => return Ok(()),
            Err(error) => return Err(map_capability_error(error, &path.display)),
        };
        let current_identity = self
            .atomic_target_state(parent.dir(), &parent.actual_relative, &parent.name)
            .map_err(|error| map_capability_error(error, &path.display))?;
        match current_identity {
            None => Ok(()),
            Some(state) if state.identity == expected_identity => parent
                .dir()
                .remove_file_or_symlink(&parent.name)
                .map_err(io_error),
            Some(_) => Err(WorkspaceToolError::ConcurrentMutation(path.display.clone())),
        }
    }

    fn remove_created_directories(
        &self,
        created: &mut [CreatedDirectory],
    ) -> Result<(), WorkspaceToolError> {
        let mut failures = Vec::new();
        for directory in created.iter_mut().rev() {
            let result = (|| {
                let parent = match self.open_parent_directory(&directory.path.relative, false) {
                    Ok(parent) => parent,
                    Err(CapabilityAccessError::Missing) => return Ok(()),
                    Err(error) => {
                        return Err(map_capability_error(error, &directory.path.display));
                    }
                };
                let opened = match open_directory_component(parent.dir(), &parent.name, false) {
                    Ok(opened) => opened,
                    Err(CapabilityAccessError::Missing) => return Ok(()),
                    Err(error) => {
                        return Err(map_capability_error(error, &directory.path.display));
                    }
                };
                let identity = directory_object_identity_key(&opened)
                    .map_err(|error| map_capability_error(error, &directory.path.display))?;
                if identity != directory.identity {
                    return Err(WorkspaceToolError::ConcurrentMutation(
                        directory.path.display.clone(),
                    ));
                }
                drop(opened);
                drop(directory._guard.take());
                let reopened = open_directory_component(parent.dir(), &parent.name, false)
                    .map_err(|error| map_capability_error(error, &directory.path.display))?;
                let reopened_identity = directory_object_identity_key(&reopened)
                    .map_err(|error| map_capability_error(error, &directory.path.display))?;
                if reopened_identity != directory.identity {
                    return Err(WorkspaceToolError::ConcurrentMutation(
                        directory.path.display.clone(),
                    ));
                }
                drop(reopened);
                parent.dir().remove_dir(&parent.name).map_err(io_error)
            })();
            if let Err(error) = result {
                failures.push(format!("{}: {error}", directory.path.display));
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(WorkspaceToolError::RollbackFailed(failures.join("; ")))
        }
    }

    fn collect_list_entries(
        &self,
        directory: &CapabilityDir,
        prefix: &str,
        depth: usize,
        state: &mut ListState,
        cancellation: &dyn Fn() -> bool,
    ) -> Result<(), WorkspaceToolError> {
        check_cancelled(cancellation)?;
        if state.entries.len() >= state.collection_limit {
            state.truncated = true;
            return Ok(());
        }
        let entries = self.sorted_directory_entries(directory, prefix, cancellation)?;
        for entry in entries {
            check_cancelled(cancellation)?;
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
                check_cancelled(cancellation)?;
                return Ok(());
            }
            if state.recursive && entry.is_dir {
                match open_directory_component(directory, &entry.name, false) {
                    Ok(child) if depth < state.max_depth => {
                        self.collect_list_entries(
                            &child,
                            &entry.relative,
                            depth + 1,
                            state,
                            cancellation,
                        )?;
                    }
                    Ok(child) => {
                        self.mark_depth_boundary(&child, &entry.relative, state, cancellation)?;
                    }
                    Err(CapabilityAccessError::Unsafe | CapabilityAccessError::Missing) => {}
                    Err(error) => {
                        return Err(map_capability_error(error, &entry.relative));
                    }
                }
            }
        }
        check_cancelled(cancellation)?;
        Ok(())
    }

    fn mark_depth_boundary(
        &self,
        directory: &CapabilityDir,
        prefix: &str,
        state: &mut ListState,
        cancellation: &dyn Fn() -> bool,
    ) -> Result<(), WorkspaceToolError> {
        check_cancelled(cancellation)?;
        let entries = self.sorted_directory_entries(directory, prefix, cancellation)?;
        for entry in entries {
            check_cancelled(cancellation)?;
            if is_protected_path(&entry.relative) {
                state.redacted_entries = state.redacted_entries.saturating_add(1);
            } else if !entry.is_symlink_or_reparse {
                state.truncated = true;
            }
        }
        check_cancelled(cancellation)?;
        Ok(())
    }

    fn sorted_directory_entries(
        &self,
        directory: &CapabilityDir,
        prefix: &str,
        cancellation: &dyn Fn() -> bool,
    ) -> Result<Vec<DirectoryEntry>, WorkspaceToolError> {
        check_cancelled(cancellation)?;
        let mut entries = Vec::new();
        let mut directory_entries = directory.entries().map_err(io_error)?;
        check_cancelled(cancellation)?;
        loop {
            check_cancelled(cancellation)?;
            let Some(entry) = directory_entries.next() else {
                break;
            };
            let entry = entry.map_err(io_error)?;
            let name = entry.file_name();
            let file_type = entry.file_type().map_err(io_error)?;
            #[cfg(windows)]
            let is_symlink_or_reparse = file_type.is_symlink()
                || metadata_is_symlink_or_reparse(&entry.full_metadata().map_err(io_error)?);
            #[cfg(not(windows))]
            let is_symlink_or_reparse = file_type.is_symlink();
            let relative = join_relative_path(prefix, &name);
            entries.push(DirectoryEntry {
                name,
                relative,
                is_dir: file_type.is_dir() && !is_symlink_or_reparse,
                is_symlink_or_reparse,
            });
        }
        check_cancelled(cancellation)?;
        entries.sort_by(|left, right| left.relative.cmp(&right.relative));
        check_cancelled(cancellation)?;
        Ok(entries)
    }

    #[allow(clippy::too_many_arguments)]
    fn grep_directory(
        &self,
        directory: &CapabilityDir,
        prefix: &str,
        pattern: &str,
        case_sensitive: bool,
        collection_limit: usize,
        matches: &mut Vec<Value>,
        cancellation: &dyn Fn() -> bool,
    ) -> Result<bool, WorkspaceToolError> {
        check_cancelled(cancellation)?;
        if matches.len() >= collection_limit {
            return Ok(true);
        }
        let entries = self.sorted_directory_entries(directory, prefix, cancellation)?;
        for entry in entries {
            check_cancelled(cancellation)?;
            if is_protected_path(&entry.relative) || entry.is_symlink_or_reparse {
                continue;
            }
            if entry.is_dir {
                let child = match open_directory_component(directory, &entry.name, false) {
                    Ok(child) => child,
                    Err(CapabilityAccessError::Unsafe | CapabilityAccessError::Missing) => {
                        continue;
                    }
                    Err(error) => {
                        return Err(map_capability_error(error, &entry.relative));
                    }
                };
                if self.grep_directory(
                    &child,
                    &entry.relative,
                    pattern,
                    case_sensitive,
                    collection_limit,
                    matches,
                    cancellation,
                )? {
                    return Ok(true);
                }
            } else {
                let file = match open_file_from_parent(directory, &entry.name) {
                    Ok(file) => file,
                    Err(CapabilityAccessError::Unsafe | CapabilityAccessError::Missing) => {
                        continue;
                    }
                    Err(CapabilityAccessError::NotRegularFile) => continue,
                    Err(error) => {
                        return Err(map_capability_error(error, &entry.relative));
                    }
                };
                if self.grep_file(
                    file,
                    &entry.relative,
                    pattern,
                    case_sensitive,
                    collection_limit,
                    matches,
                    cancellation,
                )? {
                    return Ok(true);
                }
            }
        }
        check_cancelled(cancellation)?;
        Ok(false)
    }

    #[allow(clippy::too_many_arguments)]
    fn grep_file(
        &self,
        file: CapabilityFile,
        relative: &str,
        pattern: &str,
        case_sensitive: bool,
        collection_limit: usize,
        matches: &mut Vec<Value>,
        cancellation: &dyn Fn() -> bool,
    ) -> Result<bool, WorkspaceToolError> {
        check_cancelled(cancellation)?;
        let mut reader = CancellableLineReader::new(file);
        let mut raw_line = Vec::new();
        let mut file_matches = Vec::new();
        let folded_pattern = (!case_sensitive).then(|| pattern.to_lowercase());
        let mut line_number = 0usize;
        loop {
            check_cancelled(cancellation)?;
            raw_line.clear();
            let bytes_read = reader.read_until(b'\n', &mut raw_line, cancellation)?;
            check_cancelled(cancellation)?;
            if bytes_read == 0 {
                break;
            }
            if is_binary(&raw_line) {
                check_cancelled(cancellation)?;
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
                check_cancelled(cancellation)?;
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
                    check_cancelled(cancellation)?;
                    return Ok(true);
                }
            }
        }
        check_cancelled(cancellation)?;
        matches.extend(file_matches);
        Ok(false)
    }

    /// 解析一次经过严格验证的工作区相对路径。
    fn resolve_workspace_path(
        &self,
        path: &str,
        allow_protected: bool,
    ) -> Result<CapabilityRelativePath, WorkspaceToolError> {
        let path = CapabilityRelativePath::parse(path)?;
        if !allow_protected && is_protected_path(&path.display) {
            return Err(WorkspaceToolError::ProtectedPath(path.display));
        }
        Ok(path)
    }

    fn resolve_optional_workspace_path(
        &self,
        path: Option<&str>,
        allow_protected: bool,
    ) -> Result<CapabilityRelativePath, WorkspaceToolError> {
        match path {
            Some(path) => self.resolve_workspace_path(path, allow_protected),
            None => Ok(CapabilityRelativePath::root()),
        }
    }
}

fn absolute_workspace_root(path: PathBuf) -> Result<PathBuf, WorkspaceToolError> {
    let absolute = if path.is_absolute() {
        path
    } else {
        std::env::current_dir()
            .map(|current| current.join(path))
            .map_err(io_error)?
    };
    if absolute.is_absolute() {
        if absolute
            .components()
            .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
        {
            return Err(WorkspaceToolError::ReadFailed(
                "workspace root contains an unsafe component".to_string(),
            ));
        }
        Ok(absolute)
    } else {
        Err(WorkspaceToolError::ReadFailed(
            "workspace root must be an absolute directory".to_string(),
        ))
    }
}

fn bind_workspace_root(workspace_root: &Path) -> Result<CapabilityDir, WorkspaceToolError> {
    let (anchor, components) = workspace_anchor_and_components(workspace_root)?;
    let mut capability =
        CapabilityDir::open_ambient_dir(anchor, ambient_authority()).map_err(io_error)?;
    let anchor_metadata = capability.dir_metadata().map_err(io_error)?;
    if !anchor_metadata.is_dir() || metadata_is_symlink_or_reparse(&anchor_metadata) {
        return Err(WorkspaceToolError::ReadFailed(
            "workspace root anchor is not a regular directory".to_string(),
        ));
    }
    for component in components {
        capability = open_directory_component(&capability, &component, false)
            .map_err(|error| map_capability_error(error, "workspace root"))?;
    }
    Ok(capability)
}

#[cfg(windows)]
fn bind_workspace_namespace(
    workspace_root: &Path,
    capability: &CapabilityDir,
) -> Result<(PathBuf, Vec<std::fs::File>), WorkspaceToolError> {
    let (anchor, components) = workspace_anchor_and_components(workspace_root)?;
    let mut current = anchor;
    let mut guards = Vec::with_capacity(components.len().saturating_add(1));
    guards.push(open_workspace_namespace_guard(&current)?);
    for component in components {
        current.push(component);
        guards.push(open_workspace_namespace_guard(&current)?);
    }
    let final_guard = guards.last().ok_or_else(|| {
        WorkspaceToolError::PathIdentityUnsupported(
            "workspace root guard is unavailable".to_string(),
        )
    })?;
    let capability_identity = directory_object_identity_key(capability)
        .map_err(|error| map_capability_error(error, "workspace root"))?;
    let guard_identity = standard_file_object_identity_key(final_guard)
        .map_err(|error| map_capability_error(error, "workspace root"))?;
    if capability_identity != guard_identity {
        return Err(WorkspaceToolError::OutsideWorkspace(
            "workspace root changed while it was being bound".to_string(),
        ));
    }
    let actual_path = winx::file::get_file_path(final_guard).map_err(|_| {
        WorkspaceToolError::PathIdentityUnsupported(
            "workspace root handle path is unavailable".to_string(),
        )
    })?;
    Ok((actual_path, guards))
}

#[cfg(windows)]
fn open_workspace_namespace_guard(path: &Path) -> Result<std::fs::File, WorkspaceToolError> {
    let mut options = std::fs::OpenOptions::new();
    options
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT);
    let guard = options.open(path).map_err(io_error)?;
    let metadata = guard.metadata().map_err(io_error)?;
    if !metadata.is_dir()
        || StdMetadataExt::file_attributes(&metadata) & FILE_ATTRIBUTE_REPARSE_POINT != 0
    {
        return Err(WorkspaceToolError::OutsideWorkspace(
            "workspace namespace contains a reparse point".to_string(),
        ));
    }
    Ok(guard)
}

#[cfg(unix)]
fn workspace_anchor_and_components(
    workspace_root: &Path,
) -> Result<(PathBuf, Vec<OsString>), WorkspaceToolError> {
    let mut components = Vec::new();
    for component in workspace_root.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(name) => components.push(name.to_os_string()),
            Component::CurDir | Component::ParentDir | Component::Prefix(_) => {
                return Err(WorkspaceToolError::ReadFailed(
                    "workspace root contains an unsafe component".to_string(),
                ));
            }
        }
    }
    Ok((PathBuf::from("/"), components))
}

#[cfg(windows)]
fn workspace_anchor_and_components(
    workspace_root: &Path,
) -> Result<(PathBuf, Vec<OsString>), WorkspaceToolError> {
    let mut components = workspace_root.components();
    let prefix = match components.next() {
        Some(Component::Prefix(prefix)) => prefix,
        _ => {
            return Err(WorkspaceToolError::ReadFailed(
                "workspace root must use a drive or share anchor".to_string(),
            ));
        }
    };
    if !matches!(
        prefix.kind(),
        Prefix::Disk(_) | Prefix::VerbatimDisk(_) | Prefix::UNC(_, _) | Prefix::VerbatimUNC(_, _)
    ) {
        return Err(WorkspaceToolError::ReadFailed(
            "workspace root must use a drive or share anchor".to_string(),
        ));
    }
    if !matches!(components.next(), Some(Component::RootDir)) {
        return Err(WorkspaceToolError::ReadFailed(
            "workspace root must use an absolute drive or share path".to_string(),
        ));
    }
    let mut anchor = prefix.as_os_str().to_os_string();
    anchor.push("\\");
    let mut relative_components = Vec::new();
    for component in components {
        match component {
            Component::Normal(name) => {
                validate_windows_component(name)?;
                relative_components.push(name.to_os_string());
            }
            Component::CurDir
            | Component::ParentDir
            | Component::RootDir
            | Component::Prefix(_) => {
                return Err(WorkspaceToolError::ReadFailed(
                    "workspace root contains an unsafe component".to_string(),
                ));
            }
        }
    }
    Ok((PathBuf::from(anchor), relative_components))
}

#[cfg(not(any(unix, windows)))]
fn workspace_anchor_and_components(
    _workspace_root: &Path,
) -> Result<(PathBuf, Vec<OsString>), WorkspaceToolError> {
    Err(WorkspaceToolError::ReadFailed(
        "workspace filesystem capability is unsupported on this platform".to_string(),
    ))
}

fn preflight_input<T>(input: &Value) -> Result<T, WorkspaceToolError>
where
    T: for<'de> Deserialize<'de>,
{
    serde_json::from_value(input.clone())
        .map_err(|_| WorkspaceToolError::InvalidInput("invalid tool input".to_string()))
}

fn ensure_authorization(
    entry: &ToolEntry,
    expected: ToolAuthorization,
) -> Result<(), WorkspaceToolError> {
    if entry.authorization == expected {
        Ok(())
    } else {
        Err(WorkspaceToolError::InvalidInput(
            "tool executor and authorization binding differ".to_string(),
        ))
    }
}

fn serialization_error(_error: serde_json::Error) -> WorkspaceToolError {
    WorkspaceToolError::InvalidInput("tool execution input serialization failed".to_string())
}

fn check_cancelled(cancellation: &dyn Fn() -> bool) -> Result<(), WorkspaceToolError> {
    if cancellation() {
        Err(WorkspaceToolError::Cancelled)
    } else {
        Ok(())
    }
}

struct CancellableLineReader<R> {
    reader: R,
    chunk: [u8; FILE_READ_CHUNK_SIZE],
    chunk_start: usize,
    chunk_end: usize,
}

impl<R: Read> CancellableLineReader<R> {
    fn new(reader: R) -> Self {
        Self {
            reader,
            chunk: [0; FILE_READ_CHUNK_SIZE],
            chunk_start: 0,
            chunk_end: 0,
        }
    }

    fn read_until(
        &mut self,
        delimiter: u8,
        output: &mut Vec<u8>,
        cancellation: &dyn Fn() -> bool,
    ) -> Result<usize, WorkspaceToolError> {
        output.clear();
        loop {
            check_cancelled(cancellation)?;
            if self.chunk_start == self.chunk_end {
                let bytes_read = self.reader.read(&mut self.chunk).map_err(io_error)?;
                check_cancelled(cancellation)?;
                self.chunk_start = 0;
                self.chunk_end = bytes_read;
                if bytes_read == 0 {
                    return Ok(output.len());
                }
            }

            let available = &self.chunk[self.chunk_start..self.chunk_end];
            if let Some(delimiter_index) = available.iter().position(|byte| *byte == delimiter) {
                let end = delimiter_index.saturating_add(1);
                output.extend_from_slice(&available[..end]);
                self.chunk_start = self.chunk_start.saturating_add(end);
                check_cancelled(cancellation)?;
                return Ok(output.len());
            }
            output.extend_from_slice(available);
            self.chunk_start = self.chunk_end;
            check_cancelled(cancellation)?;
        }
    }
}

#[derive(Debug, Clone)]
struct DirectoryEntry {
    name: OsString,
    relative: String,
    is_dir: bool,
    is_symlink_or_reparse: bool,
}

struct BoundCommandCwd {
    path: PathBuf,
    _capability_guard: CapabilityDir,
    #[cfg(windows)]
    _namespace_guard: std::fs::File,
    #[cfg(not(windows))]
    _namespace_guard: CapabilityDir,
}

#[derive(Clone)]
struct PreparedMutation {
    path: CapabilityRelativePath,
    relative: String,
    original: String,
    updated: String,
    original_identity: Option<String>,
}

struct PublishedMutation {
    prepared: PreparedMutation,
    published_identity: String,
}

#[derive(Debug)]
struct AtomicWriteFailure {
    error: WorkspaceToolError,
    published_identity: Option<String>,
}

impl AtomicWriteFailure {
    fn published(error: WorkspaceToolError, published_identity: String) -> Self {
        Self {
            error,
            published_identity: Some(published_identity),
        }
    }
}

impl From<WorkspaceToolError> for AtomicWriteFailure {
    fn from(error: WorkspaceToolError) -> Self {
        Self {
            error,
            published_identity: None,
        }
    }
}

struct AtomicTargetState {
    identity: String,
    permissions: CapabilityPermissions,
}

struct CreatedDirectory {
    path: CapabilityRelativePath,
    identity: String,
    _guard: Option<CapabilityDir>,
}

/// 已验证的工作区相对路径；其 `relative`、显示值和重复键来自同一次解析。
#[derive(Debug, Clone)]
struct CapabilityRelativePath {
    relative: PathBuf,
    display: String,
    key: String,
}

impl CapabilityRelativePath {
    fn root() -> Self {
        Self {
            relative: PathBuf::from("."),
            display: ".".to_string(),
            key: ".".to_string(),
        }
    }

    fn parse(path: &str) -> Result<Self, WorkspaceToolError> {
        if path == "." {
            return Ok(Self::root());
        }
        if Path::new(path).is_absolute()
            || Path::new(path)
                .components()
                .any(|component| matches!(component, Component::RootDir | Component::Prefix(_)))
        {
            return Err(WorkspaceToolError::OutsideWorkspace(
                "absolute workspace path is not allowed".to_string(),
            ));
        }
        if path.trim().is_empty() || path.contains('\0') || path.contains('\\') {
            return Err(WorkspaceToolError::InvalidInput(
                "workspace path must use non-empty slash-separated relative components".to_string(),
            ));
        }

        let mut relative = PathBuf::new();
        let mut component_count = 0usize;
        for component in path.split('/') {
            if component.is_empty() {
                return Err(WorkspaceToolError::InvalidInput(
                    "workspace path contains an empty component".to_string(),
                ));
            }
            let component_path = Path::new(component);
            let mut parsed = component_path.components();
            let name = match (parsed.next(), parsed.next()) {
                (Some(Component::Normal(name)), None) => name,
                _ => {
                    return Err(WorkspaceToolError::InvalidInput(
                        "workspace path must contain only normal relative components".to_string(),
                    ));
                }
            };
            #[cfg(windows)]
            validate_windows_component(name)?;
            relative.push(name);
            component_count = component_count.saturating_add(1);
        }
        if component_count == 0 {
            return Err(WorkspaceToolError::InvalidInput(
                "workspace path must not be empty".to_string(),
            ));
        }
        let display = relative_display(&relative);
        let key = relative_path_key(&relative);
        Ok(Self {
            relative,
            display,
            key,
        })
    }
}

#[derive(Debug)]
struct ParentDirectory<'a> {
    directory: ParentDirectoryKind<'a>,
    name: OsString,
    actual_relative: String,
}

#[derive(Debug)]
enum ParentDirectoryKind<'a> {
    Root(&'a CapabilityDir),
    Opened(CapabilityDir),
}

impl ParentDirectory<'_> {
    fn dir(&self) -> &CapabilityDir {
        match &self.directory {
            ParentDirectoryKind::Root(directory) => directory,
            ParentDirectoryKind::Opened(directory) => directory,
        }
    }
}

struct OpenedWorkspacePath {
    is_directory: bool,
    file: Option<CapabilityFile>,
}

#[derive(Debug)]
enum CapabilityAccessError {
    Missing,
    Unsafe,
    Protected(String),
    PathIdentityUnsupported,
    NotDirectory,
    NotRegularFile,
    HardLinked,
    Unsupported,
    Io(std::io::Error),
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

fn normal_component(component: Component<'_>) -> Result<&OsStr, CapabilityAccessError> {
    match component {
        Component::Normal(name) => Ok(name),
        Component::CurDir | Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
            Err(CapabilityAccessError::Unsafe)
        }
    }
}

#[cfg(windows)]
fn validate_windows_component(name: &OsStr) -> Result<(), WorkspaceToolError> {
    let name = name.to_string_lossy();
    let upper = name.to_ascii_uppercase();
    let stem = upper.split('.').next().unwrap_or_default();
    let dos_device = matches!(
        stem,
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM0"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT0"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
            | "CONIN$"
            | "CONOUT$"
            | "CLOCK$"
            | "COM¹"
            | "COM²"
            | "COM³"
            | "LPT¹"
            | "LPT²"
            | "LPT³"
    );
    let short_name_alias = stem.rsplit_once('~').is_some_and(|(prefix, suffix)| {
        !prefix.is_empty()
            && !suffix.is_empty()
            && suffix.len() <= 6
            && suffix.chars().all(|character| character.is_ascii_digit())
    });
    if name.contains(':')
        || name.ends_with('.')
        || name.ends_with(' ')
        || dos_device
        || short_name_alias
    {
        return Err(WorkspaceToolError::InvalidInput(
            "workspace path contains an unsupported Windows component".to_string(),
        ));
    }
    Ok(())
}

fn classify_io_error(error: std::io::Error) -> CapabilityAccessError {
    if error.kind() == std::io::ErrorKind::NotFound {
        CapabilityAccessError::Missing
    } else if is_symlink_io_error(&error) {
        CapabilityAccessError::Unsafe
    } else {
        CapabilityAccessError::Io(error)
    }
}

fn map_capability_error(error: CapabilityAccessError, relative: &str) -> WorkspaceToolError {
    match error {
        CapabilityAccessError::Missing => {
            WorkspaceToolError::ReadFailed("workspace path is unavailable".to_string())
        }
        CapabilityAccessError::Unsafe => WorkspaceToolError::OutsideWorkspace(relative.to_string()),
        CapabilityAccessError::Protected(path) => WorkspaceToolError::ProtectedPath(path),
        CapabilityAccessError::PathIdentityUnsupported => {
            WorkspaceToolError::PathIdentityUnsupported(relative.to_string())
        }
        CapabilityAccessError::NotDirectory => {
            WorkspaceToolError::ReadFailed("workspace path is not a directory".to_string())
        }
        CapabilityAccessError::NotRegularFile => {
            WorkspaceToolError::ReadFailed("workspace path is not a regular file".to_string())
        }
        CapabilityAccessError::HardLinked => {
            WorkspaceToolError::HardLinkRejected(relative.to_string())
        }
        CapabilityAccessError::Unsupported => {
            WorkspaceToolError::PathIdentityUnsupported(relative.to_string())
        }
        CapabilityAccessError::Io(error) => io_error(error),
    }
}

fn open_directory_component(
    parent: &CapabilityDir,
    name: &OsStr,
    create_missing: bool,
) -> Result<CapabilityDir, CapabilityAccessError> {
    match parent.symlink_metadata(name) {
        Ok(metadata) => {
            if metadata_is_symlink_or_reparse(&metadata) {
                return Err(CapabilityAccessError::Unsafe);
            }
            if !metadata.is_dir() {
                return Err(CapabilityAccessError::NotDirectory);
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && create_missing => {
            match parent.create_dir(name) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(classify_io_error(error)),
            }
        }
        Err(error) => return Err(classify_io_error(error)),
    }
    let directory = parent.open_dir_nofollow(name).map_err(classify_io_error)?;
    let metadata = directory.dir_metadata().map_err(classify_io_error)?;
    if metadata_is_symlink_or_reparse(&metadata) {
        return Err(CapabilityAccessError::Unsafe);
    }
    if !metadata.is_dir() {
        return Err(CapabilityAccessError::NotDirectory);
    }
    Ok(directory)
}

fn open_file_from_parent(
    parent: &CapabilityDir,
    name: &OsStr,
) -> Result<CapabilityFile, CapabilityAccessError> {
    let metadata = parent.symlink_metadata(name).map_err(classify_io_error)?;
    if metadata_is_symlink_or_reparse(&metadata) {
        return Err(CapabilityAccessError::Unsafe);
    }
    if !metadata.is_file() {
        return Err(CapabilityAccessError::NotRegularFile);
    }
    let file = parent
        .open_with(name, &nofollow_file_options(true, false, false))
        .map_err(classify_io_error)?;
    let metadata = file.metadata().map_err(classify_io_error)?;
    if metadata_is_symlink_or_reparse(&metadata) {
        return Err(CapabilityAccessError::Unsafe);
    }
    if !metadata.is_file() {
        return Err(CapabilityAccessError::NotRegularFile);
    }
    reject_multiple_hard_links(&file)?;
    Ok(file)
}

fn create_unique_temp_file(
    parent: &CapabilityDir,
) -> Result<(OsString, CapabilityFile), CapabilityAccessError> {
    for _ in 0..MUTATION_TEMP_FILE_ATTEMPTS {
        let sequence = MUTATION_TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let temp_name = OsString::from(format!(
            ".singularity-tmp-{}-{sequence}",
            std::process::id()
        ));
        match parent.open_with(&temp_name, &nofollow_file_options(false, true, true)) {
            Ok(file) => return Ok((temp_name, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(classify_io_error(error)),
        }
    }
    Err(CapabilityAccessError::Io(std::io::Error::other(
        "failed to allocate workspace temporary file",
    )))
}

fn cleanup_owned_file(
    parent: &CapabilityDir,
    name: &OsStr,
    expected_identity: &str,
    failure: WorkspaceToolError,
) -> WorkspaceToolError {
    let current_identity = match open_file_from_parent(parent, name)
        .and_then(|file| file_object_identity_key(&file))
    {
        Ok(identity) => identity,
        Err(CapabilityAccessError::Missing) => return failure,
        Err(_) => {
            return WorkspaceToolError::RollbackFailed(
                "workspace temporary file identity check failed".to_string(),
            );
        }
    };
    if current_identity != expected_identity {
        return WorkspaceToolError::RollbackFailed(
            "workspace temporary file was replaced before cleanup".to_string(),
        );
    }
    match parent.remove_file_or_symlink(name) {
        Ok(()) => failure,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => failure,
        Err(_) => WorkspaceToolError::RollbackFailed(
            "workspace temporary file cleanup failed".to_string(),
        ),
    }
}

fn reject_multiple_hard_links(file: &CapabilityFile) -> Result<(), CapabilityAccessError> {
    let links = file_link_count(file)?;
    if links > 1 {
        Err(CapabilityAccessError::HardLinked)
    } else {
        Ok(())
    }
}

#[cfg(unix)]
fn file_link_count(file: &CapabilityFile) -> Result<u64, CapabilityAccessError> {
    use std::os::unix::fs::MetadataExt as _;

    let links = file
        .try_clone()
        .map_err(CapabilityAccessError::Io)?
        .into_std()
        .metadata()
        .map(|metadata| metadata.nlink())
        .map_err(|_| CapabilityAccessError::Unsupported)?;
    if links == 0 {
        Err(CapabilityAccessError::Unsupported)
    } else {
        Ok(links)
    }
}

#[cfg(windows)]
fn file_link_count(file: &CapabilityFile) -> Result<u64, CapabilityAccessError> {
    let standard_file = file
        .try_clone()
        .map_err(CapabilityAccessError::Io)?
        .into_std();
    let links = winx::winapi_util::file::information(&standard_file)
        .map(|information| information.number_of_links())
        .map_err(|_| CapabilityAccessError::Unsupported)?;
    if links == 0 {
        Err(CapabilityAccessError::Unsupported)
    } else {
        Ok(links)
    }
}

#[cfg(not(any(unix, windows)))]
fn file_link_count(_file: &CapabilityFile) -> Result<u64, CapabilityAccessError> {
    Err(CapabilityAccessError::Unsupported)
}

#[cfg(unix)]
fn file_object_identity_key(file: &CapabilityFile) -> Result<String, CapabilityAccessError> {
    use std::os::unix::fs::MetadataExt as _;

    let metadata = file
        .try_clone()
        .map_err(CapabilityAccessError::Io)?
        .into_std()
        .metadata()
        .map_err(CapabilityAccessError::Io)?;
    if metadata.dev() == 0 && metadata.ino() == 0 {
        return Err(CapabilityAccessError::Unsupported);
    }
    Ok(format!("object:{:x}:{:x}", metadata.dev(), metadata.ino()))
}

#[cfg(windows)]
fn file_object_identity_key(file: &CapabilityFile) -> Result<String, CapabilityAccessError> {
    let standard_file = file
        .try_clone()
        .map_err(CapabilityAccessError::Io)?
        .into_std();
    let information = winx::winapi_util::file::information(&standard_file)
        .map_err(|_| CapabilityAccessError::Unsupported)?;
    let volume = information.volume_serial_number();
    let index = information.file_index();
    if volume == 0 || index == 0 {
        return Err(CapabilityAccessError::Unsupported);
    }
    Ok(format!("object:{volume:x}:{index:x}"))
}

#[cfg(windows)]
fn directory_object_identity_key(
    directory: &CapabilityDir,
) -> Result<String, CapabilityAccessError> {
    let standard_file = directory
        .try_clone()
        .map_err(CapabilityAccessError::Io)?
        .into_std_file();
    standard_file_object_identity_key(&standard_file)
}

#[cfg(unix)]
fn directory_object_identity_key(
    directory: &CapabilityDir,
) -> Result<String, CapabilityAccessError> {
    use std::os::unix::fs::MetadataExt as _;

    let metadata = directory
        .try_clone()
        .map_err(CapabilityAccessError::Io)?
        .into_std_file()
        .metadata()
        .map_err(CapabilityAccessError::Io)?;
    if metadata.dev() == 0 && metadata.ino() == 0 {
        return Err(CapabilityAccessError::PathIdentityUnsupported);
    }
    Ok(format!("object:{:x}:{:x}", metadata.dev(), metadata.ino()))
}

#[cfg(not(any(unix, windows)))]
fn directory_object_identity_key(
    _directory: &CapabilityDir,
) -> Result<String, CapabilityAccessError> {
    Err(CapabilityAccessError::PathIdentityUnsupported)
}

#[cfg(windows)]
fn standard_file_object_identity_key(
    file: &std::fs::File,
) -> Result<String, CapabilityAccessError> {
    let information = winx::winapi_util::file::information(file)
        .map_err(|_| CapabilityAccessError::PathIdentityUnsupported)?;
    let volume = information.volume_serial_number();
    let index = information.file_index();
    if volume == 0 || index == 0 {
        return Err(CapabilityAccessError::PathIdentityUnsupported);
    }
    Ok(format!("object:{volume:x}:{index:x}"))
}

#[cfg(not(any(unix, windows)))]
fn file_object_identity_key(_file: &CapabilityFile) -> Result<String, CapabilityAccessError> {
    Err(CapabilityAccessError::Unsupported)
}

#[cfg(windows)]
// Handle paths are used only to recover the actual entry spelling for the
// protected-path check. They never feed an open, traversal, or rename call.
fn relative_path_from_handles(
    root: &CapabilityDir,
    directory: &CapabilityDir,
) -> Result<String, CapabilityAccessError> {
    let root_path = winx::file::get_file_path(
        &root
            .try_clone()
            .map_err(CapabilityAccessError::Io)?
            .into_std_file(),
    )
    .map_err(|_| CapabilityAccessError::PathIdentityUnsupported)?;
    let directory_path = winx::file::get_file_path(
        &directory
            .try_clone()
            .map_err(CapabilityAccessError::Io)?
            .into_std_file(),
    )
    .map_err(|_| CapabilityAccessError::PathIdentityUnsupported)?;
    relative_windows_handle_path(&root_path, &directory_path)
}

#[cfg(windows)]
fn relative_path_from_file_handle(
    root: &CapabilityDir,
    file: &CapabilityFile,
) -> Result<String, CapabilityAccessError> {
    let root_path = winx::file::get_file_path(
        &root
            .try_clone()
            .map_err(CapabilityAccessError::Io)?
            .into_std_file(),
    )
    .map_err(|_| CapabilityAccessError::PathIdentityUnsupported)?;
    let file_path = winx::file::get_file_path(
        &file
            .try_clone()
            .map_err(CapabilityAccessError::Io)?
            .into_std(),
    )
    .map_err(|_| CapabilityAccessError::PathIdentityUnsupported)?;
    relative_windows_handle_path(&root_path, &file_path)
}

#[cfg(windows)]
fn relative_windows_handle_path(
    root: &Path,
    object: &Path,
) -> Result<String, CapabilityAccessError> {
    let (root_keys, _) = windows_handle_components(root)?;
    let (object_keys, object_names) = windows_handle_components(object)?;
    if object_keys.len() < root_keys.len() || object_keys[..root_keys.len()] != root_keys[..] {
        return Err(CapabilityAccessError::Unsafe);
    }
    let root_normal_count = root
        .components()
        .filter(|component| matches!(component, Component::Normal(_)))
        .count();
    let relative = object_names
        .into_iter()
        .skip(root_normal_count)
        .collect::<PathBuf>();
    Ok(relative_display(&relative))
}

#[cfg(windows)]
fn windows_handle_components(
    path: &Path,
) -> Result<(Vec<String>, Vec<OsString>), CapabilityAccessError> {
    let mut keys = Vec::new();
    let mut names = Vec::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => keys.push(format!(
                "prefix:{}",
                windows_case_key(&prefix.as_os_str().to_string_lossy())
            )),
            Component::RootDir => keys.push("root".to_string()),
            Component::Normal(name) => {
                keys.push(format!(
                    "normal:{}",
                    windows_case_key(&name.to_string_lossy())
                ));
                names.push(name.to_os_string());
            }
            Component::CurDir | Component::ParentDir => {
                return Err(CapabilityAccessError::PathIdentityUnsupported);
            }
        }
    }
    Ok((keys, names))
}

fn nofollow_file_options(read: bool, write: bool, create_new: bool) -> CapabilityOpenOptions {
    let mut options = CapabilityOpenOptions::new();
    options
        .read(read)
        .write(write)
        .create_new(create_new)
        .follow(FollowSymlinks::No);
    #[cfg(windows)]
    options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    options
}

fn is_symlink_io_error(error: &std::io::Error) -> bool {
    #[cfg(unix)]
    if error.raw_os_error() == Some(ERROR_TOO_MANY_SYMLINKS) {
        return true;
    }
    #[cfg(windows)]
    if error.raw_os_error() == Some(ERROR_STOPPED_ON_SYMLINK) {
        return true;
    }
    false
}

fn join_relative_path(prefix: &str, name: &OsStr) -> String {
    let name = name.to_string_lossy().replace('\\', "/");
    if prefix.is_empty() || prefix == "." {
        name
    } else {
        format!("{prefix}/{name}")
    }
}

fn relative_display(relative: &Path) -> String {
    let display = relative
        .to_string_lossy()
        .trim_start_matches(['/', '\\'])
        .replace('\\', "/");
    if display.is_empty() {
        ".".to_string()
    } else {
        display
    }
}

fn relative_path_key(relative: &Path) -> String {
    let display = relative_display(relative);
    #[cfg(windows)]
    let normalized = windows_case_key(&display);
    #[cfg(not(windows))]
    let normalized = display;
    format!("path:{normalized}")
}

#[cfg(windows)]
fn windows_case_key(value: &str) -> String {
    value.chars().flat_map(char::to_lowercase).collect()
}

fn metadata_is_symlink_or_reparse(metadata: &CapabilityMetadata) -> bool {
    metadata.file_type().is_symlink() || metadata_is_reparse_point(metadata)
}

#[cfg(windows)]
fn metadata_is_reparse_point(metadata: &CapabilityMetadata) -> bool {
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn metadata_is_reparse_point(_metadata: &CapabilityMetadata) -> bool {
    false
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CommandModelInput {
    command: String,
    cwd: Option<String>,
    timeout_seconds: Option<u64>,
}

impl CommandModelInput {
    fn validate(&self) -> Result<(), WorkspaceToolError> {
        CommandToolInput {
            command: self.command.clone(),
            cwd: self.cwd.clone(),
            timeout_seconds: self.timeout_seconds,
        }
        .validate()
    }
}

/// 面向模型的命令输入；执行策略由受信任的 sandbox 路径固定提供。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CommandToolInput {
    pub command: String,
    pub cwd: Option<String>,
    pub timeout_seconds: Option<u64>,
}

impl CommandToolInput {
    fn validate(&self) -> Result<(), WorkspaceToolError> {
        if self.command.trim().is_empty() {
            return Err(WorkspaceToolError::InvalidInput(
                "command must not be empty".to_string(),
            ));
        }
        if self.command.contains('\0') {
            return Err(WorkspaceToolError::InvalidInput(
                "command must not contain NUL".to_string(),
            ));
        }
        if self.command.chars().count() > MAX_COMMAND_SCRIPT_CHARS {
            return Err(WorkspaceToolError::InvalidInput(format!(
                "command must not exceed {MAX_COMMAND_SCRIPT_CHARS} characters"
            )));
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

#[derive(Serialize)]
struct CommandScriptScope<'a> {
    command: &'a str,
    cwd: &'a str,
    timeout_seconds: u64,
    sandbox_mode: SandboxFilesystemMode,
    network_access: SandboxNetworkMode,
}

impl<'a> CommandScriptScope<'a> {
    fn new(
        command: &'a str,
        cwd: &'a str,
        timeout_seconds: u64,
        sandbox_mode: SandboxFilesystemMode,
        network_access: SandboxNetworkMode,
    ) -> Self {
        Self {
            command,
            cwd,
            timeout_seconds,
            sandbox_mode,
            network_access,
        }
    }

    fn digest(&self) -> String {
        let encoded = serde_json::to_vec(self).expect("command script scope is serializable");
        format!("sha256:{:x}", Sha256::digest(encoded))
    }
}

/// 对模型 command string 及其固定的只读、离线执行范围计算哈希。
pub fn command_script_scope_digest(command: &str, cwd: &str, timeout_seconds: u64) -> String {
    command_script_scope_digest_with_policy(
        command,
        cwd,
        timeout_seconds,
        SandboxFilesystemMode::ReadOnly,
        SandboxNetworkMode::Denied,
    )
}

/// 对模型 command 及其经 Policy 绑定的执行范围计算哈希。
pub fn command_script_scope_digest_with_policy(
    command: &str,
    cwd: &str,
    timeout_seconds: u64,
    sandbox_mode: SandboxFilesystemMode,
    network_access: SandboxNetworkMode,
) -> String {
    CommandScriptScope::new(command, cwd, timeout_seconds, sandbox_mode, network_access).digest()
}

fn validate_tool_name(name: &str) -> Result<(), String> {
    if name.starts_with("builtin_")
        || name.is_empty()
        || name.len() > 64
        || !name
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
    } else if contains_artifact_reference(text) {
        ARTIFACT_REFERENCE_OMITTED.to_string()
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

fn is_binary(bytes: &[u8]) -> bool {
    bytes.contains(&0)
}

fn bounded_text(content: &str, max_chars: usize) -> (String, bool) {
    let preview = content.chars().take(max_chars).collect::<String>();
    let truncated = content.chars().count() > preview.chars().count();
    (preview, truncated)
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
    // 模型只接收命令语义和有界输出，backend/enforcement 仅保留在 audit metadata。
    let content = json!({
        "execution_status": result.execution_status,
        "semantic_status": result.semantic_status,
        "exit_code": result.exit_code,
        "duration_ms": result.duration_ms,
        "timed_out": result.timed_out,
        "stdout_preview": result.stdout_preview,
        "stderr_preview": result.stderr_preview,
        "output_truncated": result.output_truncated,
        "redacted": result.redacted,
    });
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

#[cfg(test)]
mod cancellation_tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    fn test_workspace(name: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "singularity-tools-cancellation-{name}-{}-{sequence}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("create test workspace");
        path
    }

    fn remove_workspace(path: &Path) {
        let _ = std::fs::remove_dir_all(path);
    }

    fn cancel_after(checks: &AtomicUsize, threshold: usize) -> impl Fn() -> bool + '_ {
        move || checks.fetch_add(1, Ordering::SeqCst).saturating_add(1) >= threshold
    }

    #[test]
    fn cancellable_read_stops_after_a_file_chunk_boundary() {
        let workspace = test_workspace("read-boundary");
        let content = "x".repeat(FILE_READ_CHUNK_SIZE.saturating_mul(3));
        std::fs::write(workspace.join("lines.txt"), content).expect("write lines");
        let tools = WorkspaceTools::new(&workspace).expect("bind workspace tools");
        let checks = AtomicUsize::new(0);

        let result = tools.read_with_cancellation_check(
            ReadToolInput {
                path: "lines.txt".to_string(),
                max_chars: None,
                line_start: None,
                line_end: None,
            },
            &cancel_after(&checks, 9),
        );

        assert!(matches!(result, Err(WorkspaceToolError::Cancelled)));
        assert!(checks.load(Ordering::SeqCst) >= 9);
        remove_workspace(&workspace);
    }

    #[test]
    fn cancellable_recursive_list_stops_at_an_entry_boundary() {
        let workspace = test_workspace("list-boundary");
        for directory_index in 0..4 {
            let directory = workspace.join(format!("dir-{directory_index}"));
            std::fs::create_dir_all(directory.join("nested")).expect("create nested directory");
            for file_index in 0..4 {
                std::fs::write(
                    directory.join(format!("file-{file_index}.txt")),
                    "content\n",
                )
                .expect("write nested file");
            }
            std::fs::write(directory.join("nested").join("deep.txt"), "deep\n")
                .expect("write deep file");
        }
        let tools = WorkspaceTools::new(&workspace).expect("bind workspace tools");
        let checks = AtomicUsize::new(0);

        let result = tools.list_with_cancellation_check(
            ListToolInput {
                path: None,
                max_entries: Some(1_000),
                recursive: true,
                max_depth: Some(8),
            },
            &cancel_after(&checks, 45),
        );

        assert!(matches!(result, Err(WorkspaceToolError::Cancelled)));
        assert!(checks.load(Ordering::SeqCst) >= 45);
        remove_workspace(&workspace);
    }

    #[test]
    fn cancellable_recursive_grep_stops_at_a_file_boundary() {
        let workspace = test_workspace("grep-boundary");
        for directory_index in 0..4 {
            let directory = workspace.join(format!("dir-{directory_index}"));
            std::fs::create_dir_all(&directory).expect("create directory");
            for file_index in 0..4 {
                std::fs::write(
                    directory.join(format!("file-{file_index}.txt")),
                    "no match\nno match\n",
                )
                .expect("write grep file");
            }
        }
        let tools = WorkspaceTools::new(&workspace).expect("bind workspace tools");
        let checks = AtomicUsize::new(0);

        let result = tools.grep_with_cancellation_check(
            GrepToolInput {
                path: None,
                pattern: "needle".to_string(),
                max_matches: Some(1_000),
                case_sensitive: true,
            },
            &cancel_after(&checks, 45),
        );

        assert!(matches!(result, Err(WorkspaceToolError::Cancelled)));
        assert!(checks.load(Ordering::SeqCst) >= 45);
        remove_workspace(&workspace);
    }
}

#[cfg(test)]
mod mutation_tests {
    use super::*;

    fn test_workspace(name: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "singularity-tools-mutation-{name}-{}-{sequence}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("create test workspace");
        path
    }

    fn remove_workspace(path: &Path) {
        let _ = std::fs::remove_dir_all(path);
    }

    #[test]
    fn atomic_write_rejects_target_replacement_and_cleans_its_temp_file() {
        let workspace = test_workspace("target-replacement");
        let target = workspace.join("target.txt");
        std::fs::write(&target, "before").expect("write target");
        let tools = WorkspaceTools::new(&workspace).expect("bind workspace tools");
        let path = CapabilityRelativePath::parse("target.txt").expect("relative path");
        let (_, original_identity) = tools.existing_text_or_empty(&path).expect("read original");

        let result =
            tools.atomic_write_with_hook(&path, "after", original_identity.as_deref(), |_| {
                std::fs::remove_file(&target).expect("remove original");
                std::fs::write(&target, "concurrent").expect("write concurrent target");
            });

        assert!(matches!(
            result,
            Err(AtomicWriteFailure {
                error: WorkspaceToolError::ConcurrentMutation(_),
                published_identity: None,
            })
        ));
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "concurrent");
        assert!(
            std::fs::read_dir(&workspace)
                .expect("read workspace")
                .all(|entry| !entry
                    .expect("entry")
                    .file_name()
                    .to_string_lossy()
                    .contains("singularity-tmp"))
        );
        remove_workspace(&workspace);
    }

    #[test]
    fn atomic_write_does_not_delete_a_replaced_temp_source() {
        let workspace = test_workspace("temp-replacement");
        let target = workspace.join("target.txt");
        std::fs::write(&target, "before").expect("write target");
        let tools = WorkspaceTools::new(&workspace).expect("bind workspace tools");
        let path = CapabilityRelativePath::parse("target.txt").expect("relative path");
        let (_, original_identity) = tools.existing_text_or_empty(&path).expect("read original");
        let mut replacement_path = None;

        let result = tools.atomic_write_with_hook(
            &path,
            "after",
            original_identity.as_deref(),
            |temporary_name| {
                let temporary_path = workspace.join(temporary_name);
                std::fs::remove_file(&temporary_path).expect("remove owned temp");
                std::fs::write(&temporary_path, "concurrent temp").expect("write replacement temp");
                replacement_path = Some(temporary_path);
            },
        );

        assert!(matches!(
            result,
            Err(AtomicWriteFailure {
                error: WorkspaceToolError::ConcurrentMutation(_),
                published_identity: None,
            })
        ));
        let replacement_path = replacement_path.expect("replacement path");
        assert_eq!(
            std::fs::read_to_string(&replacement_path).unwrap(),
            "concurrent temp"
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "before");
        remove_workspace(&workspace);
    }

    #[test]
    fn rollback_restores_only_published_mutations() {
        let workspace = test_workspace("published-rollback");
        std::fs::write(workspace.join("existing.txt"), "before").expect("write existing target");
        let tools = WorkspaceTools::new(&workspace).expect("bind workspace tools");
        let existing_path = CapabilityRelativePath::parse("existing.txt").expect("existing path");
        let created_path = CapabilityRelativePath::parse("created.txt").expect("created path");
        let (original, original_identity) = tools
            .existing_text_or_empty(&existing_path)
            .expect("read existing");
        let existing_published = tools
            .atomic_write(&existing_path, "after", original_identity.as_deref())
            .expect("publish existing");
        let created_published = tools
            .atomic_write(&created_path, "created", None)
            .expect("publish created");
        let published = vec![
            PublishedMutation {
                prepared: PreparedMutation {
                    path: existing_path,
                    relative: "existing.txt".to_string(),
                    original,
                    updated: "after".to_string(),
                    original_identity,
                },
                published_identity: existing_published,
            },
            PublishedMutation {
                prepared: PreparedMutation {
                    path: created_path,
                    relative: "created.txt".to_string(),
                    original: String::new(),
                    updated: "created".to_string(),
                    original_identity: None,
                },
                published_identity: created_published,
            },
        ];

        tools
            .rollback_published(&published)
            .expect("rollback published mutations");

        assert_eq!(
            std::fs::read_to_string(workspace.join("existing.txt")).unwrap(),
            "before"
        );
        assert!(!workspace.join("created.txt").exists());
        remove_workspace(&workspace);
    }

    #[test]
    fn post_publish_failure_includes_current_mutation_in_safe_rollback() {
        let workspace = test_workspace("post-publish-rollback");
        let target = workspace.join("target.txt");
        std::fs::write(&target, "before").expect("write target");
        let tools = WorkspaceTools::new(&workspace).expect("bind workspace tools");
        let path = CapabilityRelativePath::parse("target.txt").expect("relative path");
        let (original, original_identity) =
            tools.existing_text_or_empty(&path).expect("read original");

        let failure = tools
            .atomic_write_with_hooks(
                &path,
                "published",
                original_identity.as_deref(),
                |_| {},
                || {
                    Err(WorkspaceToolError::ConcurrentMutation(
                        "target.txt".to_string(),
                    ))
                },
            )
            .expect_err("post-publish verification fails");
        let published_identity = failure
            .published_identity
            .expect("failure retains published identity");
        let published = vec![PublishedMutation {
            prepared: PreparedMutation {
                path,
                relative: "target.txt".to_string(),
                original,
                updated: "published".to_string(),
                original_identity,
            },
            published_identity,
        }];

        tools
            .rollback_published(&published)
            .expect("rollback current published mutation");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "before");
        remove_workspace(&workspace);
    }

    #[test]
    fn rollback_preserves_a_concurrently_replaced_published_target() {
        let workspace = test_workspace("rollback-concurrent");
        let target = workspace.join("target.txt");
        std::fs::write(&target, "before").expect("write target");
        let tools = WorkspaceTools::new(&workspace).expect("bind workspace tools");
        let path = CapabilityRelativePath::parse("target.txt").expect("relative path");
        let (original, original_identity) =
            tools.existing_text_or_empty(&path).expect("read original");
        let published_identity = tools
            .atomic_write(&path, "published", original_identity.as_deref())
            .expect("publish mutation");
        std::fs::remove_file(&target).expect("remove published target");
        std::fs::write(&target, "concurrent").expect("write concurrent target");
        let published = vec![PublishedMutation {
            prepared: PreparedMutation {
                path,
                relative: "target.txt".to_string(),
                original,
                updated: "published".to_string(),
                original_identity,
            },
            published_identity,
        }];

        assert!(matches!(
            tools.rollback_published(&published),
            Err(WorkspaceToolError::RollbackFailed(_))
        ));
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "concurrent");
        remove_workspace(&workspace);
    }

    #[test]
    fn failed_batch_cleanup_removes_only_its_nested_directories() {
        let workspace = test_workspace("directory-cleanup");
        let tools = WorkspaceTools::new(&workspace).expect("bind workspace tools");
        let path = CapabilityRelativePath::parse("new/nested/file.txt").expect("relative path");
        let mut created = Vec::new();

        tools
            .ensure_parent_directories(&path, &mut created)
            .expect("create parents");
        assert!(workspace.join("new/nested").is_dir());
        tools
            .remove_created_directories(&mut created)
            .expect("remove created parents");

        assert!(!workspace.join("new").exists());
        remove_workspace(&workspace);
    }

    #[cfg(unix)]
    #[test]
    fn atomic_patch_preserves_existing_unix_file_mode() {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

        let workspace = test_workspace("unix-mode");
        let target = workspace.join("script.sh");
        std::fs::write(&target, "#!/bin/sh\necho before\n").expect("write script");
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o751))
            .expect("set executable mode");
        let tools = WorkspaceTools::new(&workspace).expect("bind workspace tools");

        tools
            .patch(
                WorkspacePatch {
                    changes: vec![WorkspacePatchChange {
                        path: "script.sh".to_string(),
                        expected: Some("before".to_string()),
                        replacement: "after".to_string(),
                    }],
                },
                &ToolBrokerDecision::Allow,
            )
            .expect("patch script");

        assert_eq!(std::fs::metadata(&target).unwrap().mode() & 0o777, 0o751);
        remove_workspace(&workspace);
    }
}

#[cfg(test)]
mod registry_contract_tests {
    use super::*;

    fn accept_input(_: &Value) -> Result<(), ToolInputValidationError> {
        Ok(())
    }

    #[test]
    fn agent_control_binding_rejects_inconsistent_authorization() {
        let mut entry = ToolEntry::model(
            ToolSpec::new(
                "update_plan",
                "Update the plan",
                json!({"type": "object"}),
                ToolExecutionMode::Exclusive,
                accept_input,
            ),
            1,
            ToolCapability::PlanManagement,
            ToolAuthorization::AgentControl,
            ToolExecutor::AgentControl(AgentControlToolExecutor::UpdatePlan),
        )
        .expect("valid agent control entry");
        entry.authorization = ToolAuthorization::WorkspaceRead;

        let name = entry.id.as_str().to_string();
        let mut registry = ToolRegistry::default();
        registry.tools.insert(name.clone(), entry);
        let broker = ToolBroker::new(registry);

        let result = broker.bind_authorization(
            &name,
            json!({"steps": []}),
            None,
            SandboxFilesystemMode::ReadOnly,
            SandboxNetworkMode::Denied,
        );

        assert!(matches!(result, Err(WorkspaceToolError::InvalidInput(_))));

        let envelope = ToolCallRequest::new(&name, &name, "{}");
        let mut executor_called = false;
        let result = broker.execute(&envelope, ToolBrokerDecision::Allow, |_, _| {
            executor_called = true;
            ToolOutput::success(json!({"unexpected": true}))
        });
        assert!(!executor_called);
        assert_eq!(
            result.error_code.as_deref(),
            Some(TOOL_CONTRACT_INVALID_ERROR)
        );
    }
}
