#![forbid(unsafe_code)]

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub const DEFAULT_COMMAND_TIMEOUT_SECONDS: u64 = 30;
pub const DEFAULT_MAX_OUTPUT_CHARS: usize = 40_000;
const SANDBOX_BACKEND_UNAVAILABLE: &str = "sandbox-required command has no sandbox backend";
const SHELL_COMMAND_FLAGS: [&str; 3] = ["/c", "-c", "-command"];
const GIT_STATUS_ARGS: [&str; 3] = ["git", "status", "--porcelain=v1"];
const GIT_DIFF_ARGS: [&str; 3] = ["git", "diff", "--"];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SandboxProfileName {
    ReadonlyAnalysis,
    IsolatedVerification,
    GeneratedCode,
    PackageOperation,
    LongRunningService,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SandboxFilesystemMode {
    ReadOnlyWorkspace,
    CopyOnWriteWorkspace,
    EmptyTempWorkspace,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum SandboxNetworkMode {
    Denied,
    Allowed,
    Allowlist,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SandboxFilesystemPolicy {
    pub mode: SandboxFilesystemMode,
    pub workspace_root: String,
    pub writable_paths: Vec<String>,
    pub readonly_paths: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SandboxNetworkPolicy {
    pub mode: SandboxNetworkMode,
    pub allowed_hosts: Vec<String>,
    pub denied_hosts: Vec<String>,
    pub require_hard_isolation: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SandboxResourceLimits {
    pub timeout_seconds: u64,
    pub max_output_chars: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SandboxPolicy {
    pub profile: SandboxProfileName,
    pub filesystem: SandboxFilesystemPolicy,
    pub network: SandboxNetworkPolicy,
    pub resources: SandboxResourceLimits,
}

impl SandboxPolicy {
    pub fn isolated_verification(workspace_root: impl Into<String>) -> Self {
        Self {
            profile: SandboxProfileName::IsolatedVerification,
            filesystem: SandboxFilesystemPolicy {
                mode: SandboxFilesystemMode::CopyOnWriteWorkspace,
                workspace_root: workspace_root.into(),
                writable_paths: Vec::new(),
                readonly_paths: Vec::new(),
            },
            network: SandboxNetworkPolicy {
                mode: SandboxNetworkMode::Denied,
                allowed_hosts: Vec::new(),
                denied_hosts: Vec::new(),
                require_hard_isolation: false,
            },
            resources: SandboxResourceLimits {
                timeout_seconds: DEFAULT_COMMAND_TIMEOUT_SECONDS,
                max_output_chars: DEFAULT_MAX_OUTPUT_CHARS,
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CommandPurpose {
    ReadOnlyCommand,
    ProjectVerification,
    Build,
    PackageManager,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CommandExecutionStatus {
    Completed,
    PolicyDenied,
    ReviewRequired,
    SpawnFailed,
    TimedOut,
    BackendError,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CommandSemanticStatus {
    Succeeded,
    ExitNonzero,
    TestsFailed,
    BuildFailed,
    PolicyBlocked,
    TimedOut,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CommandRequest {
    pub command_id: String,
    pub argv: Vec<String>,
    pub cwd: String,
    pub purpose: CommandPurpose,
    pub timeout_seconds: u64,
    pub network: SandboxNetworkPolicy,
    pub filesystem: SandboxFilesystemPolicy,
}

impl CommandRequest {
    pub fn project_verification(
        command_id: impl Into<String>,
        argv: Vec<String>,
        cwd: impl Into<String>,
        workspace_root: impl Into<String>,
    ) -> Self {
        Self {
            command_id: command_id.into(),
            argv,
            cwd: cwd.into(),
            purpose: CommandPurpose::ProjectVerification,
            timeout_seconds: DEFAULT_COMMAND_TIMEOUT_SECONDS,
            network: SandboxNetworkPolicy {
                mode: SandboxNetworkMode::Denied,
                allowed_hosts: Vec::new(),
                denied_hosts: Vec::new(),
                require_hard_isolation: false,
            },
            filesystem: SandboxFilesystemPolicy {
                mode: SandboxFilesystemMode::ReadOnlyWorkspace,
                workspace_root: workspace_root.into(),
                writable_paths: Vec::new(),
                readonly_paths: Vec::new(),
            },
        }
    }

    pub fn requires_sandbox(&self) -> bool {
        true
    }

    pub fn permission_resource(&self) -> String {
        normalize_command_resource(&self.argv)
    }
}

pub fn git_status_request(
    command_id: impl Into<String>,
    cwd: impl Into<String>,
    workspace_root: impl Into<String>,
) -> CommandRequest {
    project_command_request(command_id, &GIT_STATUS_ARGS, cwd, workspace_root)
}

pub fn git_diff_request(
    command_id: impl Into<String>,
    cwd: impl Into<String>,
    workspace_root: impl Into<String>,
) -> CommandRequest {
    project_command_request(command_id, &GIT_DIFF_ARGS, cwd, workspace_root)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CommandResult {
    pub command_id: String,
    pub execution_status: CommandExecutionStatus,
    pub semantic_status: CommandSemanticStatus,
    pub exit_code: Option<i32>,
    pub duration_ms: u64,
    pub timed_out: bool,
    pub stdout_preview: String,
    pub stderr_preview: String,
    pub output_truncated: bool,
    pub redacted: bool,
    pub changed_files: Vec<String>,
}

impl CommandResult {
    pub fn completed(command_id: impl Into<String>, stdout_preview: impl Into<String>) -> Self {
        Self {
            command_id: command_id.into(),
            execution_status: CommandExecutionStatus::Completed,
            semantic_status: CommandSemanticStatus::Succeeded,
            exit_code: Some(0),
            duration_ms: 0,
            timed_out: false,
            stdout_preview: stdout_preview.into(),
            stderr_preview: String::new(),
            output_truncated: false,
            redacted: false,
            changed_files: Vec::new(),
        }
    }

    pub fn policy_denied(command_id: impl Into<String>, reason: impl Into<String>) -> Self {
        Self::blocked(
            command_id,
            CommandExecutionStatus::PolicyDenied,
            CommandSemanticStatus::PolicyBlocked,
            reason,
        )
    }

    pub fn sandbox_backend_unavailable(command_id: impl Into<String>) -> Self {
        Self::blocked(
            command_id,
            CommandExecutionStatus::BackendError,
            CommandSemanticStatus::PolicyBlocked,
            SANDBOX_BACKEND_UNAVAILABLE,
        )
    }

    fn blocked(
        command_id: impl Into<String>,
        execution_status: CommandExecutionStatus,
        semantic_status: CommandSemanticStatus,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            command_id: command_id.into(),
            execution_status,
            semantic_status,
            exit_code: None,
            duration_ms: 0,
            timed_out: false,
            stdout_preview: String::new(),
            stderr_preview: reason.into(),
            output_truncated: false,
            redacted: false,
            changed_files: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SandboxCapabilities {
    pub filesystem_isolation: bool,
    pub copy_on_write: bool,
    pub readonly_mount: bool,
    pub network_isolation: bool,
    pub env_isolation: bool,
    pub process_tree_kill: bool,
    pub timeout: bool,
    pub output_limit: bool,
    pub memory_limit: bool,
    pub process_limit: bool,
    pub artifact_capture: bool,
    pub change_detection: bool,
}

impl SandboxCapabilities {
    pub fn strict() -> Self {
        Self {
            filesystem_isolation: true,
            copy_on_write: true,
            readonly_mount: true,
            network_isolation: true,
            env_isolation: true,
            process_tree_kill: true,
            timeout: true,
            output_limit: true,
            memory_limit: false,
            process_limit: false,
            artifact_capture: true,
            change_detection: true,
        }
    }
}

pub trait SandboxBackend {
    fn name(&self) -> &'static str;
    fn capabilities(&self) -> SandboxCapabilities;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SandboxBackendEnforcement {
    Strict,
    Reduced,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SandboxBackendDescriptor {
    pub backend: String,
    pub enforcement: SandboxBackendEnforcement,
    pub capabilities: SandboxCapabilities,
}

impl SandboxBackendDescriptor {
    pub fn strict(backend: impl Into<String>) -> Self {
        Self {
            backend: backend.into(),
            enforcement: SandboxBackendEnforcement::Strict,
            capabilities: SandboxCapabilities::strict(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PatchChange {
    pub path: String,
    pub expected: Option<String>,
    pub replacement: String,
}

impl PatchChange {
    pub fn replace(
        path: impl Into<String>,
        expected: impl Into<String>,
        replacement: impl Into<String>,
    ) -> Self {
        Self {
            path: path.into(),
            expected: Some(expected.into()),
            replacement: replacement.into(),
        }
    }

    pub fn create(path: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            expected: None,
            replacement: content.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PatchResult {
    pub applied: bool,
    pub changed_files: Vec<String>,
    pub rolled_back: bool,
    pub error: Option<String>,
}

fn normalize_command_resource(argv: &[String]) -> String {
    if argv.is_empty() {
        return String::new();
    }
    let lower = argv
        .iter()
        .map(|part| part.replace('\\', "/").to_ascii_lowercase())
        .collect::<Vec<_>>();
    let first = lower[0].as_str();
    if matches!(
        first,
        "cmd" | "cmd.exe" | "sh" | "bash" | "powershell" | "powershell.exe" | "pwsh" | "pwsh.exe"
    ) && let Some(index) = lower
        .iter()
        .position(|part| SHELL_COMMAND_FLAGS.contains(&part.as_str()))
    {
        return lower[index + 1..].join(" ");
    }
    lower.join(" ")
}

fn project_command_request(
    command_id: impl Into<String>,
    args: &[&str],
    cwd: impl Into<String>,
    workspace_root: impl Into<String>,
) -> CommandRequest {
    CommandRequest::project_verification(
        command_id,
        args.iter().map(|part| part.to_string()).collect(),
        cwd,
        workspace_root,
    )
}
