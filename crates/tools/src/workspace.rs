//! 工作区 capability、读写搜索、命令接入及 verification 观察。

use super::*;
use singularity_sandbox::is_toolchain_artifact_path;

mod mutation;
mod read;
#[cfg(test)]
pub(crate) use mutation::{AtomicWriteFailure, PreparedMutation, PublishedMutation};

/// 工作区 tool 返回的工作区边界、受保护路径、沙箱和变更错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceToolError {
    OutsideWorkspace(String),
    ProtectedPath(String),
    SandboxUnavailable,
    ObservationSinkFailed,
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
            Self::ObservationSinkFailed => write!(formatter, "sandbox observation sink failed"),
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

fn artifact_summary_is_trusted(summary: &WorkspaceChangeSummary) -> bool {
    if summary.changed_files.is_empty() || !is_sha256_digest(&summary.diff_digest) {
        return false;
    }
    let mut paths = BTreeSet::new();
    summary
        .changed_files
        .iter()
        .all(|path| paths.insert(path) && is_toolchain_artifact_path(path))
}

fn is_sha256_digest(value: &str) -> bool {
    let Some(hex) = value.strip_prefix("sha256:") else {
        return false;
    };
    hex.len() == 64 && hex.bytes().all(|byte| byte.is_ascii_hexdigit())
}

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
    pub(crate) fn validate(&self) -> Result<(), WorkspaceToolError> {
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
    pub(crate) fn validate(&self) -> Result<(), WorkspaceToolError> {
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
    pub(crate) fn validate(&self) -> Result<(), WorkspaceToolError> {
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
    pub(crate) fn validate(&self) -> Result<(), WorkspaceToolError> {
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
    pub(crate) fn validate(&self) -> Result<(), WorkspaceToolError> {
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

    /// Return the bounded numeric revision for internal typed prompts.
    pub fn value(self) -> u64 {
        self.0
    }
}

/// 一次 tool 结果实际观察到的工作区 revision 与变化事实。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceObservation {
    revision: Option<WorkspaceRevision>,
    mutation: WorkspaceMutation,
}

/// 实际进入 `SandboxBackend` 的一次 command 执行的安全终态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SandboxExecutionStatus {
    Ok,
    Error,
    TimedOut,
    Cancelled,
}

/// `WorkspaceTools` 在 backend 返回点形成的短生命周期 typed observation。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SandboxExecutionObservation {
    pub command_id: String,
    pub command_id_binding_valid: bool,
    pub started_at_unix_ms: u64,
    pub ended_at_unix_ms: u64,
    pub duration_ms: u64,
    pub status: SandboxExecutionStatus,
    pub workspace_mutation: WorkspaceMutation,
    pub enforcement: SandboxBackendEnforcement,
}

/// backend 调用前后的真实短生命周期边界事件。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum SandboxExecutionBoundary {
    Started {
        command_id: String,
        started_at_unix_ms: u64,
    },
    Finished(SandboxExecutionObservation),
}

/// sandbox event sink 的不透明拒绝信号。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SandboxExecutionSinkError;

/// 单次 command 调用期间同步消费 sandbox 边界事件的 callback。
pub type SandboxExecutionCallback<'a> =
    dyn FnMut(SandboxExecutionBoundary) -> Result<(), SandboxExecutionSinkError> + 'a;

/// command tool 输出与其同一次真实 backend occurrence 的 typed 绑定。
#[derive(Debug, Clone, PartialEq)]
pub struct CommandToolExecution {
    pub output: ToolOutput,
    pub sandbox_execution: SandboxExecutionObservation,
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
    command_runtime_executables: Vec<String>,
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
            command_runtime_executables: Vec::new(),
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

    /// 绑定由产品控制面声明、不会进入模型输入的 command runtime executable。
    pub fn with_command_runtime_executables(mut self, executables: Vec<String>) -> Self {
        self.command_runtime_executables = executables;
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
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |revision| {
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
    pub(crate) fn bind_tool_call(
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
        self.command_cancellable_with_policy_observed(
            input,
            filesystem,
            network,
            expected_scope,
            cancellation,
        )
        .map(|execution| execution.output)
    }

    /// 按已绑定范围执行 command，并返回与真实 backend 调用同源的 typed observation。
    pub fn command_cancellable_with_policy_observed(
        &self,
        input: CommandToolInput,
        filesystem: SandboxFilesystemMode,
        network: SandboxNetworkMode,
        expected_scope: &CommandScopeDigest,
        cancellation: &CancellationToken,
    ) -> Result<CommandToolExecution, WorkspaceToolError> {
        self.command_cancellable_with_policy_events(
            input,
            filesystem,
            network,
            expected_scope,
            cancellation,
            &mut |_| Ok(()),
        )
    }

    /// 执行 command，并在 backend 调用前后同步投影真实边界事件。
    pub fn command_cancellable_with_policy_events(
        &self,
        input: CommandToolInput,
        filesystem: SandboxFilesystemMode,
        network: SandboxNetworkMode,
        expected_scope: &CommandScopeDigest,
        cancellation: &CancellationToken,
        on_event: &mut SandboxExecutionCallback<'_>,
    ) -> Result<CommandToolExecution, WorkspaceToolError> {
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
        request.runtime_executables = self.command_runtime_executables.clone();
        if let Some(timeout_seconds) = input.timeout_seconds {
            request.timeout_seconds = timeout_seconds;
        }
        let started_at_unix_ms = command_boundary_unix_ms();
        on_event(SandboxExecutionBoundary::Started {
            command_id: request.command_id.clone(),
            started_at_unix_ms,
        })
        .map_err(|_| WorkspaceToolError::ObservationSinkFailed)?;
        let backend_started = std::time::Instant::now();
        let result = backend.execute_script_cancellable(&request, cancellation);
        let measured_duration_ms =
            u64::try_from(backend_started.elapsed().as_millis()).unwrap_or(u64::MAX);
        drop(bound_command_cwd);
        let mutation = result.workspace_mutation;
        let workspace_change_summary = result.workspace_change_summary.clone();
        let execution = result.sandbox.clone();
        let command_id_binding_valid = result.command_id == request.command_id;
        let sandbox_execution = SandboxExecutionObservation {
            command_id: request.command_id.clone(),
            command_id_binding_valid,
            started_at_unix_ms,
            ended_at_unix_ms: command_boundary_unix_ms(),
            duration_ms: result.duration_ms.max(measured_duration_ms),
            status: if !command_id_binding_valid {
                SandboxExecutionStatus::Error
            } else {
                match &result.execution_status {
                    CommandExecutionStatus::Cancelled => SandboxExecutionStatus::Cancelled,
                    CommandExecutionStatus::TimedOut => SandboxExecutionStatus::TimedOut,
                    CommandExecutionStatus::Completed
                        if result.semantic_status == CommandSemanticStatus::Succeeded =>
                    {
                        SandboxExecutionStatus::Ok
                    }
                    CommandExecutionStatus::Completed
                    | CommandExecutionStatus::PolicyDenied
                    | CommandExecutionStatus::ReviewRequired
                    | CommandExecutionStatus::Unsupported
                    | CommandExecutionStatus::ExecutableUnavailable
                    | CommandExecutionStatus::BackendError => SandboxExecutionStatus::Error,
                }
            },
            workspace_mutation: mutation,
            enforcement: execution.enforcement.clone(),
        };
        let mut output = command_tool_output(result);
        if !sandbox_execution.command_id_binding_valid {
            output.ok = false;
            output.failure_kind = Some(ToolFailureKind::Infrastructure);
            output.error_code = Some("sandbox_command_id_mismatch".to_string());
        }
        // A producer may mark a physical diff as verification-irrelevant only for a fully
        // classified, newly-created toolchain artifact set with a bound digest. Missing or
        // malformed summaries, and a false flag attached to an unknown path, remain relevant.
        let verification_relevant = workspace_change_summary.as_ref().is_none_or(|summary| {
            summary.verification_relevant || !artifact_summary_is_trusted(summary)
        });
        let observation = match (&requested_filesystem, mutation, verification_relevant) {
            (SandboxFilesystemMode::WorkspaceWrite, WorkspaceMutation::Unchanged, _) => {
                WorkspaceObservation::unchanged(self.current_workspace_revision())
            }
            (SandboxFilesystemMode::WorkspaceWrite, WorkspaceMutation::Changed, false) => {
                WorkspaceObservation::unchanged(self.current_workspace_revision())
            }
            (SandboxFilesystemMode::WorkspaceWrite, WorkspaceMutation::Changed, true) => {
                WorkspaceObservation::changed(self.advance_workspace_revision()?)
            }
            (SandboxFilesystemMode::WorkspaceWrite, WorkspaceMutation::Unknown, _) => {
                output.ok = false;
                output.failure_kind = Some(ToolFailureKind::Backend);
                output.error_code = Some("workspace_change_unknown".to_string());
                WorkspaceObservation::unknown()
            }
            (SandboxFilesystemMode::ReadOnly, WorkspaceMutation::Changed, _) => {
                output.ok = false;
                output.failure_kind = Some(ToolFailureKind::Backend);
                output.error_code = Some("workspace_changed_in_read_only_command".to_string());
                WorkspaceObservation::changed(self.advance_workspace_revision()?)
            }
            (
                SandboxFilesystemMode::ReadOnly,
                WorkspaceMutation::Unchanged | WorkspaceMutation::Unknown,
                _,
            ) => WorkspaceObservation::unchanged(self.current_workspace_revision()),
        };
        Self::attach_workspace_observation(&mut output, &observation)?;
        if let Some(summary) = workspace_change_summary {
            output.metadata[WORKSPACE_CHANGE_SUMMARY_METADATA] = json!(summary);
        }
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
        on_event(SandboxExecutionBoundary::Finished(
            sandbox_execution.clone(),
        ))
        .map_err(|_| WorkspaceToolError::ObservationSinkFailed)?;
        Ok(CommandToolExecution {
            output,
            sandbox_execution,
        })
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
            // Bind the ambient cwd to the same object so the platform adapter cannot silently
            // bypass the workspace capability boundary.
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
            let _ = directory;
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
            let _ = file;
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

    pub(crate) fn existing_text_or_empty(
        &self,
        path: &CapabilityRelativePath,
    ) -> Result<(String, Option<WorkspaceContentRevision>), WorkspaceToolError> {
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
        let (bytes, revision) = self
            .read_file_bytes_with_revision(&path.display, &mut file)
            .map_err(|error| map_capability_error(error, &path.display))?;
        let current_revision = self.current_file_content_revision(path)?;
        drop(file);
        if current_revision.as_ref() != Some(&revision) {
            return Err(WorkspaceToolError::ConcurrentMutation(path.display.clone()));
        }
        let content = String::from_utf8(bytes).map_err(|error| {
            WorkspaceToolError::ReadFailed(format!("workspace file is not valid utf-8: {error}"))
        })?;
        Ok((content, Some(revision)))
    }

    fn file_revision_metadata(
        &self,
        relative: &str,
        file: &CapabilityFile,
    ) -> Result<WorkspaceContentRevision, CapabilityAccessError> {
        let metadata = file.metadata().map_err(classify_io_error)?;
        if metadata_is_symlink_or_reparse(&metadata) {
            return Err(CapabilityAccessError::Unsafe);
        }
        if !metadata.is_file() {
            return Err(CapabilityAccessError::NotRegularFile);
        }
        let object_identity = file_object_identity_key(file)?;
        let stable_metadata = file_content_revision_metadata_key(file)?;
        Ok(WorkspaceContentRevision::metadata_only(
            relative,
            "regular",
            object_identity,
            stable_metadata,
        ))
    }

    fn read_file_bytes_with_revision(
        &self,
        relative: &str,
        file: &mut CapabilityFile,
    ) -> Result<(Vec<u8>, WorkspaceContentRevision), CapabilityAccessError> {
        self.read_file_bytes_with_revision_and_hook(relative, file, || {})
    }

    fn read_file_bytes_with_revision_and_hook(
        &self,
        relative: &str,
        file: &mut CapabilityFile,
        after_metadata: impl FnOnce(),
    ) -> Result<(Vec<u8>, WorkspaceContentRevision), CapabilityAccessError> {
        let pre_metadata = self.file_revision_metadata(relative, file)?;
        after_metadata();
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .map_err(CapabilityAccessError::Io)?;
        let post_metadata = self.file_revision_metadata(relative, file)?;
        if !pre_metadata.same_metadata(&post_metadata) {
            return Err(CapabilityAccessError::ConcurrentMutation);
        }
        let digest = format!("sha256:{:x}", Sha256::digest(&bytes));
        Ok((bytes, pre_metadata.with_digest(digest)))
    }

    #[cfg(test)]
    pub(crate) fn read_file_with_revision_after_metadata_hook_for_test(
        &self,
        path: &CapabilityRelativePath,
        after_metadata: impl FnOnce(),
    ) -> Result<(), WorkspaceToolError> {
        let mut file = self
            .open_existing_file(path)
            .map_err(|error| map_capability_error(error, &path.display))?
            .ok_or_else(|| {
                WorkspaceToolError::ReadFailed("workspace path is unavailable".to_string())
            })?;
        self.read_file_bytes_with_revision_and_hook(&path.display, &mut file, after_metadata)
            .map(|_| ())
            .map_err(|error| map_capability_error(error, &path.display))
    }

    fn current_file_content_revision(
        &self,
        path: &CapabilityRelativePath,
    ) -> Result<Option<WorkspaceContentRevision>, WorkspaceToolError> {
        let Some(mut file) = self
            .open_existing_file(path)
            .map_err(|error| map_capability_error(error, &path.display))?
        else {
            return Ok(None);
        };
        self.read_file_bytes_with_revision(&path.display, &mut file)
            .map(|(_, revision)| Some(revision))
            .map_err(|error| map_capability_error(error, &path.display))
    }

    fn current_file_content_revision_with_cancellation(
        &self,
        path: &CapabilityRelativePath,
        cancellation: &dyn Fn() -> bool,
    ) -> Result<Option<WorkspaceContentRevision>, WorkspaceToolError> {
        let Some(mut file) = self
            .open_existing_file(path)
            .map_err(|error| map_capability_error(error, &path.display))?
        else {
            return Ok(None);
        };
        let initial_metadata = self
            .file_revision_metadata(&path.display, &file)
            .map_err(|error| map_capability_error(error, &path.display))?;
        let mut hasher = Sha256::new();
        let mut chunk = [0u8; FILE_READ_CHUNK_SIZE];
        loop {
            check_cancelled(cancellation)?;
            let bytes_read = file.read(&mut chunk).map_err(io_error)?;
            check_cancelled(cancellation)?;
            if bytes_read == 0 {
                break;
            }
            hasher.update(&chunk[..bytes_read]);
        }
        let final_metadata = self
            .file_revision_metadata(&path.display, &file)
            .map_err(|error| map_capability_error(error, &path.display))?;
        if !initial_metadata.same_metadata(&final_metadata) {
            return Err(WorkspaceToolError::ConcurrentMutation(path.display.clone()));
        }
        Ok(Some(
            initial_metadata.with_digest(format!("sha256:{:x}", hasher.finalize())),
        ))
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

fn command_boundary_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
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
    #[cfg(windows)]
    let (anchor, components) = workspace_anchor_and_components_with_policy(workspace_root, true)?;
    #[cfg(not(windows))]
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
    let (anchor, components) = workspace_anchor_and_components_with_policy(workspace_root, true)?;
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
    let actual_path = singularity_sandbox::expand_windows_path_alias(&actual_path);
    // A short-name spelling is accepted only while opening the existing root. The
    // handle-derived path must be the normalized long spelling before it becomes
    // the workspace display/root path; re-run the strict component checks on it.
    workspace_anchor_and_components(&actual_path)?;
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
    workspace_anchor_and_components_with_policy(workspace_root, false)
}

#[cfg(windows)]
fn workspace_anchor_and_components_with_policy(
    workspace_root: &Path,
    allow_short_name_alias: bool,
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
                validate_windows_component(name, !allow_short_name_alias)?;
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
struct BoundCommandCwd {
    path: PathBuf,
    _capability_guard: CapabilityDir,
    #[cfg(windows)]
    _namespace_guard: std::fs::File,
    #[cfg(not(windows))]
    _namespace_guard: CapabilityDir,
}

/// 已验证的工作区相对路径；其 `relative`、显示值和重复键来自同一次解析。
#[derive(Debug, Clone)]
pub(crate) struct CapabilityRelativePath {
    relative: PathBuf,
    pub(crate) display: String,
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

    pub(crate) fn parse(path: &str) -> Result<Self, WorkspaceToolError> {
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
            validate_windows_component(name, true)?;
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

/// 工作区文件在一次安全读取/写入保护中的内容 revision。
///
/// 该值同时绑定路径、文件类型、对象身份、稳定元数据和正文摘要；任何一个
/// 组成部分变化都必须使依赖旧 revision 的原子写入失败。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorkspaceContentRevision {
    pub(crate) relative: String,
    pub(crate) file_type: String,
    pub(crate) object_identity: String,
    pub(crate) stable_metadata: String,
    pub(crate) content_digest: String,
}

impl WorkspaceContentRevision {
    fn metadata_only(
        relative: &str,
        file_type: impl Into<String>,
        object_identity: impl Into<String>,
        stable_metadata: impl Into<String>,
    ) -> Self {
        Self {
            relative: relative.to_string(),
            file_type: file_type.into(),
            object_identity: object_identity.into(),
            stable_metadata: stable_metadata.into(),
            content_digest: String::new(),
        }
    }

    fn with_digest(mut self, content_digest: String) -> Self {
        self.content_digest = content_digest;
        self
    }

    fn same_metadata(&self, other: &Self) -> bool {
        self.relative == other.relative
            && self.file_type == other.file_type
            && self.object_identity == other.object_identity
            && self.stable_metadata == other.stable_metadata
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
    ConcurrentMutation,
    NotDirectory,
    NotRegularFile,
    HardLinked,
    Unsupported,
    Io(std::io::Error),
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
fn validate_windows_component(
    name: &OsStr,
    reject_short_name_alias: bool,
) -> Result<(), WorkspaceToolError> {
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
        || (reject_short_name_alias && short_name_alias)
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
        CapabilityAccessError::ConcurrentMutation => {
            WorkspaceToolError::ConcurrentMutation(relative.to_string())
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
        // Read access lets failure cleanup derive its ownership revision from
        // this pinned handle instead of trusting the current directory entry.
        match parent.open_with(&temp_name, &nofollow_file_options(true, true, true)) {
            Ok(file) => return Ok((temp_name, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(classify_io_error(error)),
        }
    }
    Err(CapabilityAccessError::Io(std::io::Error::other(
        "failed to allocate workspace temporary file",
    )))
}

/// Move a directory entry without replacing an entry which appeared at the
/// destination.  Unix mutation cleanup uses this primitive as the first step
/// of its quarantine protocol; the operation is relative to an already-open
/// capability directory and therefore never reconstructs an ambient path.
#[cfg(unix)]
fn mutation_rename_noreplace(
    parent: &CapabilityDir,
    source: &OsStr,
    destination: &OsStr,
) -> Result<(), CapabilityAccessError> {
    #[cfg(target_os = "linux")]
    {
        rustix::fs::renameat_with(
            parent,
            source,
            parent,
            destination,
            rustix::fs::RenameFlags::NOREPLACE,
        )
        .map_err(classify_mutation_primitive_error)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (parent, source, destination);
        Err(CapabilityAccessError::Unsupported)
    }
}

/// Atomically exchange two entries in one capability directory.
#[cfg(unix)]
fn mutation_rename_exchange(
    parent: &CapabilityDir,
    source: &OsStr,
    destination: &OsStr,
) -> Result<(), CapabilityAccessError> {
    #[cfg(target_os = "linux")]
    {
        rustix::fs::renameat_with(
            parent,
            source,
            parent,
            destination,
            rustix::fs::RenameFlags::EXCHANGE,
        )
        .map_err(classify_mutation_primitive_error)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (parent, source, destination);
        Err(CapabilityAccessError::Unsupported)
    }
}

/// Remove a quarantined regular file by its capability directory and name.
#[cfg(unix)]
fn mutation_unlink_file(parent: &CapabilityDir, name: &OsStr) -> Result<(), CapabilityAccessError> {
    rustix::fs::unlinkat(parent, name, rustix::fs::AtFlags::empty())
        .map_err(classify_mutation_primitive_error)
}

/// Remove a quarantined empty directory by its capability directory and name.
#[cfg(unix)]
fn mutation_unlink_directory(
    parent: &CapabilityDir,
    name: &OsStr,
) -> Result<(), CapabilityAccessError> {
    rustix::fs::unlinkat(parent, name, rustix::fs::AtFlags::REMOVEDIR)
        .map_err(classify_mutation_primitive_error)
}

/// Allocate an unguessable quarantine name.  The name check is only a
/// collision hint; `RENAME_NOREPLACE` remains the authoritative collision
/// guard, so a concurrent creator cannot be overwritten.
#[cfg(unix)]
fn mutation_quarantine_name(
    parent: &CapabilityDir,
    kind: &str,
) -> Result<OsString, CapabilityAccessError> {
    for _ in 0..MUTATION_TEMP_FILE_ATTEMPTS {
        let name = OsString::from(format!(
            ".singularity-quarantine-{kind}-{}",
            uuid::Uuid::new_v4().simple()
        ));
        match parent.symlink_metadata(&name) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(name),
            Ok(_) => continue,
            Err(error) => return Err(classify_io_error(error)),
        }
    }
    Err(CapabilityAccessError::Io(std::io::Error::other(
        "failed to allocate workspace quarantine name",
    )))
}

/// Restore a quarantined entry without overwriting anything which appeared at
/// its original name while ownership was being checked.
#[cfg(unix)]
fn restore_quarantined_entry(
    parent: &CapabilityDir,
    quarantine: &OsStr,
    original: &OsStr,
    context: &str,
) -> Result<(), WorkspaceToolError> {
    mutation_rename_noreplace(parent, quarantine, original).map_err(|error| {
        WorkspaceToolError::RollbackFailed(format!(
            "{context}: quarantined entry restoration failed: {}",
            map_capability_error(error, context)
        ))
    })
}

#[cfg(unix)]
fn classify_mutation_primitive_error(error: rustix::io::Errno) -> CapabilityAccessError {
    // These calls always use a valid, fixed flag set and one already-open
    // parent directory. Linux filesystems which do not implement renameat2
    // flags may report EINVAL instead of EOPNOTSUPP.
    if matches!(
        error,
        rustix::io::Errno::INVAL | rustix::io::Errno::NOSYS | rustix::io::Errno::NOTSUP
    ) {
        CapabilityAccessError::Unsupported
    } else {
        classify_io_error(std::io::Error::from(error))
    }
}

#[cfg(all(test, unix))]
#[test]
fn renameat2_einval_is_an_explicit_capability_blocker() {
    assert!(matches!(
        classify_mutation_primitive_error(rustix::io::Errno::INVAL),
        CapabilityAccessError::Unsupported
    ));
}

#[cfg(not(unix))]
fn cleanup_owned_file(
    parent: &CapabilityDir,
    name: &OsStr,
    expected_identity: &str,
    failure: WorkspaceToolError,
) -> WorkspaceToolError {
    cleanup_owned_file_non_unix(parent, name, expected_identity, failure)
}

#[cfg(not(unix))]
fn cleanup_owned_file_non_unix(
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

#[cfg(unix)]
fn file_content_revision_metadata_key(
    file: &CapabilityFile,
) -> Result<String, CapabilityAccessError> {
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
    Ok(format!(
        "content-state:{:x}:{:x}:{:x}:{:x}:{:x}:{:x}:{:x}",
        metadata.dev(),
        metadata.ino(),
        metadata.mtime(),
        metadata.mtime_nsec(),
        metadata.ctime(),
        metadata.ctime_nsec(),
        metadata.len()
    ))
}

#[cfg(windows)]
fn file_content_revision_metadata_key(
    file: &CapabilityFile,
) -> Result<String, CapabilityAccessError> {
    let standard_file = file
        .try_clone()
        .map_err(CapabilityAccessError::Io)?
        .into_std();
    let metadata = standard_file
        .metadata()
        .map_err(CapabilityAccessError::Io)?;
    let modified = metadata
        .modified()
        .map_err(|_| CapabilityAccessError::Unsupported)?
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| CapabilityAccessError::Unsupported)?;
    Ok(format!(
        "content-state:{}:{:x}:{:x}:{:x}",
        file_object_identity_key(file)?,
        metadata.len(),
        modified.as_secs(),
        modified.subsec_nanos()
    ))
}

#[cfg(not(any(unix, windows)))]
fn file_content_revision_metadata_key(
    file: &CapabilityFile,
) -> Result<String, CapabilityAccessError> {
    file_object_identity_key(file)
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

#[cfg(all(test, windows))]
mod windows_root_binding_tests {
    use super::*;

    #[test]
    fn short_name_aliases_are_root_only_and_relative_paths_stay_strict() {
        let alias_root = Path::new(r"C:\Users\RUNNER~1\workspace");
        assert!(workspace_anchor_and_components(alias_root).is_err());
        assert!(workspace_anchor_and_components_with_policy(alias_root, true).is_ok());
        assert!(CapabilityRelativePath::parse("RUNNER~1/file.txt").is_err());
    }
}
