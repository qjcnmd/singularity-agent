//! 工具定义、schema、能力/授权契约与唯一注册表。

use super::*;

pub(crate) const REDACTED_TOOL_OUTPUT: &str = "[redacted sensitive tool output]";
pub(crate) const UNKNOWN_TOOL_ERROR: &str = "unknown_tool";
pub(crate) const INVALID_TOOL_ARGUMENTS_ERROR: &str = "invalid_tool_arguments";
pub(crate) const TOOL_DENIED_ERROR: &str = "tool_denied";
pub(crate) const TOOL_APPROVAL_REQUIRED_ERROR: &str = "approval_required";
pub(crate) const TOOL_SANDBOX_UNAVAILABLE_ERROR: &str = "sandbox_unavailable";
pub(crate) const TOOL_CONTRACT_INVALID_ERROR: &str = "tool_contract_invalid";
pub(crate) const WORKSPACE_MUTATION_NOT_APPROVED: &str =
    "workspace mutation requires allowed tool decision";
pub(crate) const WORKSPACE_OBSERVATION_METADATA: &str = "workspace_observation";
pub(crate) const WORKSPACE_CHANGE_SUMMARY_METADATA: &str = "workspace_change_summary";
pub(crate) const DUPLICATE_PATCH_TARGET: &str = "patch contains duplicate canonical target";
pub(crate) const MUTATION_TEMP_FILE_ATTEMPTS: usize = 64;
pub(crate) const DEFAULT_READ_MAX_CHARS: usize = 8_192;
pub(crate) const MAX_READ_MAX_CHARS: usize = 1_000_000;
pub(crate) const DEFAULT_LIST_MAX_ENTRIES: usize = 200;
pub(crate) const MAX_LIST_MAX_ENTRIES: usize = 10_000;
pub(crate) const DEFAULT_LIST_MAX_DEPTH: usize = 16;
pub(crate) const MAX_LIST_MAX_DEPTH: usize = 64;
pub(crate) const DEFAULT_GREP_MAX_MATCHES: usize = 200;
pub(crate) const MAX_GREP_MAX_MATCHES: usize = 10_000;
pub(crate) const MAX_COMMAND_TIMEOUT_SECONDS: u64 = 3_600;
pub(crate) const MAX_COMMAND_SCRIPT_CHARS: usize = 8_000;
pub(crate) const DEFAULT_RESULT_PREVIEW_MAX_CHARS: usize = 4_096;
pub(crate) const APPROXIMATE_ASCII_CHARS_PER_TOKEN: usize = 4;
pub(crate) const FILE_READ_CHUNK_SIZE: usize = 8 * 1024;
pub(crate) const BINARY_CONTENT_PREVIEW: &str = "[binary content omitted]";
pub(crate) const ARTIFACT_REFERENCE_OMITTED: &str = "[artifact reference omitted]";
pub(crate) const TRUNCATED_OUTPUT_OMITTED: &str = "[truncated output omitted]";
pub(crate) const MAX_TRUNCATED_SUMMARY_STRING_CHARS: usize = 512;
pub(crate) const TRUNCATED_RAW_OUTPUT_KEYS: [&str; 14] = [
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
pub(crate) const PROMPT_INJECTION_MARKERS: [&str; 4] = [
    "developer message",
    "ignore previous",
    "reveal hidden",
    "system prompt",
];
#[cfg(windows)]
pub(crate) const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
#[cfg(windows)]
pub(crate) const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
#[cfg(windows)]
pub(crate) const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
#[cfg(windows)]
pub(crate) const FILE_SHARE_READ: u32 = 0x0000_0001;
#[cfg(windows)]
pub(crate) const FILE_SHARE_WRITE: u32 = 0x0000_0002;
#[cfg(unix)]
pub(crate) const ERROR_TOO_MANY_SYMLINKS: i32 = 40;
#[cfg(windows)]
pub(crate) const ERROR_STOPPED_ON_SYMLINK: i32 = 681;
/// 核心 read tool 名称。
pub const READ_TOOL: &str = "read";
/// 核心 list tool 名称。
pub const LIST_TOOL: &str = "list";
/// 核心 grep tool 名称。
pub const GREP_TOOL: &str = "grep";
/// 核心 patch tool 名称。
pub const PATCH_TOOL: &str = "patch";
/// 核心 command tool 名称。
pub const COMMAND_TOOL: &str = "command";
pub(crate) static COMMAND_ID_COUNTER: AtomicU64 = AtomicU64::new(0);
pub(crate) static MUTATION_TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

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
}

/// 工作区执行器的类型化入口；名称到执行器的绑定只存在于注册表。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceToolExecutor {
    Read,
    List,
    Grep,
    Patch,
    Command,
}

/// 注册表绑定的真实执行器。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", content = "executor", rename_all = "snake_case")]
pub enum ToolExecutor {
    Workspace(WorkspaceToolExecutor),
}

/// 注册表绑定的授权投影合同。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ToolAuthorization {
    WorkspaceRead,
    WorkspaceWrite,
    Command,
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
            Self::Workspace(WorkspaceToolExecutor::Patch) => ToolExecutorContract {
                capability: ToolCapability::WorkspaceWrite,
                authorization: ToolAuthorization::WorkspaceWrite,
                execution_mode: ToolExecutionMode::Exclusive,
            },
            Self::Workspace(WorkspaceToolExecutor::Command) => ToolExecutorContract {
                capability: ToolCapability::CommandExecution,
                authorization: ToolAuthorization::Command,
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

/// 面向模型提供方的 schema，以及模型和执行边界共享的输入校验逻辑。
#[derive(Clone)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub execution_mode: ToolExecutionMode,
    input_validator: ToolInputValidator,
}

impl fmt::Debug for ToolSpec {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ToolSpec")
            .field("name", &self.name)
            .field("description", &self.description)
            .field("input_schema", &self.input_schema)
            .field("execution_mode", &self.execution_mode)
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
            input_validator,
        }
    }

    /// 校验并投影模型提交的输入。
    pub fn prepare_model_input(&self, input: &Value) -> Result<Value, ToolInputValidationError> {
        (self.input_validator)(input)?;
        Ok(input.clone())
    }

    /// 在执行器闸门重新校验可执行输入。
    pub fn validate_execution_input(&self, input: &Value) -> Result<(), ToolInputValidationError> {
        (self.input_validator)(input)?;
        Ok(())
    }

    pub fn to_schema_payload(&self) -> Value {
        json!({
            "name": self.name,
            "description": redact_public_text(&self.description),
            "input_schema": self.input_schema,
        })
    }
}

/// 单一工具事实源：定义、能力、授权投影和执行器绑定。
#[derive(Debug, Clone)]
pub struct ToolEntry {
    pub id: ToolId,
    pub version: u32,
    pub capability: ToolCapability,
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
    pub(crate) sensitive_resources: BTreeSet<PermissionResource>,
}

impl BoundToolCall {
    /// 判断某个资源是否在工作区边界被标记为受保护资源。
    pub fn resource_is_sensitive(&self, resource: &PermissionResource) -> bool {
        self.sensitive_resources.contains(resource)
    }
}

impl ToolEntry {
    /// 创建一个拥有真实执行器的版本化模型工具条目。
    pub fn new(
        spec: ToolSpec,
        version: u32,
        capability: ToolCapability,
        authorization: ToolAuthorization,
        executor: ToolExecutor,
    ) -> Result<Self, String> {
        let id = ToolId::new(spec.name.clone())?;
        let entry = Self {
            id,
            version,
            capability,
            authorization,
            executor,
            spec,
        };
        entry.validate_consistency()?;
        Ok(entry)
    }

    pub(crate) fn validate_consistency(&self) -> Result<(), String> {
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

fn validate_command_tool_input(input: &Value) -> Result<(), ToolInputValidationError> {
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
                validate_command_tool_input,
            ),
            ToolCapability::CommandExecution,
            ToolAuthorization::Command,
            WorkspaceToolExecutor::Command,
        ),
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
    ToolEntry::new(
        spec,
        1,
        capability,
        authorization,
        ToolExecutor::Workspace(executor),
    )
    .expect("built-in workspace tool entry is valid")
}

/// 负责管理能力、授权投影和执行器绑定的唯一工具注册表。
#[derive(Debug, Default, Clone)]
pub struct ToolRegistry {
    pub(crate) tools: BTreeMap<String, ToolEntry>,
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

    /// 返回全部注册工具的模型 schema payload。
    pub fn schema_payloads(&self) -> Vec<Value> {
        self.tools
            .values()
            .map(|entry| entry.spec.to_schema_payload())
            .collect::<Vec<_>>()
    }

    /// 校验并准备指定 tool 的模型输入。
    pub fn prepare_model_input(
        &self,
        name: &str,
        input: &Value,
    ) -> Result<(ToolExecutionMode, Value), ToolInputValidationError> {
        let entry = self
            .entry(name)
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
