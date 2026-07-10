#![deny(unsafe_op_in_unsafe_fn)]

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use singularity_core::contains_sensitive_text;

pub const DEFAULT_COMMAND_TIMEOUT_SECONDS: u64 = 30;
pub const DEFAULT_MAX_OUTPUT_CHARS: usize = 40_000;
const SANDBOX_BACKEND_UNAVAILABLE: &str = "sandbox-required command has no sandbox backend";
const COMMAND_SPAWN_FAILED: &str = "sandbox command spawn failed";
const COMMAND_TIMED_OUT: &str = "sandbox command timed out";
const COMMAND_CANCELLED: &str = "sandbox command cancelled";
const COMMAND_EMPTY_ARGV: &str = "sandbox command argv is empty";
const COMMAND_CWD_OUTSIDE_WORKSPACE: &str = "sandbox command cwd is outside workspace";
const COMMAND_CWD_UNAVAILABLE: &str = "sandbox command cwd is unavailable";
const COMMAND_PATH_OUTSIDE_WORKSPACE: &str = "sandbox command path is outside workspace";
const COMMAND_READ_ONLY_WRITE_DENIED: &str = "sandbox command write denied in read-only mode";
const COMMAND_SENSITIVE_PATH_DENIED: &str = "sandbox command path denied";
const COMMAND_ENV_PATH_UNSUPPORTED: &str = "sandbox command env-expanded path is unsupported";
const COMMAND_UNSUPPORTED: &str = "sandbox command mode unsupported";
const PATH_ENV_NAME: &str = "PATH";
const SHELL_COMMAND_FLAGS: [&str; 3] = ["/c", "-c", "-command"];
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
const GIT_STATUS_ARGS: [&str; 3] = ["git", "status", "--porcelain=v1"];
const GIT_DIFF_ARGS: [&str; 3] = ["git", "diff", "--"];
const REDACTED_COMMAND_OUTPUT: &str = "[redacted sensitive command output]";
const SECRET_ENV_MARKERS: [&str; 6] = [
    "API_KEY",
    "AUTH",
    "CREDENTIAL",
    "PASSWORD",
    "SECRET",
    "TOKEN",
];
const SENSITIVE_PATH_EXACT_MARKERS: [&str; 12] = [
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
    "secrets",
];
const SENSITIVE_PATH_PREFIXES: [&str; 3] = [".env", "credential", "private-key"];
const SENSITIVE_PATH_SUFFIXES: [&str; 4] = [".key", ".pem", ".p12", ".pfx"];

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
    ReadOnly,
    WorkspaceWrite,
    DangerFullAccess,
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
                mode: SandboxFilesystemMode::WorkspaceWrite,
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
    Unsupported,
    SpawnFailed,
    TimedOut,
    Cancelled,
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
    Unsupported,
    TimedOut,
    Cancelled,
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
                mode: SandboxFilesystemMode::WorkspaceWrite,
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
        command_permission_resource(&self.argv)
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
#[serde(rename_all = "snake_case")]
pub enum SandboxBackendEnforcement {
    Strict,
    RestrictedToken,
    Unavailable,
}

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
    pub changed_files: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct BoundedCommandOutput {
    pub preview: String,
    pub truncated: bool,
}

pub fn bound_command_output(output: &str, max_chars: usize) -> BoundedCommandOutput {
    let preview = output.chars().take(max_chars).collect::<String>();
    let truncated = output.chars().count() > preview.chars().count();
    BoundedCommandOutput { preview, truncated }
}

pub fn redacted_child_env(env: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    env.iter()
        .filter(|(name, _value)| {
            name.eq_ignore_ascii_case(PATH_ENV_NAME) || !is_secret_env_name(name)
        })
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect()
}

pub fn changed_files_inside_workspace(
    workspace_root: impl AsRef<Path>,
    changed_paths: &[String],
) -> Vec<String> {
    let workspace = normalize_path(workspace_root.as_ref());
    changed_paths
        .iter()
        .filter_map(|path| {
            let normalized = normalize_path(Path::new(path));
            normalized
                .strip_prefix(&workspace)
                .ok()
                .map(|relative| relative.to_string_lossy().replace('\\', "/"))
                .filter(|relative| !relative.is_empty())
        })
        .collect()
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

    pub fn spawn_failed(command_id: impl Into<String>, reason: impl Into<String>) -> Self {
        Self::blocked(
            command_id,
            CommandExecutionStatus::SpawnFailed,
            CommandSemanticStatus::PolicyBlocked,
            format!("{}: {}", COMMAND_SPAWN_FAILED, reason.into()),
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
            changed_files: Vec::new(),
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
            changed_files: Vec::new(),
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

    pub fn supports_command_execution(&self) -> bool {
        self.env_isolation
            && self.restricted_token
            && self.job_object
            && self.path_admission
            && self.process_tree_kill
            && self.timeout
            && self.output_limit
    }

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

pub trait SandboxBackend {
    fn name(&self) -> &'static str;
    fn capabilities(&self) -> SandboxCapabilities;
    fn execute(&self, request: &CommandRequest) -> CommandResult;
}

#[derive(Debug, Clone)]
pub struct UnavailableSandboxBackend;

impl SandboxBackend for UnavailableSandboxBackend {
    fn name(&self) -> &'static str {
        "unavailable"
    }

    fn capabilities(&self) -> SandboxCapabilities {
        SandboxCapabilities::unavailable()
    }

    fn execute(&self, request: &CommandRequest) -> CommandResult {
        CommandResult::sandbox_backend_unavailable(&request.command_id)
            .with_sandbox_execution(self.name(), SandboxBackendEnforcement::Unavailable)
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

fn command_reference_tokens(request: &CommandRequest) -> Vec<String> {
    let mut tokens = Vec::new();
    for part in &request.argv {
        collect_command_tokens(part, &mut tokens);
    }
    collect_command_tokens(&command_permission_resource(&request.argv), &mut tokens);
    tokens
}

fn collect_command_tokens(value: &str, tokens: &mut Vec<String>) {
    tokens.extend(
        value
            .split(command_token_separator)
            .map(clean_command_token)
            .filter(|token| !token.is_empty()),
    );
}

fn command_token_separator(ch: char) -> bool {
    ch.is_whitespace() || matches!(ch, '&' | '|' | ';' | '<' | '>')
}

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

fn command_token_has_env_reference(token: &str) -> bool {
    let lower = token.to_ascii_lowercase();
    token.contains('%')
        || lower.contains("$env:")
        || lower.contains("${")
        || lower.contains("$home")
        || lower.contains("$userprofile")
}

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
        .any(|component| sensitive_path_component(&component))
}

fn command_has_read_only_write_intent(request: &CommandRequest) -> bool {
    let resource = command_permission_resource(&request.argv);
    command_has_file_redirection(&resource)
        || command_reference_tokens(request).iter().any(|token| {
            let lower = token.to_ascii_lowercase();
            WRITE_COMMAND_WORDS
                .iter()
                .any(|write_command| lower == *write_command)
        })
}

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

fn redirection_target_is_non_file(target: &str) -> bool {
    let lower = target.to_ascii_lowercase();
    matches!(lower.as_str(), "nul" | "nul:" | "/dev/null" | "&1" | "&2")
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

fn is_secret_env_name(name: &str) -> bool {
    let upper_name = name.to_ascii_uppercase();
    SECRET_ENV_MARKERS
        .iter()
        .any(|marker| upper_name.contains(marker))
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
    for path in request
        .filesystem
        .writable_paths
        .iter()
        .chain(request.filesystem.readonly_paths.iter())
    {
        let resolved = resolve_existing_or_parent_command_path(&workspace, path);
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

fn resolve_command_path(workspace: &Path, path: &str) -> PathBuf {
    let candidate = Path::new(path);
    let joined = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        workspace.join(candidate)
    };
    normalize_path(&joined)
}

fn resolve_existing_command_path(workspace: &Path, path: &str) -> Option<PathBuf> {
    let resolved = resolve_command_path(workspace, path);
    std::fs::canonicalize(&resolved)
        .map(|path| normalize_path(&path))
        .ok()
}

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

fn path_has_sensitive_component(path: &Path) -> bool {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(value) => Some(value.to_string_lossy().to_ascii_lowercase()),
            _ => None,
        })
        .any(|component| sensitive_path_component(&component))
}

fn sensitive_path_component(component: &str) -> bool {
    SENSITIVE_PATH_EXACT_MARKERS.contains(&component)
        || SENSITIVE_PATH_PREFIXES.iter().any(|prefix| {
            component == *prefix
                || component
                    .strip_prefix(*prefix)
                    .is_some_and(|suffix| suffix.starts_with('.'))
        })
        || SENSITIVE_PATH_SUFFIXES
            .iter()
            .any(|suffix| component.ends_with(suffix))
        || component.contains("secret")
}

#[cfg(windows)]
mod windows_backend {
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::time::Instant;

    use singularity_windows_sandbox::{
        AbsolutePathBuf, ElevatedSandboxProfileCaptureRequest, FileSystemSandboxPolicy,
        ManagedFileSystemPermissions, NetworkSandboxPolicy, PermissionProfile,
        run_windows_sandbox_capture, run_windows_sandbox_capture_for_permission_profile_elevated,
    };

    use super::{
        CommandRequest, CommandResult, SandboxBackend, SandboxBackendEnforcement,
        SandboxCapabilities, SandboxFilesystemMode, SandboxNetworkMode, command_request_denial,
        is_secret_env_name,
    };

    const BACKEND_NAME: &str = "windows";
    const ELEVATED_BACKEND_NAME: &str = "windows_elevated";
    const RESTRICTED_TOKEN_BACKEND_NAME: &str = "windows_restricted_token";
    const SANDBOX_HOME_ENV: &str = "SINGULARITY_HOME";
    const USER_PROFILE_ENV: &str = "USERPROFILE";
    const DEFAULT_HOME_DIR_NAME: &str = ".singularity";
    const ELEVATED_FAILURE_PREFIX: &str = "elevated Windows sandbox failed";
    const RESTRICTED_FAILURE_PREFIX: &str = "restricted-token Windows sandbox failed";
    const CUSTOM_ROOTS_UNSUPPORTED: &str =
        "custom writable_paths and readonly_paths are not supported by the Windows sandbox adapter";
    const DANGER_FULL_ACCESS_UNSUPPORTED: &str = "danger-full-access requires an explicit unsandboxed executor and is unavailable in the sandbox backend";

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
            if let Some(denied) = command_request_denial(request) {
                return denied
                    .with_sandbox_execution(self.name(), SandboxBackendEnforcement::Unavailable);
            }
            match execute_windows_sandbox(request) {
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
    }

    impl PreparedCommand {
        fn from_request(request: &CommandRequest) -> Result<Self, String> {
            if !request.filesystem.writable_paths.is_empty()
                || !request.filesystem.readonly_paths.is_empty()
            {
                return Err(CUSTOM_ROOTS_UNSUPPORTED.to_string());
            }
            let workspace_root =
                canonical_directory(Path::new(&request.filesystem.workspace_root))?;
            let cwd = canonical_directory(Path::new(&request.cwd))?;
            let workspace_root = AbsolutePathBuf::from_absolute_path_checked(&workspace_root)
                .map_err(|error| format!("invalid workspace root: {error}"))?;
            let workspace_roots = vec![workspace_root];
            let network = match request.network.mode {
                SandboxNetworkMode::Denied => NetworkSandboxPolicy::Restricted,
                SandboxNetworkMode::Allowed => NetworkSandboxPolicy::Enabled,
                SandboxNetworkMode::Allowlist => {
                    return Err("network allowlist mode is unsupported".to_string());
                }
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
                    return Err(DANGER_FULL_ACCESS_UNSUPPORTED.to_string());
                }
            };
            let restricted_token_fallback = singularity_windows_sandbox::ResolvedWindowsSandboxPermissions::try_from_permission_profile_for_workspace_roots(
                &permission_profile,
                &workspace_roots,
            )
            .map_err(|error| format!("invalid Windows sandbox permissions: {error}"))?
            .supports_restricted_token_fallback();
            Ok(Self {
                permission_profile,
                workspace_roots,
                sandbox_home: sandbox_home()?,
                cwd,
                env_map: child_environment(),
                timeout_ms: request.timeout_seconds.saturating_mul(1_000),
                restricted_token_fallback,
            })
        }
    }

    fn execute_windows_sandbox(request: &CommandRequest) -> Result<CommandResult, String> {
        let prepared = PreparedCommand::from_request(request)?;
        let started = Instant::now();
        let elevated = run_windows_sandbox_capture_for_permission_profile_elevated({
            let mut elevated = ElevatedSandboxProfileCaptureRequest::new(
                &prepared.permission_profile,
                &prepared.workspace_roots,
                &prepared.sandbox_home,
                request.argv.clone(),
                &prepared.cwd,
                prepared.env_map.clone(),
            );
            elevated.timeout_ms = Some(prepared.timeout_ms);
            elevated
        });
        match elevated {
            Ok(capture) => Ok(command_result_from_capture(request, capture, started)
                .with_sandbox_execution(ELEVATED_BACKEND_NAME, SandboxBackendEnforcement::Strict)),
            Err(elevated_error) if prepared.restricted_token_fallback => {
                let capture = run_windows_sandbox_capture(
                    &prepared.permission_profile,
                    &prepared.workspace_roots,
                    &prepared.sandbox_home,
                    request.argv.clone(),
                    &prepared.cwd,
                    prepared.env_map,
                    Some(prepared.timeout_ms),
                    None,
                    true,
                )
                .map_err(|restricted_error| {
                    format!(
                        "{ELEVATED_FAILURE_PREFIX}: {elevated_error:#}; {RESTRICTED_FAILURE_PREFIX}: {restricted_error:#}"
                    )
                })?;
                Ok(
                    command_result_from_capture(request, capture, started).with_sandbox_execution(
                        RESTRICTED_TOKEN_BACKEND_NAME,
                        SandboxBackendEnforcement::RestrictedToken,
                    ),
                )
            }
            Err(error) => Err(format!("{ELEVATED_FAILURE_PREFIX}: {error:#}")),
        }
    }

    fn command_result_from_capture(
        request: &CommandRequest,
        capture: singularity_windows_sandbox::CaptureResult,
        started: Instant,
    ) -> CommandResult {
        let duration_ms = started.elapsed().as_millis().try_into().unwrap_or(u64::MAX);
        if capture.cancelled {
            return CommandResult::cancelled(&request.command_id, duration_ms);
        }
        if capture.timed_out {
            return CommandResult::timed_out(&request.command_id, duration_ms);
        }
        CommandResult::executed(
            &request.command_id,
            capture.exit_code,
            duration_ms,
            String::from_utf8_lossy(&capture.stdout),
            String::from_utf8_lossy(&capture.stderr),
            capture.output_truncated,
        )
    }

    fn canonical_directory(path: &Path) -> Result<PathBuf, String> {
        let canonical = std::fs::canonicalize(path)
            .map_err(|error| format!("failed to resolve {}: {error}", path.display()))?;
        if !canonical.is_dir() {
            return Err(format!("path is not a directory: {}", path.display()));
        }
        Ok(canonical)
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

    fn child_environment() -> HashMap<String, String> {
        std::env::vars()
            .filter(|(name, _)| !is_secret_env_name(name))
            .collect()
    }
}
