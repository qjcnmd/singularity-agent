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

#[cfg(windows)]
mod protected_paths;

pub const DEFAULT_COMMAND_TIMEOUT_SECONDS: u64 = 30;
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
    DangerFullAccess,
}

/// 命令请求的网络权限。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum SandboxNetworkMode {
    Denied,
    Allowed,
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
            timeout_seconds: DEFAULT_COMMAND_TIMEOUT_SECONDS,
            network: SandboxNetworkPolicy {
                mode: SandboxNetworkMode::Denied,
            },
            filesystem: SandboxFilesystemPolicy {
                mode: SandboxFilesystemMode::WorkspaceWrite,
                workspace_root: workspace_root.into(),
            },
            environment: CommandEnvironmentPolicy::default(),
        }
    }

    pub fn requires_sandbox(&self) -> bool {
        true
    }

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
    pub fn unavailable(backend: impl Into<String>) -> Self {
        Self {
            backend: backend.into(),
            enforcement: SandboxBackendEnforcement::Unavailable,
            local_process_fallback: false,
        }
    }

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
        Self::backend_error(command_id, SANDBOX_BACKEND_UNAVAILABLE)
    }

    pub fn backend_error(command_id: impl Into<String>, reason: impl Into<String>) -> Self {
        Self::blocked(
            command_id,
            CommandExecutionStatus::BackendError,
            CommandSemanticStatus::PolicyBlocked,
            reason,
        )
    }

    pub fn unsupported(command_id: impl Into<String>, reason: impl Into<String>) -> Self {
        Self::blocked(
            command_id,
            CommandExecutionStatus::Unsupported,
            CommandSemanticStatus::Unsupported,
            format!("{}: {}", COMMAND_UNSUPPORTED, reason.into()),
        )
    }

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
        }
    }

    pub fn with_sandbox_execution(
        mut self,
        backend: impl Into<String>,
        enforcement: SandboxBackendEnforcement,
    ) -> Self {
        self.sandbox = SandboxExecutionMetadata::sandboxed(backend, enforcement);
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

impl SandboxCapabilities {
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
}

/// 严格命令执行和取消传播的 backend 边界。
pub trait SandboxBackend {
    /// 用于能力和执行元数据的稳定 backend 名称。
    fn name(&self) -> &'static str;
    /// 报告该 backend 在当前平台能够强制执行的控制项。
    fn capabilities(&self) -> SandboxCapabilities;
    /// 执行一个请求；不可用或不支持的 backend 必须返回阻塞结果。
    fn execute(&self, request: &CommandRequest) -> CommandResult;

    /// 执行模型提交的 shell script；不支持的平台必须返回 typed unsupported。
    fn execute_script(&self, request: &CommandScriptRequest) -> CommandResult {
        CommandResult::unsupported(&request.command_id, COMMAND_SCRIPT_UNSUPPORTED)
    }

    /// 执行并支持取消，默认先进行执行前取消检查。
    fn execute_cancellable(
        &self,
        request: &CommandRequest,
        cancellation: &CancellationToken,
    ) -> CommandResult {
        if cancellation.is_cancelled() {
            return CommandResult::cancelled(&request.command_id, 0);
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
            return CommandResult::cancelled(&request.command_id, 0);
        }
        self.execute_script(request)
    }
}

#[cfg(windows)]
pub use windows_backend::WindowsSandboxBackend;

#[cfg(not(windows))]
#[derive(Debug, Clone, Default)]
pub struct WindowsSandboxBackend;

#[cfg(not(windows))]
impl WindowsSandboxBackend {
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
            .with_sandbox_execution(self.name(), SandboxBackendEnforcement::Unavailable)
    }
}

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
        SandboxFilesystemMode::WorkspaceWrite | SandboxFilesystemMode::DangerFullAccess => {}
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
mod windows_backend {
    use std::collections::HashMap;
    use std::ffi::OsStr;
    use std::path::{Path, PathBuf};
    use std::time::Instant;

    use singularity_windows_sandbox::{
        AbsolutePathBuf, ElevatedSandboxProfileCaptureRequest, FileSystemSandboxPolicy,
        ManagedFileSystemPermissions, NetworkSandboxPolicy, PermissionProfile,
        WindowsSandboxCancellationToken, run_windows_sandbox_capture,
        run_windows_sandbox_capture_for_permission_profile_elevated,
    };

    use super::protected_paths::{ProtectedPathDiscoveryError, discover_existing_protected_paths};
    use super::{
        COMMAND_CANCELLED, COMMAND_TIMED_OUT, CancellationToken, CommandEnvironmentPolicy,
        CommandExecutionStatus, CommandRequest, CommandResult, CommandScriptRequest,
        CommandSemanticStatus, SandboxBackend, SandboxBackendEnforcement, SandboxCapabilities,
        SandboxFilesystemMode, SandboxNetworkMode, command_request_denial,
        command_script_request_denial, is_secret_env_name, path_has_sensitive_component,
    };

    const BACKEND_NAME: &str = "windows";
    const ELEVATED_BACKEND_NAME: &str = "windows_elevated";
    const RESTRICTED_TOKEN_BACKEND_NAME: &str = "windows_restricted_token";
    const SANDBOX_HOME_ENV: &str = "SINGULARITY_HOME";
    const USER_PROFILE_ENV: &str = "USERPROFILE";
    const DEFAULT_HOME_DIR_NAME: &str = ".singularity";
    const UNSAFE_BATCH_ARGUMENT: &str = "batch command contains unsupported shell syntax";
    const ELEVATED_FAILURE_PREFIX: &str = "elevated Windows sandbox failed";
    const RESTRICTED_FAILURE_PREFIX: &str = "restricted-token Windows sandbox failed";
    const PROTECTED_PATH_ENFORCEMENT_FAILED: &str = "protected workspace path enforcement failed";
    const DANGER_FULL_ACCESS_UNSUPPORTED: &str = "danger-full-access requires an explicit unsandboxed executor and is unavailable in the sandbox backend";

    #[derive(Debug)]
    struct ResolvedExecutable {
        argv: Vec<String>,
        read_roots: Vec<PathBuf>,
    }

    #[derive(Debug)]
    enum PrepareCommandError {
        Executable(ExecutableResolutionError),
        Backend(String),
        ProtectedPaths(ProtectedPathDiscoveryError),
    }

    #[derive(Debug, PartialEq, Eq)]
    enum ExecutableResolutionError {
        Unavailable(String),
        NotPermitted(String),
        Unsupported(String),
    }

    impl ExecutableResolutionError {
        fn into_command_result(self, command_id: &str) -> CommandResult {
            match self {
                Self::Unavailable(message) => {
                    CommandResult::executable_unavailable(command_id, message)
                }
                Self::Unsupported(message) => CommandResult::unsupported(command_id, message),
                Self::NotPermitted(message) => CommandResult::policy_denied(command_id, message),
            }
        }
    }

    #[derive(Debug, Clone, Default)]
    pub struct WindowsSandboxBackend;

    impl WindowsSandboxBackend {
        pub fn new() -> Self {
            Self
        }
    }

    impl SandboxBackend for WindowsSandboxBackend {
        fn name(&self) -> &'static str {
            BACKEND_NAME
        }

        fn capabilities(&self) -> SandboxCapabilities {
            SandboxCapabilities::strict()
        }

        fn execute(&self, request: &CommandRequest) -> CommandResult {
            self.execute_cancellable(request, &CancellationToken::new())
        }

        fn execute_cancellable(
            &self,
            request: &CommandRequest,
            cancellation: &CancellationToken,
        ) -> CommandResult {
            if cancellation.is_cancelled() {
                return CommandResult::cancelled(&request.command_id, 0)
                    .with_sandbox_execution(self.name(), SandboxBackendEnforcement::Strict);
            }
            if let Some(denied) = command_request_denial(request) {
                return denied
                    .with_sandbox_execution(self.name(), SandboxBackendEnforcement::Unavailable);
            }
            let prepared = match PreparedCommand::from_request(request) {
                Ok(prepared) => prepared,
                Err(PrepareCommandError::Executable(error)) => {
                    return error
                        .into_command_result(&request.command_id)
                        .with_sandbox_execution(self.name(), SandboxBackendEnforcement::Strict);
                }
                Err(PrepareCommandError::Backend(error)) => {
                    return CommandResult::backend_error(&request.command_id, error)
                        .with_sandbox_execution(
                            self.name(),
                            SandboxBackendEnforcement::Unavailable,
                        );
                }
                Err(PrepareCommandError::ProtectedPaths(error)) => {
                    return CommandResult::backend_error(&request.command_id, error.to_string())
                        .with_sandbox_execution(
                            self.name(),
                            SandboxBackendEnforcement::Unavailable,
                        );
                }
            };
            match execute_windows_sandbox(&request.command_id, cancellation, prepared) {
                Ok(result) => result,
                Err(error) => CommandResult::backend_error(&request.command_id, error)
                    .with_sandbox_execution(self.name(), SandboxBackendEnforcement::Unavailable),
            }
        }

        fn execute_script(&self, request: &CommandScriptRequest) -> CommandResult {
            self.execute_script_cancellable(request, &CancellationToken::new())
        }

        fn execute_script_cancellable(
            &self,
            request: &CommandScriptRequest,
            cancellation: &CancellationToken,
        ) -> CommandResult {
            if cancellation.is_cancelled() {
                return CommandResult::cancelled(&request.command_id, 0)
                    .with_sandbox_execution(self.name(), SandboxBackendEnforcement::Strict);
            }
            if let Some(denied) = command_script_request_denial(request) {
                return denied
                    .with_sandbox_execution(self.name(), SandboxBackendEnforcement::Unavailable);
            }
            let prepared = match PreparedCommand::from_script_request(request) {
                Ok(prepared) => prepared,
                Err(PrepareCommandError::Executable(error)) => {
                    return error
                        .into_command_result(&request.command_id)
                        .with_sandbox_execution(self.name(), SandboxBackendEnforcement::Strict);
                }
                Err(PrepareCommandError::Backend(error)) => {
                    return CommandResult::backend_error(&request.command_id, error)
                        .with_sandbox_execution(
                            self.name(),
                            SandboxBackendEnforcement::Unavailable,
                        );
                }
                Err(PrepareCommandError::ProtectedPaths(error)) => {
                    return CommandResult::backend_error(&request.command_id, error.to_string())
                        .with_sandbox_execution(
                            self.name(),
                            SandboxBackendEnforcement::Unavailable,
                        );
                }
            };
            match execute_windows_sandbox(&request.command_id, cancellation, prepared) {
                Ok(result) => result,
                Err(error) => CommandResult::backend_error(&request.command_id, error)
                    .with_sandbox_execution(self.name(), SandboxBackendEnforcement::Unavailable),
            }
        }
    }

    struct PreparedCommand {
        permission_profile: PermissionProfile,
        workspace_roots: Vec<AbsolutePathBuf>,
        sandbox_home: PathBuf,
        cwd: PathBuf,
        env_map: HashMap<String, String>,
        timeout_ms: u64,
        restricted_token_fallback: bool,
        argv: Vec<String>,
        read_roots: Vec<PathBuf>,
        protected_deny_read_paths: Vec<AbsolutePathBuf>,
    }

    impl PreparedCommand {
        fn from_request(request: &CommandRequest) -> Result<Self, PrepareCommandError> {
            let workspace_root = canonical_directory(Path::new(&request.filesystem.workspace_root))
                .map_err(PrepareCommandError::Backend)?;
            let cwd = canonical_directory(Path::new(&request.cwd))
                .map_err(PrepareCommandError::Backend)?;
            let env_map = child_environment(&request.environment);
            let resolved = resolve_executable(&request.argv, &cwd, &env_map)
                .map_err(PrepareCommandError::Executable)?;
            let workspace_root = AbsolutePathBuf::from_absolute_path_checked(&workspace_root)
                .map_err(|error| {
                    PrepareCommandError::Backend(format!("invalid workspace root: {error}"))
                })?;
            let workspace_roots = vec![workspace_root];
            let network = match request.network.mode {
                SandboxNetworkMode::Denied => NetworkSandboxPolicy::Restricted,
                SandboxNetworkMode::Allowed => NetworkSandboxPolicy::Enabled,
            };
            let permission_profile = match request.filesystem.mode {
                SandboxFilesystemMode::ReadOnly => PermissionProfile::Managed {
                    file_system: ManagedFileSystemPermissions::Restricted {
                        entries: FileSystemSandboxPolicy::read_only().entries,
                        glob_scan_max_depth: None,
                    },
                    network,
                },
                SandboxFilesystemMode::WorkspaceWrite => {
                    PermissionProfile::workspace_write_with(&[], network, false, false)
                }
                SandboxFilesystemMode::DangerFullAccess => {
                    return Err(PrepareCommandError::Backend(
                        DANGER_FULL_ACCESS_UNSUPPORTED.to_string(),
                    ));
                }
            };
            let restricted_token_fallback = singularity_windows_sandbox::ResolvedWindowsSandboxPermissions::try_from_permission_profile_for_workspace_roots(
                &permission_profile,
                &workspace_roots,
            )
            .map_err(|error| {
                PrepareCommandError::Backend(format!(
                    "invalid Windows sandbox permissions: {error}"
                ))
            })?
            .supports_restricted_token_fallback();
            Ok(Self {
                permission_profile,
                workspace_roots,
                sandbox_home: sandbox_home().map_err(PrepareCommandError::Backend)?,
                cwd,
                env_map,
                timeout_ms: request.timeout_seconds.saturating_mul(1_000),
                restricted_token_fallback,
                argv: resolved.argv,
                read_roots: resolved.read_roots,
                protected_deny_read_paths: Vec::new(),
            })
        }

        fn from_script_request(
            request: &CommandScriptRequest,
        ) -> Result<Self, PrepareCommandError> {
            let workspace_root = canonical_directory(Path::new(&request.filesystem.workspace_root))
                .map_err(PrepareCommandError::Backend)?;
            let cwd = canonical_directory(Path::new(&request.cwd))
                .map_err(PrepareCommandError::Backend)?;
            let env_map = child_environment(&request.environment);
            let powershell = system_powershell(&env_map).ok_or_else(|| {
                PrepareCommandError::Executable(ExecutableResolutionError::Unavailable(
                    "required Windows PowerShell executable was not found".to_string(),
                ))
            })?;
            let argv = singularity_windows_sandbox::powershell_encoded_command_argv(
                powershell.clone(),
                &request.script,
            )
            .map_err(|error| {
                PrepareCommandError::Executable(ExecutableResolutionError::Unsupported(error))
            })?;
            let workspace_root = AbsolutePathBuf::from_absolute_path_checked(&workspace_root)
                .map_err(|error| {
                    PrepareCommandError::Backend(format!("invalid workspace root: {error}"))
                })?;
            let protected_deny_read_paths = discover_existing_protected_paths(&workspace_root)
                .map_err(PrepareCommandError::ProtectedPaths)?;
            let workspace_roots = vec![workspace_root];
            let network = match request.network.mode {
                SandboxNetworkMode::Denied => NetworkSandboxPolicy::Restricted,
                SandboxNetworkMode::Allowed => NetworkSandboxPolicy::Enabled,
            };
            let permission_profile = match request.filesystem.mode {
                SandboxFilesystemMode::ReadOnly => PermissionProfile::Managed {
                    file_system: ManagedFileSystemPermissions::Restricted {
                        entries: FileSystemSandboxPolicy::read_only().entries,
                        glob_scan_max_depth: None,
                    },
                    network,
                },
                SandboxFilesystemMode::WorkspaceWrite => {
                    PermissionProfile::workspace_write_with(&[], network, false, false)
                }
                SandboxFilesystemMode::DangerFullAccess => {
                    return Err(PrepareCommandError::Backend(
                        DANGER_FULL_ACCESS_UNSUPPORTED.to_string(),
                    ));
                }
            };
            let restricted_token_fallback = singularity_windows_sandbox::ResolvedWindowsSandboxPermissions::try_from_permission_profile_for_workspace_roots(
                &permission_profile,
                &workspace_roots,
            )
            .map_err(|error| {
                PrepareCommandError::Backend(format!(
                    "invalid Windows sandbox permissions: {error}"
                ))
            })?
            .supports_restricted_token_fallback()
                && protected_deny_read_paths.is_empty();
            Ok(Self {
                permission_profile,
                workspace_roots,
                sandbox_home: sandbox_home().map_err(PrepareCommandError::Backend)?,
                cwd,
                env_map,
                timeout_ms: request.timeout_seconds.saturating_mul(1_000),
                restricted_token_fallback,
                argv,
                read_roots: executable_read_roots(&powershell),
                protected_deny_read_paths,
            })
        }
    }

    /// 将准备阶段的 deny-read 集合绑定到现有 elevated capture 请求。
    fn elevated_capture_request<'a>(
        prepared: &'a PreparedCommand,
        windows_cancellation: WindowsSandboxCancellationToken,
    ) -> ElevatedSandboxProfileCaptureRequest<'a> {
        let mut elevated = ElevatedSandboxProfileCaptureRequest::new(
            &prepared.permission_profile,
            &prepared.workspace_roots,
            &prepared.sandbox_home,
            prepared.argv.clone(),
            &prepared.cwd,
            prepared.env_map.clone(),
        );
        elevated.timeout_ms = Some(prepared.timeout_ms);
        elevated.cancellation = Some(windows_cancellation);
        elevated.additional_read_roots = &prepared.read_roots;
        elevated.deny_read_paths_override = &prepared.protected_deny_read_paths;
        elevated
    }

    fn execute_windows_sandbox(
        command_id: &str,
        cancellation: &CancellationToken,
        prepared: PreparedCommand,
    ) -> Result<CommandResult, String> {
        let started = Instant::now();
        let windows_cancellation = WindowsSandboxCancellationToken::new({
            let cancellation = cancellation.clone();
            move || cancellation.is_cancelled()
        });
        let elevated = run_windows_sandbox_capture_for_permission_profile_elevated(
            elevated_capture_request(&prepared, windows_cancellation.clone()),
        );
        match elevated {
            Ok(capture) => Ok(command_result_from_capture(command_id, capture, started)
                .with_sandbox_execution(ELEVATED_BACKEND_NAME, SandboxBackendEnforcement::Strict)),
            Err(elevated_error)
                if prepared.restricted_token_fallback
                    && prepared.protected_deny_read_paths.is_empty() =>
            {
                let elevated_error = windows_error_summary(&elevated_error);
                let capture = run_windows_sandbox_capture(
                    &prepared.permission_profile,
                    &prepared.workspace_roots,
                    &prepared.sandbox_home,
                    prepared.argv.clone(),
                    &prepared.cwd,
                    prepared.env_map,
                    Some(prepared.timeout_ms),
                    Some(windows_cancellation),
                    true,
                )
                .map_err(|restricted_error| {
                    let restricted_error = windows_error_summary(&restricted_error);
                    format!(
                        "{ELEVATED_FAILURE_PREFIX}: {elevated_error}; {RESTRICTED_FAILURE_PREFIX}: {restricted_error}"
                    )
                })?;
                Ok(command_result_from_capture(command_id, capture, started)
                    .with_sandbox_execution(
                        RESTRICTED_TOKEN_BACKEND_NAME,
                        SandboxBackendEnforcement::RestrictedToken,
                    ))
            }
            Err(error) => {
                if !prepared.protected_deny_read_paths.is_empty() {
                    Err(PROTECTED_PATH_ENFORCEMENT_FAILED.to_string())
                } else {
                    Err(format!(
                        "{ELEVATED_FAILURE_PREFIX}: {}",
                        windows_error_summary(&error)
                    ))
                }
            }
        }
    }

    fn windows_error_summary(error: &impl std::fmt::Display) -> String {
        error
            .to_string()
            .split_once(" | cwd=")
            .map_or_else(|| error.to_string(), |(summary, _)| summary.to_string())
    }

    fn command_result_from_capture(
        command_id: &str,
        capture: singularity_windows_sandbox::CaptureResult,
        started: Instant,
    ) -> CommandResult {
        let duration_ms = started.elapsed().as_millis().try_into().unwrap_or(u64::MAX);
        if capture.cancelled {
            return interrupted_command_result(
                command_id,
                &capture,
                duration_ms,
                CommandExecutionStatus::Cancelled,
                CommandSemanticStatus::Cancelled,
                COMMAND_CANCELLED,
            );
        }
        if capture.timed_out {
            return interrupted_command_result(
                command_id,
                &capture,
                duration_ms,
                CommandExecutionStatus::TimedOut,
                CommandSemanticStatus::TimedOut,
                COMMAND_TIMED_OUT,
            );
        }
        CommandResult::executed(
            command_id,
            capture.exit_code,
            duration_ms,
            String::from_utf8_lossy(&capture.stdout),
            String::from_utf8_lossy(&capture.stderr),
            capture.output_truncated,
        )
    }

    fn interrupted_command_result(
        command_id: &str,
        capture: &singularity_windows_sandbox::CaptureResult,
        duration_ms: u64,
        execution_status: CommandExecutionStatus,
        semantic_status: CommandSemanticStatus,
        fallback_message: &str,
    ) -> CommandResult {
        let mut result = CommandResult::executed(
            command_id,
            capture.exit_code,
            duration_ms,
            String::from_utf8_lossy(&capture.stdout),
            String::from_utf8_lossy(&capture.stderr),
            capture.output_truncated,
        );
        result.execution_status = execution_status;
        result.semantic_status = semantic_status;
        result.exit_code = None;
        result.timed_out = capture.timed_out;
        if result.stderr_preview.is_empty() {
            result.stderr_preview = fallback_message.to_string();
        }
        result
    }

    fn canonical_directory(path: &Path) -> Result<PathBuf, String> {
        let canonical = dunce::canonicalize(path)
            .map_err(|error| format!("failed to resolve {}: {error}", path.display()))?;
        if !canonical.is_dir() {
            return Err(format!("path is not a directory: {}", path.display()));
        }
        Ok(canonical)
    }

    fn resolve_executable(
        argv: &[String],
        cwd: &Path,
        env_map: &HashMap<String, String>,
    ) -> Result<ResolvedExecutable, ExecutableResolutionError> {
        let requested = argv.first().ok_or_else(|| {
            ExecutableResolutionError::Unsupported("sandbox command argv is empty".to_string())
        })?;
        let requested_path = Path::new(requested);
        let has_path = requested_path.is_absolute() || requested_path.components().count() > 1;
        let executable = if has_path {
            let candidate = if requested_path.is_absolute() {
                requested_path.to_path_buf()
            } else {
                cwd.join(requested_path)
            };
            canonical_executable(&candidate).ok_or_else(|| {
                ExecutableResolutionError::Unavailable(format!(
                    "required executable '{}' is unavailable",
                    executable_display_name(requested)
                ))
            })?
        } else {
            find_executable_on_path(requested, env_map).ok_or_else(|| {
                ExecutableResolutionError::Unavailable(format!(
                    "required executable '{}' was not found on host PATH",
                    executable_display_name(requested)
                ))
            })?
        };
        if path_has_sensitive_component(&executable) {
            return Err(ExecutableResolutionError::NotPermitted(format!(
                "required executable '{}' is not permitted",
                executable_display_name(requested)
            )));
        }

        let mut read_roots = executable_read_roots(&executable);
        let resolved_argv = if is_batch_executable(&executable) {
            let shell = system_command_interpreter(env_map).ok_or_else(|| {
                ExecutableResolutionError::Unavailable(
                    "required Windows command interpreter is unavailable".to_string(),
                )
            })?;
            read_roots.extend(executable_read_roots(&shell));
            batch_argv(&shell, &executable, &argv[1..])
                .map_err(ExecutableResolutionError::Unsupported)?
        } else {
            let mut resolved = argv.to_vec();
            resolved[0] = executable.to_string_lossy().into_owned();
            resolved
        };
        read_roots.sort_by_key(|path| path.to_string_lossy().to_ascii_lowercase());
        read_roots.dedup_by(|left, right| {
            left.to_string_lossy()
                .eq_ignore_ascii_case(&right.to_string_lossy())
        });
        Ok(ResolvedExecutable {
            argv: resolved_argv,
            read_roots,
        })
    }

    fn is_batch_executable(path: &Path) -> bool {
        path.extension().is_some_and(|extension| {
            extension.eq_ignore_ascii_case("cmd") || extension.eq_ignore_ascii_case("bat")
        })
    }

    fn system_command_interpreter(env_map: &HashMap<String, String>) -> Option<PathBuf> {
        let system_root = PathBuf::from(env_value(env_map, "SystemRoot")?);
        if !system_root.is_absolute() {
            return None;
        }
        canonical_executable(&system_root.join("System32").join("cmd.exe"))
    }

    fn system_powershell(env_map: &HashMap<String, String>) -> Option<PathBuf> {
        let system_root = PathBuf::from(env_value(env_map, "SystemRoot")?);
        if !system_root.is_absolute() {
            return None;
        }
        canonical_executable(
            &system_root
                .join("System32")
                .join("WindowsPowerShell")
                .join("v1.0")
                .join("powershell.exe"),
        )
    }

    fn batch_argv(
        shell: &Path,
        script: &Path,
        arguments: &[String],
    ) -> Result<Vec<String>, String> {
        let script = script.to_string_lossy();
        if !batch_argument_is_safe(&script)
            || arguments.iter().any(|arg| !batch_argument_is_safe(arg))
        {
            return Err(UNSAFE_BATCH_ARGUMENT.to_string());
        }
        let mut command = format!("call {script}");
        for argument in arguments {
            command.push(' ');
            command.push_str(argument);
        }
        Ok(vec![
            shell.to_string_lossy().into_owned(),
            "/D".to_string(),
            "/V:OFF".to_string(),
            "/S".to_string(),
            "/C".to_string(),
            command,
        ])
    }

    fn batch_argument_is_safe(value: &str) -> bool {
        !value.chars().any(|character| {
            character.is_whitespace()
                || matches!(
                    character,
                    '\0' | '"' | '&' | '|' | '<' | '>' | '^' | '(' | ')' | '%' | '!'
                )
        })
    }

    fn find_executable_on_path(
        requested: &str,
        env_map: &HashMap<String, String>,
    ) -> Option<PathBuf> {
        let path = env_value(env_map, "PATH")?;
        let extensions = executable_extensions(requested, env_map);
        for directory in std::env::split_paths(OsStr::new(path)) {
            if !directory.is_absolute() {
                continue;
            }
            for extension in &extensions {
                let candidate = directory.join(format!("{requested}{extension}"));
                if let Some(executable) = canonical_executable(&candidate) {
                    return Some(executable);
                }
            }
        }
        None
    }

    fn executable_extensions(requested: &str, env_map: &HashMap<String, String>) -> Vec<String> {
        if Path::new(requested).extension().is_some() {
            return vec![String::new()];
        }
        env_value(env_map, "PATHEXT")
            .unwrap_or(".COM;.EXE;.BAT;.CMD")
            .split(';')
            .filter(|value| !value.is_empty())
            .map(|value| {
                if value.starts_with('.') {
                    value.to_string()
                } else {
                    format!(".{value}")
                }
            })
            .collect()
    }

    fn canonical_executable(path: &Path) -> Option<PathBuf> {
        if !path.is_file() {
            return None;
        }
        dunce::canonicalize(path).ok().filter(|path| path.is_file())
    }

    fn executable_read_roots(executable: &Path) -> Vec<PathBuf> {
        let mut roots = Vec::new();
        if let Some(parent) = executable.parent() {
            push_safe_read_root(&mut roots, parent);
            if parent.file_name().is_some_and(|name| {
                name.eq_ignore_ascii_case("bin") || name.eq_ignore_ascii_case("scripts")
            }) && let Some(install_root) = parent.parent()
                && !install_root
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with('.'))
            {
                push_safe_read_root(&mut roots, install_root);
            }
        }
        roots
    }

    fn push_safe_read_root(roots: &mut Vec<PathBuf>, path: &Path) {
        if !path.is_absolute() || path.parent().is_none() || path_has_sensitive_component(path) {
            return;
        }
        let Ok(canonical) = dunce::canonicalize(path) else {
            return;
        };
        if !canonical.is_dir()
            || canonical.parent().is_none()
            || path_has_sensitive_component(&canonical)
        {
            return;
        }
        if std::env::var_os(USER_PROFILE_ENV)
            .and_then(|profile| dunce::canonicalize(profile).ok())
            .is_some_and(|profile| canonical == profile)
        {
            return;
        }
        roots.push(canonical);
    }

    fn env_value<'a>(env_map: &'a HashMap<String, String>, name: &str) -> Option<&'a str> {
        env_map
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    fn executable_display_name(requested: &str) -> String {
        Path::new(requested)
            .file_name()
            .unwrap_or_else(|| OsStr::new("command"))
            .to_string_lossy()
            .into_owned()
    }

    fn sandbox_home() -> Result<PathBuf, String> {
        let home = std::env::var_os(SANDBOX_HOME_ENV)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os(USER_PROFILE_ENV)
                    .filter(|value| !value.is_empty())
                    .map(PathBuf::from)
                    .map(|profile| profile.join(DEFAULT_HOME_DIR_NAME))
            })
            .ok_or_else(|| {
                format!("{SANDBOX_HOME_ENV} and {USER_PROFILE_ENV} are both unavailable")
            })?;
        let home = if home.is_absolute() {
            home
        } else {
            std::env::current_dir()
                .map_err(|error| format!("failed to resolve sandbox home: {error}"))?
                .join(home)
        };
        std::fs::create_dir_all(&home).map_err(|error| {
            format!("failed to create sandbox home {}: {error}", home.display())
        })?;
        Ok(home)
    }

    fn child_environment(policy: &CommandEnvironmentPolicy) -> HashMap<String, String> {
        let mut env_map = filtered_child_environment(std::env::vars(), policy);
        if let Some(temp) = env_value(&env_map, "TEMP")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
        {
            let cache = temp.join("singularity-tool-cache");
            env_map.insert(
                "PIP_CACHE_DIR".to_string(),
                cache.join("pip").to_string_lossy().into_owned(),
            );
            env_map.insert(
                "NPM_CONFIG_CACHE".to_string(),
                cache.join("npm").to_string_lossy().into_owned(),
            );
        }
        env_map
    }

    fn filtered_child_environment(
        environment: impl IntoIterator<Item = (String, String)>,
        policy: &CommandEnvironmentPolicy,
    ) -> HashMap<String, String> {
        environment
            .into_iter()
            .filter(|(name, _)| !is_secret_env_name(name))
            .filter(|(name, _)| {
                policy != &CommandEnvironmentPolicy::EvaluationIsolated
                    || !is_evaluation_host_environment(name)
            })
            .collect()
    }

    fn is_evaluation_host_environment(name: &str) -> bool {
        let name = name.to_ascii_uppercase();
        name.starts_with("SINGULARITY_")
            || matches!(
                name.as_str(),
                "CARGO_TARGET_DIR"
                    | "CARGO_BUILD_TARGET"
                    | "CARGO_ENCODED_RUSTFLAGS"
                    | "RUSTFLAGS"
                    | "RUSTDOCFLAGS"
                    | "RUSTC_WRAPPER"
                    | "RUSTC_WORKSPACE_WRAPPER"
                    | "NODE_OPTIONS"
                    | "NODE_PATH"
                    | "PYTHONHOME"
                    | "PYTHONPATH"
                    | "VIRTUAL_ENV"
                    | "GOFLAGS"
                    | "GOWORK"
            )
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::CommandExecutionStatus;
        use std::fs;
        use std::io::Write;

        fn create_test_file(path: &Path, contents: &str) {
            let mut file = fs::File::create(path).expect("create test file");
            file.write_all(contents.as_bytes())
                .expect("write test file");
        }

        #[test]
        fn evaluation_environment_removes_host_build_overrides_but_keeps_tool_discovery() {
            let environment = [
                ("PATH".to_string(), "C:\\tools".to_string()),
                ("Pathext".to_string(), ".EXE;.CMD".to_string()),
                (
                    "cargo_target_dir".to_string(),
                    "D:\\host-target".to_string(),
                ),
                ("RUSTFLAGS".to_string(), "-C target-cpu=native".to_string()),
                ("NODE_OPTIONS".to_string(), "--require host.js".to_string()),
                ("PYTHONPATH".to_string(), "C:\\host-python".to_string()),
                ("GOFLAGS".to_string(), "-mod=vendor".to_string()),
                (
                    "SINGULARITY_MODEL".to_string(),
                    "provider-model".to_string(),
                ),
                ("SERVICE_API_KEY".to_string(), "secret".to_string()),
            ];

            let isolated = filtered_child_environment(
                environment.clone(),
                &CommandEnvironmentPolicy::EvaluationIsolated,
            );
            assert_eq!(env_value(&isolated, "PATH"), Some("C:\\tools"));
            assert_eq!(env_value(&isolated, "PATHEXT"), Some(".EXE;.CMD"));
            for removed in [
                "CARGO_TARGET_DIR",
                "RUSTFLAGS",
                "NODE_OPTIONS",
                "PYTHONPATH",
                "GOFLAGS",
                "SINGULARITY_MODEL",
                "SERVICE_API_KEY",
            ] {
                assert!(env_value(&isolated, removed).is_none(), "{removed} leaked");
            }

            let ordinary =
                filtered_child_environment(environment, &CommandEnvironmentPolicy::HostSanitized);
            assert_eq!(
                env_value(&ordinary, "CARGO_TARGET_DIR"),
                Some("D:\\host-target")
            );
            assert_eq!(
                env_value(&ordinary, "RUSTFLAGS"),
                Some("-C target-cpu=native")
            );
            assert!(env_value(&ordinary, "SERVICE_API_KEY").is_none());
        }

        #[test]
        fn resolver_uses_absolute_path_entries_and_pathext_order() {
            let temp = tempfile::tempdir().expect("tempdir");
            let first = temp.path().join("first");
            let second = temp.path().join("second");
            fs::create_dir_all(&first).expect("first");
            fs::create_dir_all(&second).expect("second");
            create_test_file(&first.join("runner"), "extensionless");
            create_test_file(&first.join("runner.CMD"), "first");
            create_test_file(&second.join("runner.EXE"), "second");
            let path = std::env::join_paths([&first, &second])
                .expect("join PATH")
                .to_string_lossy()
                .into_owned();
            let env = HashMap::from([
                ("Path".to_string(), path),
                ("PathExt".to_string(), ".CMD;.EXE".to_string()),
                (
                    "SystemRoot".to_string(),
                    std::env::var("SystemRoot").expect("SystemRoot"),
                ),
            ]);

            let resolved = resolve_executable(&["runner".to_string()], temp.path(), &env)
                .expect("resolve runner");

            assert!(resolved.argv[0].to_ascii_lowercase().ends_with("cmd.exe"));
            assert!(resolved.argv[5].contains("runner.CMD"));
            assert!(
                !resolved.read_roots.contains(
                    &dunce::canonicalize(second).expect("canonical unrelated PATH entry")
                )
            );
        }

        #[test]
        fn resolver_rejects_unsafe_batch_arguments() {
            let temp = tempfile::tempdir().expect("tempdir");
            let script = temp.path().join("runner.cmd");
            create_test_file(&script, "@exit /b 0");
            let env = HashMap::from([(
                "SystemRoot".to_string(),
                std::env::var("SystemRoot").expect("SystemRoot"),
            )]);

            let error = resolve_executable(
                &[
                    script.to_string_lossy().into_owned(),
                    "safe & unsafe".to_string(),
                ],
                temp.path(),
                &env,
            )
            .expect_err("unsafe batch argument must fail closed");

            let ExecutableResolutionError::Unsupported(message) = error else {
                panic!("unsafe batch arguments must be unsupported")
            };
            assert_eq!(message, UNSAFE_BATCH_ARGUMENT);
            assert!(!message.contains(&temp.path().to_string_lossy().to_string()));
        }

        #[test]
        fn resolver_skips_relative_path_entries() {
            let temp = tempfile::tempdir().expect("tempdir");
            create_test_file(&temp.path().join("runner.EXE"), "runner");
            let env = HashMap::from([
                ("PATH".to_string(), ".".to_string()),
                ("PATHEXT".to_string(), ".EXE".to_string()),
            ]);

            let error = resolve_executable(&["runner".to_string()], temp.path(), &env)
                .expect_err("relative PATH must be rejected");

            let ExecutableResolutionError::Unavailable(message) = error else {
                panic!("missing PATH executable must be unavailable")
            };
            assert_eq!(
                message,
                "required executable 'runner' was not found on host PATH"
            );
            assert!(!message.contains(&temp.path().to_string_lossy().to_string()));
        }

        #[test]
        fn resolver_rejects_sensitive_executable_paths() {
            let temp = tempfile::tempdir().expect("tempdir");
            let sensitive = temp.path().join(".ssh");
            fs::create_dir(&sensitive).expect("sensitive dir");
            let executable = sensitive.join("runner.exe");
            create_test_file(&executable, "runner");

            let error = resolve_executable(
                &[executable.to_string_lossy().into_owned()],
                temp.path(),
                &HashMap::new(),
            )
            .expect_err("sensitive executable must be rejected");

            assert_eq!(
                error,
                ExecutableResolutionError::NotPermitted(
                    "required executable 'runner.exe' is not permitted".to_string()
                )
            );
        }

        #[test]
        fn resolver_adds_conventional_toolchain_parent_as_read_only_root() {
            let temp = tempfile::tempdir().expect("tempdir");
            let toolchain = temp.path().join("runtime");
            let bin = toolchain.join("bin");
            fs::create_dir_all(&bin).expect("bin");
            let executable = bin.join("runner.exe");
            create_test_file(&executable, "runner");
            let env = HashMap::from([("PATH".to_string(), bin.to_string_lossy().into_owned())]);

            let resolved = resolve_executable(&["runner.exe".to_string()], temp.path(), &env)
                .expect("resolve runner");
            let canonical_toolchain = dunce::canonicalize(toolchain).expect("canonical toolchain");

            assert!(resolved.read_roots.contains(&canonical_toolchain));
        }

        #[test]
        fn resolver_does_not_expand_hidden_tool_home_parent() {
            let temp = tempfile::tempdir().expect("temp dir");
            let tool_home = temp.path().join(".cargo");
            let bin = tool_home.join("bin");
            fs::create_dir_all(&bin).expect("create bin");
            create_test_file(&bin.join("cargo.exe"), "cargo");
            let env = HashMap::from([("PATH".to_string(), bin.to_string_lossy().into_owned())]);

            let resolved = resolve_executable(&["cargo.exe".to_string()], temp.path(), &env)
                .expect("resolve executable");
            let canonical_bin = dunce::canonicalize(bin).expect("canonical bin");
            let canonical_tool_home = dunce::canonicalize(tool_home).expect("canonical tool home");

            assert!(resolved.read_roots.contains(&canonical_bin));
            assert!(!resolved.read_roots.contains(&canonical_tool_home));
        }

        #[test]
        fn read_root_rechecks_canonical_sensitive_target() {
            use std::os::windows::fs::symlink_dir;

            let temp = tempfile::tempdir().expect("temp dir");
            let sensitive = temp.path().join("secrets");
            let alias = temp.path().join("runtime");
            fs::create_dir(&sensitive).expect("create sensitive target");
            if symlink_dir(&sensitive, &alias).is_err() {
                return;
            }

            let mut roots = Vec::new();
            push_safe_read_root(&mut roots, &alias);

            assert!(roots.is_empty());
        }

        #[test]
        fn backend_error_summary_omits_resolved_process_paths() {
            let error = "CreateProcessAsUserW failed: 193 | cwd=C:\\workspace | cmd=D:\\tools\\runner.cmd --version";

            let summary = windows_error_summary(&error);

            assert_eq!(summary, "CreateProcessAsUserW failed: 193");
            assert!(!summary.contains("workspace"));
            assert!(!summary.contains("runner.cmd"));
        }

        #[test]
        fn admission_allows_external_argv0_but_denies_external_data_paths() {
            let workspace = tempfile::tempdir().expect("workspace");
            let external = tempfile::tempdir().expect("external");
            let executable = external.path().join("runner.exe");
            let data = external.path().join("data.txt");
            create_test_file(&executable, "runner");
            create_test_file(&data, "data");
            let allowed = CommandRequest::project_verification(
                "external_executable",
                vec![executable.to_string_lossy().into_owned()],
                workspace.path().to_string_lossy(),
                workspace.path().to_string_lossy(),
            );
            let denied = CommandRequest::project_verification(
                "external_data",
                vec![
                    executable.to_string_lossy().into_owned(),
                    data.to_string_lossy().into_owned(),
                ],
                workspace.path().to_string_lossy(),
                workspace.path().to_string_lossy(),
            );

            assert!(command_request_denial(&allowed).is_none());
            assert_eq!(
                command_request_denial(&denied)
                    .expect("external data must be denied")
                    .execution_status,
                CommandExecutionStatus::PolicyDenied
            );
        }

        #[test]
        fn model_script_with_shell_syntax_and_sensitive_text_reaches_backend_boundary() {
            let workspace = tempfile::tempdir().expect("workspace");
            let request = CommandScriptRequest::agent_requested(
                "script_boundary",
                r#"Write-Output 'C:\Users\runner\.ssh\id_rsa' & echo ready > NUL"#,
                workspace.path().to_string_lossy(),
                workspace.path().to_string_lossy(),
            );

            assert!(command_script_request_denial(&request).is_none());
        }

        #[test]
        fn existing_protected_path_disables_restricted_token_fallback() {
            let workspace = tempfile::tempdir().expect("workspace");
            let nested = workspace.path().join("nested");
            fs::create_dir_all(&nested).expect("nested directory");
            fs::create_dir(workspace.path().join(".git")).expect("git directory");
            create_test_file(&workspace.path().join(".env.local"), "opaque");
            create_test_file(&nested.join("private-key.pem"), "opaque");
            create_test_file(&nested.join("client.p12"), "opaque");
            let mut request = CommandScriptRequest::agent_requested(
                "script_protected_path",
                "Join-Path 'nested' (Get-Random) | Out-Null",
                workspace.path().to_string_lossy(),
                workspace.path().to_string_lossy(),
            );
            request.network.mode = SandboxNetworkMode::Allowed;

            let prepared = match PreparedCommand::from_script_request(&request) {
                Ok(prepared) => prepared,
                Err(_) => panic!("script preparation should succeed"),
            };

            assert!(!prepared.restricted_token_fallback);
            let protected_paths = prepared
                .protected_deny_read_paths
                .iter()
                .map(|path| path.to_path_buf())
                .collect::<std::collections::HashSet<_>>();
            assert_eq!(
                protected_paths,
                [
                    workspace.path().join(".git"),
                    workspace.path().join(".env.local"),
                    nested.join("private-key.pem"),
                    nested.join("client.p12"),
                ]
                .into_iter()
                .map(|path| dunce::canonicalize(path).expect("canonical protected path"))
                .collect()
            );
            let elevated =
                elevated_capture_request(&prepared, WindowsSandboxCancellationToken::new(|| false));
            assert_eq!(
                elevated.deny_read_paths_override,
                prepared.protected_deny_read_paths.as_slice()
            );
        }

        #[test]
        fn model_script_cancellation_is_typed_before_backend_execution() {
            let workspace = tempfile::tempdir().expect("workspace");
            let request = CommandScriptRequest::agent_requested(
                "script_cancelled",
                "Write-Output cancelled",
                workspace.path().to_string_lossy(),
                workspace.path().to_string_lossy(),
            );
            let cancellation = CancellationToken::new();
            cancellation.cancel();

            let result =
                WindowsSandboxBackend::new().execute_script_cancellable(&request, &cancellation);

            assert_eq!(result.execution_status, CommandExecutionStatus::Cancelled);
            assert_eq!(result.semantic_status, CommandSemanticStatus::Cancelled);
            assert_eq!(result.exit_code, None);
            assert_eq!(result.sandbox.backend, BACKEND_NAME);
            assert_eq!(
                result.sandbox.enforcement,
                SandboxBackendEnforcement::Strict
            );
        }

        #[test]
        fn interrupted_capture_maps_to_typed_cancel_and_timeout_results() {
            let cases = [
                (
                    true,
                    false,
                    CommandExecutionStatus::Cancelled,
                    CommandSemanticStatus::Cancelled,
                    "cancelled",
                ),
                (
                    false,
                    true,
                    CommandExecutionStatus::TimedOut,
                    CommandSemanticStatus::TimedOut,
                    "timed out",
                ),
            ];

            for (index, (cancelled, timed_out, execution, semantic, message)) in
                cases.into_iter().enumerate()
            {
                let result = command_result_from_capture(
                    &format!("script_interrupt_{index}"),
                    singularity_windows_sandbox::CaptureResult {
                        exit_code: 1,
                        stdout: Vec::new(),
                        stderr: Vec::new(),
                        timed_out,
                        cancelled,
                        output_truncated: false,
                    },
                    Instant::now(),
                );

                assert_eq!(result.execution_status, execution);
                assert_eq!(result.semantic_status, semantic);
                assert_eq!(result.exit_code, None);
                assert_eq!(result.timed_out, timed_out);
                assert!(result.stderr_preview.contains(message));
            }
        }
    }
}
