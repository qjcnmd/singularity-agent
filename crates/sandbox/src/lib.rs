#![deny(unsafe_op_in_unsafe_fn)]

//! 严格沙箱执行所需的跨平台命令请求与结果契约。
//!
//! 平台适配器实现 `SandboxBackend` 边界；本模块负责可移植的权限模式、能力检查、
//! 取消语义和安全失败结果映射。

use std::path::Path;
#[cfg(windows)]
use std::path::{Component, PathBuf};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
#[cfg(windows)]
use singularity_core::is_protected_path;
use singularity_core::{CancellationToken, contains_sensitive_text};

mod workspace_change;
use workspace_change::{WorkspaceSnapshot, snapshot_workspace};

/// command tool 未指定超时时使用的秒数。
pub const DEFAULT_COMMAND_TIMEOUT_SECONDS: u64 = 30;
/// 命令公开输出的默认字符上限。
pub const DEFAULT_MAX_OUTPUT_CHARS: usize = 40_000;
const SANDBOX_BACKEND_UNAVAILABLE: &str = "sandbox-required command has no sandbox backend";
const COMMAND_EXECUTABLE_UNAVAILABLE: &str = "sandbox command executable unavailable";
const COMMAND_TIMED_OUT: &str = "sandbox command timed out";
const COMMAND_CANCELLED: &str = "sandbox command cancelled";
const COMMAND_SCRIPT_UNSUPPORTED: &str = "sandbox command script is unsupported on this platform";
#[cfg(windows)]
const COMMAND_EMPTY_ARGV: &str = "sandbox command argv is empty";
#[cfg(windows)]
const COMMAND_EMPTY_SCRIPT: &str = "sandbox command script is empty";
#[cfg(windows)]
const COMMAND_CWD_OUTSIDE_WORKSPACE: &str = "sandbox command cwd is outside workspace";
#[cfg(windows)]
const COMMAND_CWD_UNAVAILABLE: &str = "sandbox command cwd is unavailable";
#[cfg(windows)]
const COMMAND_PATH_OUTSIDE_WORKSPACE: &str = "sandbox command path is outside workspace";
#[cfg(windows)]
const COMMAND_READ_ONLY_WRITE_DENIED: &str = "sandbox command write denied in read-only mode";
#[cfg(windows)]
const COMMAND_SENSITIVE_PATH_DENIED: &str = "sandbox command path denied";
#[cfg(windows)]
const COMMAND_ENV_PATH_UNSUPPORTED: &str = "sandbox command env-expanded path is unsupported";
const COMMAND_UNSUPPORTED: &str = "sandbox command mode unsupported";
const SHELL_COMMAND_FLAGS: [&str; 3] = ["/c", "-c", "-command"];
#[cfg(windows)]
const WRITE_COMMAND_WORDS: [&str; 13] = [
    "copy",
    "cp",
    "del",
    "erase",
    "mkdir",
    "move",
    "mv",
    "new-item",
    "out-file",
    "remove-item",
    "ren",
    "rename",
    "set-content",
];
const REDACTED_COMMAND_OUTPUT: &str = "[redacted sensitive command output]";
#[cfg(windows)]
const SECRET_ENV_MARKERS: [&str; 6] = [
    "API_KEY",
    "AUTH",
    "CREDENTIAL",
    "PASSWORD",
    "SECRET",
    "TOKEN",
];
/// 命令请求的文件系统权限。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SandboxFilesystemMode {
    ReadOnly,
    WorkspaceWrite,
}

/// 命令请求的网络权限。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum SandboxNetworkMode {
    Denied,
    Allowed,
}

/// 沙箱命令对受保护工作区是否产生了实际文件系统变化的执行事实。
///
/// `WorkspaceWrite` 命令只有在 backend 明确返回 `Unchanged` 或 `Changed` 时才可进入
/// completion verification；`Unknown` 会在 `WorkspaceTools` 边界 fail closed。该事实不
/// 通过模型 payload 或普通 trace 暴露。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceMutation {
    Unchanged,
    Changed,
    #[default]
    Unknown,
}

/// Producer-owned summary of concrete workspace paths and published content diff digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceChangeSummary {
    pub changed_files: Vec<String>,
    pub diff_digest: String,
}

impl WorkspaceChangeSummary {
    /// Construct a producer-owned summary; the consuming runtime validates its bounds and digest.
    pub fn new(changed_files: Vec<String>, diff_digest: impl Into<String>) -> Self {
        Self {
            changed_files,
            diff_digest: diff_digest.into(),
        }
    }
}

/// 与命令请求使用的工作区根目录配对的文件系统策略。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SandboxFilesystemPolicy {
    pub mode: SandboxFilesystemMode,
    pub workspace_root: String,
}

/// 由选定沙箱 backend 应用的网络策略。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SandboxNetworkPolicy {
    pub mode: SandboxNetworkMode,
}

/// 执行是已完成、被拒绝、已取消、超时还是不可用。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CommandExecutionStatus {
    Completed,
    PolicyDenied,
    ReviewRequired,
    Unsupported,
    ExecutableUnavailable,
    TimedOut,
    Cancelled,
    BackendError,
}

/// 命令结果的领域语义，与 backend 执行状态分开保存。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CommandSemanticStatus {
    Succeeded,
    ExitNonzero,
    TestsFailed,
    BuildFailed,
    PolicyBlocked,
    Unsupported,
    TimedOut,
    Cancelled,
}

/// 控制哪些宿主环境变量可以传入子命令。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CommandEnvironmentPolicy {
    #[default]
    HostSanitized,
    EvaluationIsolated,
}

/// 交给沙箱 backend 的完整可移植命令请求。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CommandRequest {
    pub command_id: String,
    pub argv: Vec<String>,
    pub cwd: String,
    pub timeout_seconds: u64,
    pub network: SandboxNetworkPolicy,
    pub filesystem: SandboxFilesystemPolicy,
    #[serde(default)]
    pub environment: CommandEnvironmentPolicy,
    #[serde(skip)]
    #[schemars(skip)]
    protected_path_enforcement: ProtectedPathEnforcement,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum ProtectedPathEnforcement {
    #[default]
    Enforced,
    TrustedWorkspacePreparation,
}

impl CommandRequest {
    /// 构造 evaluator 使用的命令请求。
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
            timeout_seconds: DEFAULT_COMMAND_TIMEOUT_SECONDS,
            network: SandboxNetworkPolicy {
                mode: SandboxNetworkMode::Denied,
            },
            filesystem: SandboxFilesystemPolicy {
                mode: SandboxFilesystemMode::WorkspaceWrite,
                workspace_root: workspace_root.into(),
            },
            environment: CommandEnvironmentPolicy::default(),
            protected_path_enforcement: ProtectedPathEnforcement::Enforced,
        }
    }

    /// 构造产品控制面固定操作使用的工作区准备请求。
    ///
    /// 该来源只存在于进程内 Rust API，不参与序列化或 schema；反序列化后的请求始终恢复
    /// protected-path enforcement，模型脚本和 Evaluation manifest 命令无法选择该来源。
    pub fn trusted_workspace_preparation(
        command_id: impl Into<String>,
        argv: Vec<String>,
        cwd: impl Into<String>,
        workspace_root: impl Into<String>,
    ) -> Self {
        let mut request = Self::project_verification(command_id, argv, cwd, workspace_root);
        request.protected_path_enforcement = ProtectedPathEnforcement::TrustedWorkspacePreparation;
        request
    }

    /// 判断请求是否来自仅限进程内的可信工作区准备边界。
    pub fn is_trusted_workspace_preparation(&self) -> bool {
        matches!(
            self.protected_path_enforcement,
            ProtectedPathEnforcement::TrustedWorkspacePreparation
        )
    }

    /// 判断请求是否必须经过 sandbox。
    pub fn requires_sandbox(&self) -> bool {
        true
    }

    /// 返回用于 policy/approval 的资源标识。
    pub fn permission_resource(&self) -> String {
        command_permission_resource(&self.argv)
    }
}

/// 交给 sandbox backend 的模型命令脚本请求；它与可信内部 `argv` 请求保持类型隔离。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandScriptRequest {
    pub command_id: String,
    pub script: String,
    pub cwd: String,
    pub timeout_seconds: u64,
    pub network: SandboxNetworkPolicy,
    pub filesystem: SandboxFilesystemPolicy,
    pub environment: CommandEnvironmentPolicy,
}

impl CommandScriptRequest {
    /// 创建使用只读文件系统和拒绝网络的模型命令请求。
    pub fn agent_requested(
        command_id: impl Into<String>,
        script: impl Into<String>,
        cwd: impl Into<String>,
        workspace_root: impl Into<String>,
    ) -> Self {
        Self {
            command_id: command_id.into(),
            script: script.into(),
            cwd: cwd.into(),
            timeout_seconds: DEFAULT_COMMAND_TIMEOUT_SECONDS,
            network: SandboxNetworkPolicy {
                mode: SandboxNetworkMode::Denied,
            },
            filesystem: SandboxFilesystemPolicy {
                mode: SandboxFilesystemMode::ReadOnly,
                workspace_root: workspace_root.into(),
            },
            environment: CommandEnvironmentPolicy::default(),
        }
    }

    /// 创建由 Agent/Policy 绑定文件系统和网络范围的模型 command 请求。
    pub fn agent_requested_with_policy(
        command_id: impl Into<String>,
        script: impl Into<String>,
        cwd: impl Into<String>,
        workspace_root: impl Into<String>,
        filesystem: SandboxFilesystemMode,
        network: SandboxNetworkMode,
    ) -> Self {
        Self {
            command_id: command_id.into(),
            script: script.into(),
            cwd: cwd.into(),
            timeout_seconds: DEFAULT_COMMAND_TIMEOUT_SECONDS,
            network: SandboxNetworkPolicy { mode: network },
            filesystem: SandboxFilesystemPolicy {
                mode: filesystem,
                workspace_root: workspace_root.into(),
            },
            environment: CommandEnvironmentPolicy::default(),
        }
    }

    /// 模型脚本始终要求经过 sandbox backend 执行。
    pub fn requires_sandbox(&self) -> bool {
        true
    }
}

/// backend 实际提供的强制执行强度。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SandboxBackendEnforcement {
    Strict,
    RestrictedToken,
    Unavailable,
}

/// 用于区分严格、受限和不可用 backend 的安全执行元数据。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SandboxExecutionMetadata {
    pub backend: String,
    pub enforcement: SandboxBackendEnforcement,
    pub local_process_fallback: bool,
}

impl SandboxExecutionMetadata {
    /// 构造 backend 不可用的结果元数据。
    pub fn unavailable(backend: impl Into<String>) -> Self {
        Self {
            backend: backend.into(),
            enforcement: SandboxBackendEnforcement::Unavailable,
            local_process_fallback: false,
        }
    }

    /// 构造已在 sandbox 中执行的结果元数据。
    pub fn sandboxed(backend: impl Into<String>, enforcement: SandboxBackendEnforcement) -> Self {
        Self {
            backend: backend.into(),
            enforcement,
            local_process_fallback: false,
        }
    }
}

/// 带脱敏预览和 backend 强制执行元数据的有界命令结果。
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
    pub sandbox: SandboxExecutionMetadata,
    #[serde(skip)]
    #[schemars(skip)]
    pub workspace_mutation: WorkspaceMutation,
    #[serde(skip)]
    #[schemars(skip)]
    pub workspace_change_summary: Option<WorkspaceChangeSummary>,
}

/// 输出限制辅助函数返回的有界文本预览。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct BoundedCommandOutput {
    pub preview: String,
    pub truncated: bool,
}

/// 限制命令输出，避免向调用方暴露无界的 stdout 或 stderr。
pub fn bound_command_output(output: &str, max_chars: usize) -> BoundedCommandOutput {
    let preview = output.chars().take(max_chars).collect::<String>();
    let truncated = output.chars().count() > preview.chars().count();
    BoundedCommandOutput { preview, truncated }
}

impl CommandResult {
    /// 构造成功且带有界预览的结果。
    pub fn completed(command_id: impl Into<String>, stdout_preview: impl Into<String>) -> Self {
        let stdout = safe_command_preview(stdout_preview.into());
        Self {
            command_id: command_id.into(),
            execution_status: CommandExecutionStatus::Completed,
            semantic_status: CommandSemanticStatus::Succeeded,
            exit_code: Some(0),
            duration_ms: 0,
            timed_out: false,
            stdout_preview: stdout.preview,
            stderr_preview: String::new(),
            output_truncated: stdout.truncated,
            redacted: true,
            sandbox: SandboxExecutionMetadata::unavailable("not_executed"),
            workspace_mutation: WorkspaceMutation::Unknown,
            workspace_change_summary: None,
        }
    }

    /// 构造策略拒绝结果。
    pub fn policy_denied(command_id: impl Into<String>, reason: impl Into<String>) -> Self {
        Self::blocked(
            command_id,
            CommandExecutionStatus::PolicyDenied,
            CommandSemanticStatus::PolicyBlocked,
            reason,
        )
    }

    /// 构造 sandbox backend 不可用结果。
    pub fn sandbox_backend_unavailable(command_id: impl Into<String>) -> Self {
        Self::backend_error(command_id, SANDBOX_BACKEND_UNAVAILABLE)
    }

    /// 构造 backend 错误结果。
    pub fn backend_error(command_id: impl Into<String>, reason: impl Into<String>) -> Self {
        Self::blocked(
            command_id,
            CommandExecutionStatus::BackendError,
            CommandSemanticStatus::PolicyBlocked,
            reason,
        )
    }

    /// 构造平台不支持结果。
    pub fn unsupported(command_id: impl Into<String>, reason: impl Into<String>) -> Self {
        Self::blocked(
            command_id,
            CommandExecutionStatus::Unsupported,
            CommandSemanticStatus::Unsupported,
            format!("{}: {}", COMMAND_UNSUPPORTED, reason.into()),
        )
    }

    /// 构造可执行文件不可用结果。
    pub fn executable_unavailable(
        command_id: impl Into<String>,
        reason: impl Into<String>,
    ) -> Self {
        Self::blocked(
            command_id,
            CommandExecutionStatus::ExecutableUnavailable,
            CommandSemanticStatus::Unsupported,
            format!("{}: {}", COMMAND_EXECUTABLE_UNAVAILABLE, reason.into()),
        )
    }

    /// 构造超时结果。
    pub fn timed_out(command_id: impl Into<String>, duration_ms: u64) -> Self {
        let mut result = Self::blocked(
            command_id,
            CommandExecutionStatus::TimedOut,
            CommandSemanticStatus::TimedOut,
            COMMAND_TIMED_OUT,
        );
        result.duration_ms = duration_ms;
        result.timed_out = true;
        result
    }

    /// 构造取消结果。
    pub fn cancelled(command_id: impl Into<String>, duration_ms: u64) -> Self {
        let mut result = Self::blocked(
            command_id,
            CommandExecutionStatus::Cancelled,
            CommandSemanticStatus::Cancelled,
            COMMAND_CANCELLED,
        );
        result.duration_ms = duration_ms;
        result
    }

    /// 构造已执行命令的详细结果。
    pub fn executed(
        command_id: impl Into<String>,
        exit_code: i32,
        duration_ms: u64,
        stdout: impl Into<String>,
        stderr: impl Into<String>,
        captured_truncated: bool,
    ) -> Self {
        let stdout = safe_command_preview(stdout.into());
        let stderr = safe_command_preview(stderr.into());
        let succeeded = exit_code == 0;
        Self {
            command_id: command_id.into(),
            execution_status: CommandExecutionStatus::Completed,
            semantic_status: if succeeded {
                CommandSemanticStatus::Succeeded
            } else {
                CommandSemanticStatus::ExitNonzero
            },
            exit_code: Some(exit_code),
            duration_ms,
            timed_out: false,
            stdout_preview: stdout.preview,
            stderr_preview: stderr.preview,
            output_truncated: captured_truncated || stdout.truncated || stderr.truncated,
            redacted: true,
            sandbox: SandboxExecutionMetadata::unavailable("not_executed"),
            workspace_mutation: WorkspaceMutation::Unknown,
            workspace_change_summary: None,
        }
    }

    /// 记录 sandbox 执行元数据。
    pub fn with_sandbox_execution(
        mut self,
        backend: impl Into<String>,
        enforcement: SandboxBackendEnforcement,
    ) -> Self {
        self.sandbox = SandboxExecutionMetadata::sandboxed(backend, enforcement);
        self
    }

    /// 绑定 backend 对受保护工作区变化的明确执行观察。
    pub fn with_workspace_mutation(mut self, mutation: WorkspaceMutation) -> Self {
        self.workspace_mutation = mutation;
        self
    }

    /// Bind a trusted producer summary to the command result.
    pub fn with_workspace_change_summary(mut self, summary: WorkspaceChangeSummary) -> Self {
        self.workspace_change_summary = Some(summary);
        self
    }

    fn blocked(
        command_id: impl Into<String>,
        execution_status: CommandExecutionStatus,
        semantic_status: CommandSemanticStatus,
        reason: impl Into<String>,
    ) -> Self {
        let stderr = safe_command_preview(reason.into());
        Self {
            command_id: command_id.into(),
            execution_status,
            semantic_status,
            exit_code: None,
            duration_ms: 0,
            timed_out: false,
            stdout_preview: String::new(),
            stderr_preview: stderr.preview,
            output_truncated: stderr.truncated,
            redacted: true,
            sandbox: SandboxExecutionMetadata::unavailable("not_executed"),
            workspace_mutation: WorkspaceMutation::Unknown,
            workspace_change_summary: None,
        }
    }
}

fn safe_command_preview(output: String) -> BoundedCommandOutput {
    if command_output_contains_sensitive_marker(&output) {
        return BoundedCommandOutput {
            preview: REDACTED_COMMAND_OUTPUT.to_string(),
            truncated: true,
        };
    }
    bound_command_output(&output, DEFAULT_MAX_OUTPUT_CHARS)
}

fn command_output_contains_sensitive_marker(output: &str) -> bool {
    contains_sensitive_text(output)
}

/// backend 必须提供、命令执行才可视为可用的能力。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SandboxCapabilities {
    pub filesystem_isolation: bool,
    pub copy_on_write: bool,
    pub readonly_mount: bool,
    pub network_isolation: bool,
    pub env_isolation: bool,
    pub restricted_token: bool,
    pub job_object: bool,
    pub path_admission: bool,
    pub process_tree_kill: bool,
    pub timeout: bool,
    pub output_limit: bool,
    pub memory_limit: bool,
    pub process_limit: bool,
    pub artifact_capture: bool,
    pub change_detection: bool,
}

/// Stable outcome of the run-level sandbox preflight.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SandboxPreflightOutcome {
    Supported,
    Unsupported,
}

/// A bounded fact about one platform-specific or cross-platform control.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SandboxPreflightFact {
    Passed,
    Failed,
    NotApplicable,
    Unknown,
}

/// Stable, redacted facts collected before an Evaluation run samples a provider.
///
/// Platform adapters may refine the platform-specific facts, while the portable
/// fields describe the command sandbox contract consumed by AppServer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SandboxPreflightReport {
    pub outcome: SandboxPreflightOutcome,
    pub error_code: Option<String>,
    pub profile: String,
    pub backend: String,
    pub missing_capabilities: Vec<String>,
    pub os: String,
    pub arch: String,
    pub kernel: Option<String>,
    pub filesystem: Option<String>,
    pub overlayfs: SandboxPreflightFact,
    pub user_namespace: SandboxPreflightFact,
    pub mount_namespace: SandboxPreflightFact,
    pub pid_namespace: SandboxPreflightFact,
    pub network_namespace: SandboxPreflightFact,
    pub no_new_privs: SandboxPreflightFact,
    pub seccomp: SandboxPreflightFact,
    pub landlock: SandboxPreflightFact,
    pub transactional_workspace: SandboxPreflightFact,
    pub network_denied: SandboxPreflightFact,
    pub protected_paths: SandboxPreflightFact,
}

impl SandboxPreflightReport {
    fn from_capabilities(backend: &(impl SandboxBackend + ?Sized)) -> Self {
        let capabilities = backend.capabilities();
        let mut missing_capabilities = Vec::new();
        let required = [
            (capabilities.filesystem_isolation, "filesystem_isolation"),
            (capabilities.network_isolation, "network_isolation"),
            (capabilities.env_isolation, "env_isolation"),
            (capabilities.path_admission, "path_admission"),
            (capabilities.process_tree_kill, "process_tree_kill"),
            (capabilities.timeout, "timeout"),
            (capabilities.output_limit, "output_limit"),
            (capabilities.change_detection, "transactional_workspace"),
        ];
        for (available, name) in required {
            if !available {
                missing_capabilities.push(name.to_string());
            }
        }
        let unavailable = !missing_capabilities.is_empty()
            || capabilities.enforcement() != SandboxBackendEnforcement::Strict;
        Self {
            outcome: if unavailable {
                SandboxPreflightOutcome::Unsupported
            } else {
                SandboxPreflightOutcome::Supported
            },
            error_code: unavailable.then(|| "sandbox_preflight_unavailable".to_string()),
            profile: "workspace_write_network_denied".to_string(),
            backend: backend.name().to_string(),
            missing_capabilities,
            os: std::env::consts::OS.to_string(),
            arch: std::env::consts::ARCH.to_string(),
            kernel: None,
            filesystem: None,
            overlayfs: SandboxPreflightFact::NotApplicable,
            user_namespace: SandboxPreflightFact::NotApplicable,
            mount_namespace: SandboxPreflightFact::NotApplicable,
            pid_namespace: SandboxPreflightFact::NotApplicable,
            network_namespace: SandboxPreflightFact::NotApplicable,
            no_new_privs: SandboxPreflightFact::NotApplicable,
            seccomp: SandboxPreflightFact::NotApplicable,
            landlock: SandboxPreflightFact::NotApplicable,
            transactional_workspace: if capabilities.change_detection {
                SandboxPreflightFact::Unknown
            } else {
                SandboxPreflightFact::Failed
            },
            network_denied: if capabilities.network_isolation {
                SandboxPreflightFact::Unknown
            } else {
                SandboxPreflightFact::Failed
            },
            protected_paths: if capabilities.path_admission {
                SandboxPreflightFact::Unknown
            } else {
                SandboxPreflightFact::Failed
            },
        }
    }

    pub(crate) fn unsupported(&mut self, code: &'static str, missing: &[&str]) {
        self.outcome = SandboxPreflightOutcome::Unsupported;
        self.error_code = Some(code.to_string());
        for capability in missing {
            if !self
                .missing_capabilities
                .iter()
                .any(|item| item == capability)
            {
                self.missing_capabilities.push((*capability).to_string());
            }
        }
    }

    /// Verify that a supported report belongs to the selected backend and proves every strict
    /// Evaluation control before the caller is allowed to sample a provider.
    pub fn proves_supported_contract_for(&self, backend_name: &str) -> bool {
        const MAX_FACT_CHARS: usize = 128;
        let valid_text =
            |value: &str| !value.trim().is_empty() && value.chars().count() <= MAX_FACT_CHARS;
        let facts = [
            self.overlayfs,
            self.user_namespace,
            self.mount_namespace,
            self.pid_namespace,
            self.network_namespace,
            self.no_new_privs,
            self.seccomp,
            self.landlock,
            self.transactional_workspace,
            self.network_denied,
            self.protected_paths,
        ];
        self.outcome == SandboxPreflightOutcome::Supported
            && self.error_code.is_none()
            && self.profile == "workspace_write_network_denied"
            && self.backend == backend_name
            && valid_text(&self.backend)
            && valid_text(&self.os)
            && valid_text(&self.arch)
            && self.kernel.as_deref().is_none_or(valid_text)
            && self.filesystem.as_deref().is_none_or(valid_text)
            && self.missing_capabilities.is_empty()
            && !facts.contains(&SandboxPreflightFact::Unknown)
            && self.transactional_workspace == SandboxPreflightFact::Passed
            && self.network_denied == SandboxPreflightFact::Passed
            && self.protected_paths == SandboxPreflightFact::Passed
            && (self.os != "linux"
                || (self.kernel.is_some()
                    && self.filesystem.is_some()
                    && self.overlayfs == SandboxPreflightFact::Passed
                    && self.user_namespace == SandboxPreflightFact::Passed
                    && self.mount_namespace == SandboxPreflightFact::Passed
                    && self.pid_namespace == SandboxPreflightFact::Passed
                    && self.network_namespace == SandboxPreflightFact::Passed
                    && self.no_new_privs == SandboxPreflightFact::Passed
                    && self.seccomp == SandboxPreflightFact::Passed
                    && self.landlock == SandboxPreflightFact::Passed))
            && (self.os != "windows" || (self.kernel.is_some() && self.filesystem.is_some()))
    }
}

impl SandboxCapabilities {
    /// 返回完整严格命令执行能力。
    pub fn strict() -> Self {
        Self {
            filesystem_isolation: true,
            copy_on_write: false,
            readonly_mount: false,
            network_isolation: true,
            env_isolation: true,
            restricted_token: true,
            job_object: true,
            path_admission: true,
            process_tree_kill: true,
            timeout: true,
            output_limit: true,
            memory_limit: false,
            process_limit: false,
            artifact_capture: false,
            change_detection: false,
        }
    }

    /// 返回仅具备 restricted-token 能力的执行集合。
    pub fn restricted_token() -> Self {
        Self {
            filesystem_isolation: false,
            copy_on_write: false,
            readonly_mount: false,
            network_isolation: false,
            env_isolation: true,
            restricted_token: true,
            job_object: true,
            path_admission: true,
            process_tree_kill: true,
            timeout: true,
            output_limit: true,
            memory_limit: false,
            process_limit: false,
            artifact_capture: false,
            change_detection: false,
        }
    }

    /// 判断能力集合是否足以满足严格命令契约。
    pub fn supports_command_execution(&self) -> bool {
        self.env_isolation
            && self.path_admission
            && self.process_tree_kill
            && self.timeout
            && self.output_limit
            && ((self.filesystem_isolation && self.network_isolation)
                || (self.restricted_token && self.job_object))
    }

    /// 将能力投影为记录在命令结果中的强制执行级别。
    pub fn enforcement(&self) -> SandboxBackendEnforcement {
        if !self.supports_command_execution() {
            SandboxBackendEnforcement::Unavailable
        } else if self.filesystem_isolation && self.network_isolation {
            SandboxBackendEnforcement::Strict
        } else {
            SandboxBackendEnforcement::RestrictedToken
        }
    }

    /// 返回没有可用命令 backend 的能力集合。
    pub fn unavailable() -> Self {
        Self {
            filesystem_isolation: false,
            copy_on_write: false,
            readonly_mount: false,
            network_isolation: false,
            env_isolation: false,
            restricted_token: false,
            job_object: false,
            path_admission: false,
            process_tree_kill: false,
            timeout: false,
            output_limit: true,
            memory_limit: false,
            process_limit: false,
            artifact_capture: false,
            change_detection: false,
        }
    }

    /// 声明 backend 同时能够返回受保护工作区变化事实。
    pub fn with_change_detection(mut self) -> Self {
        self.change_detection = true;
        self
    }
}

/// 严格命令执行和取消传播的 backend 边界。
pub trait SandboxBackend {
    /// 用于能力和执行元数据的稳定 backend 名称。
    fn name(&self) -> &'static str;
    /// 报告该 backend 在当前平台能够强制执行的控制项。
    fn capabilities(&self) -> SandboxCapabilities;

    /// Probe the strict Evaluation command contract in a caller-owned scratch workspace.
    ///
    /// The default implementation exercises the portable workspace-write, denied-network and
    /// protected-path boundaries. Platform adapters can refine the returned OS/kernel facts.
    fn preflight(
        &self,
        workspace: &Path,
        cancellation: &CancellationToken,
    ) -> SandboxPreflightReport {
        default_sandbox_preflight(self, workspace, cancellation)
    }
    /// 执行一个请求；不可用或不支持的 backend 必须返回阻塞结果。
    fn execute(&self, request: &CommandRequest) -> CommandResult;

    /// 执行模型提交的 shell script；不支持的平台必须返回 typed unsupported。
    ///
    /// 对 `WorkspaceWrite` 请求，支持变化检测的 backend 必须在返回的
    /// [`CommandResult`] 中绑定 [`WorkspaceMutation::Unchanged`] 或
    /// [`WorkspaceMutation::Changed`]；无法证明时保留 `Unknown`，由上层拒绝验证。
    fn execute_script(&self, request: &CommandScriptRequest) -> CommandResult {
        CommandResult::unsupported(&request.command_id, COMMAND_SCRIPT_UNSUPPORTED)
            .with_workspace_mutation(WorkspaceMutation::Unknown)
    }

    /// 执行并支持取消，默认先进行执行前取消检查。
    fn execute_cancellable(
        &self,
        request: &CommandRequest,
        cancellation: &CancellationToken,
    ) -> CommandResult {
        if cancellation.is_cancelled() {
            return CommandResult::cancelled(&request.command_id, 0)
                .with_workspace_mutation(WorkspaceMutation::Unknown);
        }
        self.execute(request)
    }

    /// 执行模型 script 并支持取消；默认实现保持 fail closed。
    fn execute_script_cancellable(
        &self,
        request: &CommandScriptRequest,
        cancellation: &CancellationToken,
    ) -> CommandResult {
        if cancellation.is_cancelled() {
            return CommandResult::cancelled(&request.command_id, 0)
                .with_workspace_mutation(WorkspaceMutation::Unknown);
        }
        self.execute_script(request)
    }
}

pub(crate) fn baseline_sandbox_preflight(
    backend: &(impl SandboxBackend + ?Sized),
) -> SandboxPreflightReport {
    SandboxPreflightReport::from_capabilities(backend)
}

pub(crate) fn default_sandbox_preflight(
    backend: &(impl SandboxBackend + ?Sized),
    _workspace: &Path,
    _cancellation: &CancellationToken,
) -> SandboxPreflightReport {
    let mut report = baseline_sandbox_preflight(backend);
    report.unsupported("sandbox_preflight_not_implemented", &["preflight_probe"]);
    report
}

pub(crate) fn preflight_command(
    backend: &(impl SandboxBackend + ?Sized),
    workspace: &Path,
    argv: Vec<String>,
    network: SandboxNetworkMode,
    cancellation: &CancellationToken,
    label: &str,
) -> CommandResult {
    let mut request = CommandRequest::project_verification(
        format!("sandbox_preflight_{}", label.replace(' ', "_")),
        argv,
        workspace.to_string_lossy().into_owned(),
        workspace.to_string_lossy().into_owned(),
    );
    request.timeout_seconds = 15;
    request.network.mode = network;
    request.environment = CommandEnvironmentPolicy::EvaluationIsolated;
    backend.execute_cancellable(&request, cancellation)
}

pub(crate) fn preflight_write_verified(
    result: &CommandResult,
    expected_relative_path: &str,
) -> bool {
    result.execution_status == CommandExecutionStatus::Completed
        && result.semantic_status == CommandSemanticStatus::Succeeded
        && result.workspace_mutation == WorkspaceMutation::Changed
        && result.sandbox.enforcement == SandboxBackendEnforcement::Strict
        && !result.sandbox.local_process_fallback
        && result
            .workspace_change_summary
            .as_ref()
            .is_some_and(|summary| {
                summary.changed_files.len() == 1
                    && summary.changed_files[0] == expected_relative_path
                    && summary.diff_digest.starts_with("sha256:")
            })
}

#[cfg(target_os = "linux")]
pub(crate) fn preflight_unchanged_verified(result: &CommandResult) -> bool {
    result.execution_status == CommandExecutionStatus::Completed
        && result.semantic_status == CommandSemanticStatus::Succeeded
        && result.workspace_mutation == WorkspaceMutation::Unchanged
        && result.sandbox.enforcement == SandboxBackendEnforcement::Strict
        && !result.sandbox.local_process_fallback
}

#[cfg(target_os = "linux")]
mod linux_backend;

#[cfg(target_os = "linux")]
pub use linux_backend::{
    LinuxCapability, LinuxSandboxBackend, LinuxSandboxProbe, probe_linux_capabilities,
};

#[cfg(windows)]
pub use windows_backend::WindowsSandboxBackend;

#[cfg(not(windows))]
#[derive(Debug, Clone, Default)]
/// 非 Windows 平台的占位 backend；执行始终保持 unsupported。
pub struct WindowsSandboxBackend;

#[cfg(not(windows))]
impl WindowsSandboxBackend {
    /// 创建平台不支持的 backend 占位对象。
    pub fn new() -> Self {
        Self
    }
}

#[cfg(not(windows))]
impl SandboxBackend for WindowsSandboxBackend {
    fn name(&self) -> &'static str {
        "windows"
    }

    fn capabilities(&self) -> SandboxCapabilities {
        SandboxCapabilities::unavailable()
    }

    fn execute(&self, request: &CommandRequest) -> CommandResult {
        CommandResult::sandbox_backend_unavailable(&request.command_id)
            .with_workspace_mutation(WorkspaceMutation::Unknown)
            .with_sandbox_execution(self.name(), SandboxBackendEnforcement::Unavailable)
    }
}

/// Backend selected for the current target without exposing platform selection to callers.
#[cfg(windows)]
pub type PlatformSandboxBackend = WindowsSandboxBackend;

/// Backend selected for the current target without exposing platform selection to callers.
#[cfg(target_os = "linux")]
pub type PlatformSandboxBackend = LinuxSandboxBackend;

/// Fail-closed placeholder for targets without a strict sandbox implementation.
#[cfg(not(any(windows, target_os = "linux")))]
pub type PlatformSandboxBackend = WindowsSandboxBackend;

/// 将 `argv` 转换为策略和审计记录使用的稳定权限资源标识。
pub fn command_permission_resource(argv: &[String]) -> String {
    if argv.is_empty() {
        return String::new();
    }
    let lower = argv
        .iter()
        .map(|part| part.replace('\\', "/").to_ascii_lowercase())
        .collect::<Vec<_>>();
    let first = command_executable_name(&lower[0]);
    if matches!(
        first.as_str(),
        "cmd" | "cmd.exe" | "sh" | "bash" | "powershell" | "powershell.exe" | "pwsh" | "pwsh.exe"
    ) && let Some(index) = lower
        .iter()
        .position(|part| SHELL_COMMAND_FLAGS.contains(&part.as_str()))
    {
        return lower[index + 1..].join(" ");
    }
    lower.join(" ")
}

fn command_executable_name(value: &str) -> String {
    Path::new(value)
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| value.to_string())
}

#[cfg(windows)]
fn command_reference_tokens(request: &CommandRequest) -> Vec<String> {
    let mut tokens = Vec::new();
    for part in request.argv.iter().skip(1) {
        collect_command_tokens(part, &mut tokens);
    }
    if request
        .argv
        .first()
        .is_some_and(|value| is_shell_executable(value))
    {
        collect_command_tokens(&command_permission_resource(&request.argv), &mut tokens);
    }
    tokens
}

#[cfg(windows)]
fn is_shell_executable(value: &str) -> bool {
    matches!(
        command_executable_name(&value.replace('\\', "/").to_ascii_lowercase()).as_str(),
        "cmd" | "cmd.exe" | "sh" | "bash" | "powershell" | "powershell.exe" | "pwsh" | "pwsh.exe"
    )
}

#[cfg(windows)]
fn collect_command_tokens(value: &str, tokens: &mut Vec<String>) {
    tokens.extend(
        value
            .split(command_token_separator)
            .map(clean_command_token)
            .filter(|token| !token.is_empty()),
    );
}

#[cfg(windows)]
fn command_token_separator(ch: char) -> bool {
    ch.is_whitespace() || matches!(ch, '&' | '|' | ';' | '<' | '>')
}

#[cfg(windows)]
fn clean_command_token(value: &str) -> String {
    value
        .trim_matches(|ch| {
            matches!(
                ch,
                '"' | '\'' | '`' | '(' | ')' | '[' | ']' | '{' | '}' | ','
            )
        })
        .to_string()
}

#[cfg(windows)]
fn command_token_references_path(cwd: &Path, token: &str) -> bool {
    let lower = token.to_ascii_lowercase();
    if SHELL_COMMAND_FLAGS.contains(&lower.as_str()) {
        return false;
    }
    let normalized = token.replace('\\', "/");
    Path::new(token).is_absolute()
        || normalized == "."
        || normalized == ".."
        || normalized.starts_with("./")
        || normalized.starts_with("../")
        || normalized.starts_with("~/")
        || normalized.contains("/../")
        || normalized.contains("/./")
        || normalized.contains('/')
        || cwd.join(token).exists()
}

#[cfg(windows)]
fn command_token_has_env_reference(token: &str) -> bool {
    let lower = token.to_ascii_lowercase();
    token.contains('%')
        || lower.contains("$env:")
        || lower.contains("${")
        || lower.contains("$home")
        || lower.contains("$userprofile")
}

#[cfg(windows)]
fn command_token_has_sensitive_path_marker(token: &str) -> bool {
    token
        .replace('\\', "/")
        .split('/')
        .map(|component| {
            component
                .trim_matches(|ch| {
                    matches!(
                        ch,
                        '"' | '\'' | '`' | '(' | ')' | '[' | ']' | '{' | '}' | ','
                    )
                })
                .to_ascii_lowercase()
        })
        .any(|component| is_protected_path(&component))
}

#[cfg(windows)]
fn command_has_read_only_write_intent(request: &CommandRequest) -> bool {
    let resource = command_permission_resource(&request.argv);
    request.argv.first().is_some_and(|executable| {
        let executable = command_executable_name(&executable.to_ascii_lowercase());
        WRITE_COMMAND_WORDS.contains(&executable.as_str())
    }) || command_has_file_redirection(&resource)
        || command_reference_tokens(request).iter().any(|token| {
            let lower = token.to_ascii_lowercase();
            WRITE_COMMAND_WORDS
                .iter()
                .any(|write_command| lower == *write_command)
        })
}

#[cfg(windows)]
fn command_has_file_redirection(value: &str) -> bool {
    let chars = value.chars().collect::<Vec<_>>();
    let mut index = 0;
    while index < chars.len() {
        if chars[index] == '>' {
            let target = redirection_target(&chars, index + 1);
            if !redirection_target_is_non_file(&target) {
                return true;
            }
            index += target.chars().count().max(1);
        }
        index += 1;
    }
    false
}

#[cfg(windows)]
fn redirection_target(chars: &[char], mut index: usize) -> String {
    while index < chars.len() && chars[index] == '>' {
        index += 1;
    }
    while index < chars.len() && chars[index].is_whitespace() {
        index += 1;
    }
    let mut target = String::new();
    while index < chars.len()
        && !chars[index].is_whitespace()
        && !matches!(chars[index], '&' | '|' | ';' | '<' | '>')
    {
        target.push(chars[index]);
        index += 1;
    }
    if target.is_empty() && index < chars.len() && chars[index] == '&' {
        target.push(chars[index]);
        index += 1;
        while index < chars.len() && chars[index].is_ascii_digit() {
            target.push(chars[index]);
            index += 1;
        }
    }
    clean_command_token(&target)
}

#[cfg(windows)]
fn redirection_target_is_non_file(target: &str) -> bool {
    let lower = target.to_ascii_lowercase();
    matches!(lower.as_str(), "nul" | "nul:" | "/dev/null" | "&1" | "&2")
}

#[cfg(windows)]
fn is_secret_env_name(name: &str) -> bool {
    let upper_name = name.to_ascii_uppercase();
    SECRET_ENV_MARKERS
        .iter()
        .any(|marker| upper_name.contains(marker))
}

#[cfg(windows)]
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

#[cfg(windows)]
fn command_request_denial(request: &CommandRequest) -> Option<CommandResult> {
    if request.argv.is_empty() {
        return Some(CommandResult::policy_denied(
            &request.command_id,
            COMMAND_EMPTY_ARGV,
        ));
    }
    let workspace = match std::fs::canonicalize(&request.filesystem.workspace_root) {
        Ok(path) => normalize_path(&path),
        Err(_) => {
            return Some(CommandResult::policy_denied(
                &request.command_id,
                COMMAND_PATH_OUTSIDE_WORKSPACE,
            ));
        }
    };
    let cwd = match resolve_existing_command_path(&workspace, &request.cwd) {
        Some(path) => path,
        None => {
            return Some(CommandResult::policy_denied(
                &request.command_id,
                COMMAND_CWD_UNAVAILABLE,
            ));
        }
    };
    if path_has_sensitive_component(&cwd) {
        return Some(CommandResult::policy_denied(
            &request.command_id,
            COMMAND_SENSITIVE_PATH_DENIED,
        ));
    }
    let workspace_bound = matches!(
        request.filesystem.mode,
        SandboxFilesystemMode::ReadOnly | SandboxFilesystemMode::WorkspaceWrite
    );
    if workspace_bound && !cwd.starts_with(&workspace) {
        return Some(CommandResult::policy_denied(
            &request.command_id,
            COMMAND_CWD_OUTSIDE_WORKSPACE,
        ));
    }
    let executable = Path::new(&request.argv[0]);
    if executable.components().count() > 1 && path_has_sensitive_component(executable) {
        return Some(CommandResult::policy_denied(
            &request.command_id,
            COMMAND_SENSITIVE_PATH_DENIED,
        ));
    }
    let command_tokens = command_reference_tokens(request);
    if command_tokens
        .iter()
        .any(|token| command_token_has_sensitive_path_marker(token))
    {
        return Some(CommandResult::policy_denied(
            &request.command_id,
            COMMAND_SENSITIVE_PATH_DENIED,
        ));
    }
    for token in command_tokens
        .iter()
        .filter(|token| command_token_references_path(&cwd, token))
    {
        if command_token_has_env_reference(token) {
            return Some(CommandResult::unsupported(
                &request.command_id,
                COMMAND_ENV_PATH_UNSUPPORTED,
            ));
        }
        let resolved = resolve_existing_or_parent_command_path(&cwd, token);
        if path_has_sensitive_component(&resolved) {
            return Some(CommandResult::policy_denied(
                &request.command_id,
                COMMAND_SENSITIVE_PATH_DENIED,
            ));
        }
        if workspace_bound && !resolved.starts_with(&workspace) {
            return Some(CommandResult::policy_denied(
                &request.command_id,
                COMMAND_PATH_OUTSIDE_WORKSPACE,
            ));
        }
    }
    match request.filesystem.mode {
        SandboxFilesystemMode::ReadOnly => {
            if command_has_read_only_write_intent(request) {
                return Some(CommandResult::policy_denied(
                    &request.command_id,
                    COMMAND_READ_ONLY_WRITE_DENIED,
                ));
            }
        }
        SandboxFilesystemMode::WorkspaceWrite => {}
    }
    None
}

#[cfg(windows)]
fn command_script_request_denial(request: &CommandScriptRequest) -> Option<CommandResult> {
    if request.script.trim().is_empty() {
        return Some(CommandResult::policy_denied(
            &request.command_id,
            COMMAND_EMPTY_SCRIPT,
        ));
    }
    let workspace = match std::fs::canonicalize(&request.filesystem.workspace_root) {
        Ok(path) => normalize_path(&path),
        Err(_) => {
            return Some(CommandResult::policy_denied(
                &request.command_id,
                COMMAND_PATH_OUTSIDE_WORKSPACE,
            ));
        }
    };
    let cwd = match resolve_existing_command_path(&workspace, &request.cwd) {
        Some(path) => path,
        None => {
            return Some(CommandResult::policy_denied(
                &request.command_id,
                COMMAND_CWD_UNAVAILABLE,
            ));
        }
    };
    if path_has_sensitive_component(&cwd) {
        return Some(CommandResult::policy_denied(
            &request.command_id,
            COMMAND_SENSITIVE_PATH_DENIED,
        ));
    }
    let workspace_bound = matches!(
        request.filesystem.mode,
        SandboxFilesystemMode::ReadOnly | SandboxFilesystemMode::WorkspaceWrite
    );
    if workspace_bound && !cwd.starts_with(&workspace) {
        return Some(CommandResult::policy_denied(
            &request.command_id,
            COMMAND_CWD_OUTSIDE_WORKSPACE,
        ));
    }
    // model script 已是独立字符串；这里只绑定请求自己的 cwd 和 workspace 边界。
    // script 内的 shell 语义、路径访问和写入由 policy、approval 与严格 backend 负责。
    None
}

#[cfg(windows)]
fn resolve_command_path(workspace: &Path, path: &str) -> PathBuf {
    let candidate = Path::new(path);
    let joined = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        workspace.join(candidate)
    };
    normalize_path(&joined)
}

#[cfg(windows)]
fn resolve_existing_command_path(workspace: &Path, path: &str) -> Option<PathBuf> {
    let resolved = resolve_command_path(workspace, path);
    std::fs::canonicalize(&resolved)
        .map(|path| normalize_path(&path))
        .ok()
}

#[cfg(windows)]
fn resolve_existing_or_parent_command_path(workspace: &Path, path: &str) -> PathBuf {
    let resolved = resolve_command_path(workspace, path);
    let mut missing = Vec::new();
    let mut ancestor = resolved.as_path();
    while !ancestor.exists() {
        let Some(name) = ancestor.file_name() else {
            return normalize_path(&resolved);
        };
        missing.push(name.to_owned());
        let Some(parent) = ancestor.parent() else {
            return normalize_path(&resolved);
        };
        ancestor = parent;
    }
    let mut canonical = std::fs::canonicalize(ancestor)
        .map(|path| normalize_path(&path))
        .unwrap_or_else(|_| normalize_path(ancestor));
    for component in missing.iter().rev() {
        canonical.push(component);
    }
    normalize_path(&canonical)
}

#[cfg(windows)]
fn path_has_sensitive_component(path: &Path) -> bool {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(value) => Some(value.to_string_lossy().to_ascii_lowercase()),
            _ => None,
        })
        .any(|component| is_protected_path(&component))
}

#[cfg(windows)]
mod windows_backend;
