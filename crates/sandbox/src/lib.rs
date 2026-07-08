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
const COMMAND_EMPTY_ARGV: &str = "sandbox command argv is empty";
const COMMAND_CWD_OUTSIDE_WORKSPACE: &str = "sandbox command cwd is outside workspace";
const COMMAND_CWD_UNAVAILABLE: &str = "sandbox command cwd is unavailable";
const COMMAND_PATH_OUTSIDE_WORKSPACE: &str = "sandbox command path is outside workspace";
const COMMAND_READ_ONLY_WRITE_DENIED: &str = "sandbox command write denied in read-only mode";
const COMMAND_SENSITIVE_PATH_DENIED: &str = "sandbox command path denied";
const COMMAND_ENV_PATH_UNSUPPORTED: &str = "sandbox command env-expanded path is unsupported";
const COMMAND_NETWORK_UNSUPPORTED: &str = "sandbox command network mode is unsupported";
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
            changed_files: Vec::new(),
        }
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

    pub fn supports_strict_command_execution(&self) -> bool {
        self.env_isolation
            && self.restricted_token
            && self.job_object
            && self.path_admission
            && self.process_tree_kill
            && self.timeout
            && self.output_limit
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
    }
}

#[cfg(windows)]
pub use windows_restricted_token::WindowsRestrictedTokenSandboxBackend;

#[cfg(not(windows))]
#[derive(Debug, Clone, Default)]
pub struct WindowsRestrictedTokenSandboxBackend;

#[cfg(not(windows))]
impl WindowsRestrictedTokenSandboxBackend {
    pub fn new() -> Self {
        Self
    }
}

#[cfg(not(windows))]
impl SandboxBackend for WindowsRestrictedTokenSandboxBackend {
    fn name(&self) -> &'static str {
        "windows_restricted_token"
    }

    fn capabilities(&self) -> SandboxCapabilities {
        SandboxCapabilities::unavailable()
    }

    fn execute(&self, request: &CommandRequest) -> CommandResult {
        CommandResult::sandbox_backend_unavailable(&request.command_id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SandboxBackendEnforcement {
    Strict,
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
    if request.network.mode == SandboxNetworkMode::Allowlist
        || request.network.require_hard_isolation
    {
        return Some(CommandResult::unsupported(
            &request.command_id,
            COMMAND_NETWORK_UNSUPPORTED,
        ));
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
    SENSITIVE_PATH_EXACT_MARKERS
        .iter()
        .any(|marker| component == *marker)
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
mod windows_restricted_token {
    use std::ffi::{OsStr, c_void};
    use std::mem::{size_of, size_of_val};
    use std::os::windows::ffi::OsStrExt;
    use std::path::{Path, PathBuf};
    use std::ptr::{null, null_mut};
    use std::time::Instant;

    use windows_sys::Win32::Foundation::{
        CloseHandle, FALSE, GetLastError, HANDLE, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, TRUE,
        WAIT_OBJECT_0, WAIT_TIMEOUT,
    };
    use windows_sys::Win32::Security::{
        AllocateAndInitializeSid, CreateRestrictedToken, DISABLE_MAX_PRIVILEGE, FreeSid, PSID,
        SECURITY_ATTRIBUTES, SECURITY_MANDATORY_LABEL_AUTHORITY, SID_IDENTIFIER_AUTHORITY,
        SetTokenInformation, TOKEN_ADJUST_DEFAULT, TOKEN_ADJUST_SESSIONID, TOKEN_ASSIGN_PRIMARY,
        TOKEN_DUPLICATE, TOKEN_MANDATORY_LABEL, TOKEN_QUERY, TokenIntegrityLevel,
    };
    use windows_sys::Win32::Storage::FileSystem::ReadFile;
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject, TerminateJobObject,
    };
    use windows_sys::Win32::System::Pipes::{CreatePipe, PeekNamedPipe};
    use windows_sys::Win32::System::SystemServices::{
        SE_GROUP_INTEGRITY, SECURITY_MANDATORY_LOW_RID,
    };
    use windows_sys::Win32::System::Threading::{
        CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessAsUserW,
        CreateProcessWithTokenW, DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT,
        GetCurrentProcess, GetExitCodeProcess, InitializeProcThreadAttributeList,
        LPPROC_THREAD_ATTRIBUTE_LIST, OpenProcessToken, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
        PROCESS_INFORMATION, ResumeThread, STARTF_USESTDHANDLES, STARTUPINFOEXW, STARTUPINFOW,
        TerminateProcess, UpdateProcThreadAttribute, WaitForSingleObject,
    };

    use super::{
        CommandRequest, CommandResult, DEFAULT_MAX_OUTPUT_CHARS, SandboxBackend,
        SandboxCapabilities, SandboxFilesystemMode, command_request_denial, is_secret_env_name,
        normalize_path,
    };

    const WINDOWS_VERBATIM_PREFIX: &str = r"\\?\";
    const WINDOWS_VERBATIM_UNC_PREFIX: &str = r"\\?\UNC\";
    const POLL_INTERVAL_MS: u32 = 25;
    const STDIO_PIPE_BUFFER_BYTES: u32 = 64 * 1024;
    const READ_BUFFER_BYTES: usize = 8 * 1024;
    const CAPTURE_BYTE_LIMIT: usize = DEFAULT_MAX_OUTPUT_CHARS * 4;
    const TIMEOUT_JOB_EXIT_CODE: u32 = 1;
    const ERROR_PRIVILEGE_NOT_HELD: u32 = 1314;
    const RESUME_THREAD_FAILED: u32 = u32::MAX;
    const PROC_THREAD_ATTRIBUTE_COUNT: u32 = 1;

    #[derive(Debug, Clone, Default)]
    pub struct WindowsRestrictedTokenSandboxBackend;

    impl WindowsRestrictedTokenSandboxBackend {
        pub fn new() -> Self {
            Self
        }
    }

    impl SandboxBackend for WindowsRestrictedTokenSandboxBackend {
        fn name(&self) -> &'static str {
            "windows_restricted_token"
        }

        fn capabilities(&self) -> SandboxCapabilities {
            SandboxCapabilities::strict()
        }

        fn execute(&self, request: &CommandRequest) -> CommandResult {
            if let Some(denied) = command_request_denial(request) {
                return denied;
            }
            match run_restricted(request) {
                Ok(result) => result,
                Err(error) => {
                    CommandResult::spawn_failed(&request.command_id, error.safe_message())
                }
            }
        }
    }

    #[derive(Debug)]
    struct WindowsError {
        operation: &'static str,
        code: u32,
    }

    impl WindowsError {
        fn last(operation: &'static str) -> Self {
            Self {
                operation,
                code: last_error_code(),
            }
        }

        fn safe_message(&self) -> String {
            format!("{} failed with Windows error {}", self.operation, self.code)
        }
    }

    struct Handle(HANDLE);

    impl Handle {
        fn new(handle: HANDLE) -> Result<Self, WindowsError> {
            if handle.is_null() || handle == INVALID_HANDLE_VALUE {
                Err(WindowsError::last("handle"))
            } else {
                Ok(Self(handle))
            }
        }

        fn raw(&self) -> HANDLE {
            self.0
        }
    }

    impl Drop for Handle {
        fn drop(&mut self) {
            if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
                // SAFETY: Handle owns a valid Windows HANDLE and drops it exactly once.
                unsafe {
                    CloseHandle(self.0);
                }
                self.0 = null_mut();
            }
        }
    }

    struct ProcThreadAttributeList {
        buffer: Vec<u8>,
    }

    impl ProcThreadAttributeList {
        fn new_inherited_handles(handles: &[HANDLE]) -> Result<Self, WindowsError> {
            let mut bytes = 0usize;
            // SAFETY: The null first call is the documented way to query required buffer size.
            unsafe {
                InitializeProcThreadAttributeList(
                    null_mut(),
                    PROC_THREAD_ATTRIBUTE_COUNT,
                    0,
                    &mut bytes,
                );
            }
            if bytes == 0 {
                return Err(WindowsError::last("InitializeProcThreadAttributeList"));
            }
            let mut buffer = vec![0u8; bytes];
            let raw = buffer.as_mut_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST;
            // SAFETY: raw points to a writable buffer of the size returned by the sizing call.
            let initialized = unsafe {
                InitializeProcThreadAttributeList(raw, PROC_THREAD_ATTRIBUTE_COUNT, 0, &mut bytes)
            };
            if initialized == FALSE {
                return Err(WindowsError::last("InitializeProcThreadAttributeList"));
            }
            let mut list = Self { buffer };
            // SAFETY: list is initialized; handles points to the explicit inherited handle list.
            let updated = unsafe {
                UpdateProcThreadAttribute(
                    list.raw(),
                    0,
                    PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                    handles.as_ptr() as *const c_void,
                    size_of_val(handles),
                    null_mut(),
                    null(),
                )
            };
            if updated == FALSE {
                return Err(WindowsError::last("UpdateProcThreadAttribute"));
            }
            Ok(list)
        }

        fn raw(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
            self.buffer.as_mut_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST
        }
    }

    impl Drop for ProcThreadAttributeList {
        fn drop(&mut self) {
            // SAFETY: The attribute list was initialized by InitializeProcThreadAttributeList.
            unsafe {
                DeleteProcThreadAttributeList(self.raw());
            }
        }
    }

    fn run_restricted(request: &CommandRequest) -> Result<CommandResult, WindowsError> {
        let start = Instant::now();
        let token = restricted_primary_token(matches!(
            request.filesystem.mode,
            SandboxFilesystemMode::ReadOnly
        ))?;
        let job = kill_on_close_job()?;
        let (stdout_read, stdout_write) = output_pipe()?;
        let (stderr_read, stderr_write) = output_pipe()?;
        let mut command_line = command_line(&request.argv);
        let mut environment = environment_block();
        let cwd = command_cwd(request);
        let cwd_wide = wide_null(cwd.as_os_str());
        let inherited_handles = [stdout_write.raw(), stderr_write.raw()];
        let mut handle_list = ProcThreadAttributeList::new_inherited_handles(&inherited_handles)?;
        let mut startup = STARTUPINFOEXW {
            StartupInfo: STARTUPINFOW {
                cb: size_of::<STARTUPINFOEXW>() as u32,
                dwFlags: STARTF_USESTDHANDLES,
                hStdOutput: stdout_write.raw(),
                hStdError: stderr_write.raw(),
                ..STARTUPINFOW::default()
            },
            lpAttributeList: handle_list.raw(),
        };
        let mut process_information = PROCESS_INFORMATION::default();
        create_process(
            token.raw(),
            &mut command_line,
            &mut environment,
            cwd_wide.as_ptr(),
            &mut startup,
            &mut process_information,
        )?;
        let process = Handle::new(process_information.hProcess)?;
        let thread = Handle::new(process_information.hThread)?;
        drop(stdout_write);
        drop(stderr_write);
        if let Err(error) = assign_to_job(&job, &process) {
            terminate_process(&process);
            return Err(error);
        }
        if let Err(error) = resume_thread(&thread) {
            terminate_job(&job);
            return Err(error);
        }
        let wait = wait_for_process(request, &process, &job, stdout_read, stderr_read, start);
        wait
    }

    fn restricted_primary_token(read_only: bool) -> Result<Handle, WindowsError> {
        let mut current_token: HANDLE = null_mut();
        let access = TOKEN_ASSIGN_PRIMARY
            | TOKEN_DUPLICATE
            | TOKEN_QUERY
            | TOKEN_ADJUST_DEFAULT
            | TOKEN_ADJUST_SESSIONID;
        // SAFETY: GetCurrentProcess returns a pseudo-handle; current_token is a valid out pointer.
        let opened = unsafe { OpenProcessToken(GetCurrentProcess(), access, &mut current_token) };
        if opened == FALSE {
            return Err(WindowsError::last("OpenProcessToken"));
        }
        let current_token = Handle::new(current_token)?;
        let mut restricted_token: HANDLE = null_mut();
        // SAFETY: All optional SID/privilege arrays are null with zero counts as required by
        // CreateRestrictedToken. restricted_token is a valid out pointer.
        let created = unsafe {
            CreateRestrictedToken(
                current_token.raw(),
                DISABLE_MAX_PRIVILEGE,
                0,
                null(),
                0,
                null(),
                0,
                null(),
                &mut restricted_token,
            )
        };
        if created == FALSE {
            return Err(WindowsError::last("CreateRestrictedToken"));
        }
        let restricted_token = Handle::new(restricted_token)?;
        if read_only {
            set_low_integrity(&restricted_token)?;
        }
        Ok(restricted_token)
    }

    struct Sid(PSID);

    impl Sid {
        fn low_integrity() -> Result<Self, WindowsError> {
            let mut sid: PSID = null_mut();
            let authority: SID_IDENTIFIER_AUTHORITY = SECURITY_MANDATORY_LABEL_AUTHORITY;
            // SAFETY: authority is the documented mandatory label authority; sid is a valid out pointer.
            let allocated = unsafe {
                AllocateAndInitializeSid(
                    &authority,
                    1,
                    SECURITY_MANDATORY_LOW_RID as u32,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    &mut sid,
                )
            };
            if allocated == FALSE {
                Err(WindowsError::last("AllocateAndInitializeSid"))
            } else {
                Ok(Self(sid))
            }
        }

        fn raw(&self) -> PSID {
            self.0
        }
    }

    impl Drop for Sid {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: Sid owns memory allocated by AllocateAndInitializeSid.
                unsafe {
                    FreeSid(self.0);
                }
                self.0 = null_mut();
            }
        }
    }

    fn set_low_integrity(token: &Handle) -> Result<(), WindowsError> {
        let sid = Sid::low_integrity()?;
        let label = TOKEN_MANDATORY_LABEL {
            Label: windows_sys::Win32::Security::SID_AND_ATTRIBUTES {
                Sid: sid.raw(),
                Attributes: SE_GROUP_INTEGRITY as u32,
            },
        };
        // SAFETY: token is a restricted token handle; label points to a valid mandatory label SID
        // for the duration of the call.
        let applied = unsafe {
            SetTokenInformation(
                token.raw(),
                TokenIntegrityLevel,
                &label as *const _ as *const c_void,
                size_of::<TOKEN_MANDATORY_LABEL>() as u32,
            )
        };
        if applied == FALSE {
            Err(WindowsError::last("SetTokenInformation"))
        } else {
            Ok(())
        }
    }

    fn kill_on_close_job() -> Result<Handle, WindowsError> {
        // SAFETY: Null attributes/name request an unnamed job with default security.
        let job = unsafe { CreateJobObjectW(null(), null()) };
        let job = Handle::new(job)?;
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: limits points to the documented struct for JobObjectExtendedLimitInformation.
        let applied = unsafe {
            SetInformationJobObject(
                job.raw(),
                JobObjectExtendedLimitInformation,
                &limits as *const _ as *const _,
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if applied == FALSE {
            return Err(WindowsError::last("SetInformationJobObject"));
        }
        Ok(job)
    }

    fn output_pipe() -> Result<(Handle, Handle), WindowsError> {
        let mut read: HANDLE = null_mut();
        let mut write: HANDLE = null_mut();
        let attributes = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: null_mut(),
            bInheritHandle: TRUE,
        };
        // SAFETY: read/write are valid out pointers and attributes lives through the call.
        let created =
            unsafe { CreatePipe(&mut read, &mut write, &attributes, STDIO_PIPE_BUFFER_BYTES) };
        if created == FALSE {
            return Err(WindowsError::last("CreatePipe"));
        }
        let read = Handle::new(read)?;
        let write = Handle::new(write)?;
        // SAFETY: read is a valid pipe handle; clearing inherit prevents leaking read side.
        let protected = unsafe {
            windows_sys::Win32::Foundation::SetHandleInformation(read.raw(), HANDLE_FLAG_INHERIT, 0)
        };
        if protected == FALSE {
            return Err(WindowsError::last("SetHandleInformation"));
        }
        Ok((read, write))
    }

    fn create_process(
        token: HANDLE,
        command_line: &mut [u16],
        environment: &mut [u16],
        cwd: *const u16,
        startup: &mut STARTUPINFOEXW,
        process_information: &mut PROCESS_INFORMATION,
    ) -> Result<(), WindowsError> {
        let flags = CREATE_NO_WINDOW
            | CREATE_UNICODE_ENVIRONMENT
            | CREATE_SUSPENDED
            | EXTENDED_STARTUPINFO_PRESENT;
        let original_command_line = command_line.to_vec();
        // SAFETY: token is a restricted primary token; pointers refer to mutable, null-terminated
        // command/environment buffers; startup/process_information live through the call.
        let created = unsafe {
            CreateProcessAsUserW(
                token,
                null(),
                command_line.as_mut_ptr(),
                null(),
                null(),
                TRUE,
                flags,
                environment.as_mut_ptr() as *const _,
                cwd,
                startup as *mut STARTUPINFOEXW as *const STARTUPINFOW,
                process_information,
            )
        };
        if created != FALSE {
            return Ok(());
        }
        if last_error_code() != ERROR_PRIVILEGE_NOT_HELD {
            return Err(WindowsError::last("CreateProcessAsUserW"));
        }
        command_line.copy_from_slice(&original_command_line);
        let created = unsafe {
            CreateProcessWithTokenW(
                token,
                0,
                null(),
                command_line.as_mut_ptr(),
                flags,
                environment.as_mut_ptr() as *const _,
                cwd,
                startup as *mut STARTUPINFOEXW as *const STARTUPINFOW,
                process_information,
            )
        };
        if created == FALSE {
            Err(WindowsError::last("CreateProcessWithTokenW"))
        } else {
            Ok(())
        }
    }

    fn assign_to_job(job: &Handle, process: &Handle) -> Result<(), WindowsError> {
        // SAFETY: job and process are valid handles owned by this function's caller.
        let assigned = unsafe { AssignProcessToJobObject(job.raw(), process.raw()) };
        if assigned == FALSE {
            Err(WindowsError::last("AssignProcessToJobObject"))
        } else {
            Ok(())
        }
    }

    fn resume_thread(thread: &Handle) -> Result<(), WindowsError> {
        // SAFETY: thread is the primary thread created in suspended state.
        let previous = unsafe { ResumeThread(thread.raw()) };
        if previous == RESUME_THREAD_FAILED {
            Err(WindowsError::last("ResumeThread"))
        } else {
            Ok(())
        }
    }

    fn terminate_process(process: &Handle) {
        // SAFETY: process is a valid process handle created suspended and owned by this backend.
        unsafe {
            TerminateProcess(process.raw(), TIMEOUT_JOB_EXIT_CODE);
            WaitForSingleObject(process.raw(), POLL_INTERVAL_MS);
        }
    }

    fn terminate_job(job: &Handle) {
        // SAFETY: job is a valid job handle owned by this backend.
        unsafe {
            TerminateJobObject(job.raw(), TIMEOUT_JOB_EXIT_CODE);
        }
    }

    fn wait_for_process(
        request: &CommandRequest,
        process: &Handle,
        job: &Handle,
        stdout_read: Handle,
        stderr_read: Handle,
        start: Instant,
    ) -> Result<CommandResult, WindowsError> {
        let mut stdout = CapturedOutput::default();
        let mut stderr = CapturedOutput::default();
        let timeout_ms = request.timeout_seconds.saturating_mul(1_000);
        loop {
            drain_available(stdout_read.raw(), &mut stdout)?;
            drain_available(stderr_read.raw(), &mut stderr)?;
            // SAFETY: process is a valid process handle.
            let wait = unsafe { WaitForSingleObject(process.raw(), POLL_INTERVAL_MS) };
            if wait == WAIT_OBJECT_0 {
                break;
            }
            if wait != WAIT_TIMEOUT {
                return Err(WindowsError::last("WaitForSingleObject"));
            }
            if elapsed_ms(start) >= timeout_ms {
                // SAFETY: job owns the process tree; timeout fail-closed terminates the job.
                terminate_job(job);
                // SAFETY: process is a valid process handle; wait observes job termination.
                unsafe {
                    WaitForSingleObject(process.raw(), POLL_INTERVAL_MS);
                }
                drain_to_end(stdout_read.raw(), &mut stdout)?;
                drain_to_end(stderr_read.raw(), &mut stderr)?;
                return Ok(CommandResult::timed_out(
                    &request.command_id,
                    elapsed_ms(start),
                ));
            }
        }
        drain_to_end(stdout_read.raw(), &mut stdout)?;
        drain_to_end(stderr_read.raw(), &mut stderr)?;
        let mut exit_code = 0u32;
        // SAFETY: process is a valid process handle and exit_code is a valid out pointer.
        let got_exit = unsafe { GetExitCodeProcess(process.raw(), &mut exit_code) };
        if got_exit == FALSE {
            return Err(WindowsError::last("GetExitCodeProcess"));
        }
        Ok(CommandResult::executed(
            &request.command_id,
            exit_code_to_i32(exit_code),
            elapsed_ms(start),
            stdout.to_string(),
            stderr.to_string(),
            stdout.truncated || stderr.truncated,
        ))
    }

    #[derive(Default)]
    struct CapturedOutput {
        bytes: Vec<u8>,
        truncated: bool,
    }

    impl CapturedOutput {
        fn append(&mut self, chunk: &[u8]) {
            let remaining = CAPTURE_BYTE_LIMIT.saturating_sub(self.bytes.len());
            if chunk.len() > remaining {
                self.bytes.extend_from_slice(&chunk[..remaining]);
                self.truncated = true;
            } else {
                self.bytes.extend_from_slice(chunk);
            }
        }

        fn to_string(&self) -> String {
            String::from_utf8_lossy(&self.bytes).into_owned()
        }
    }

    fn drain_available(handle: HANDLE, output: &mut CapturedOutput) -> Result<(), WindowsError> {
        loop {
            let mut available = 0u32;
            // SAFETY: handle is a valid pipe read handle; null buffer probes available bytes.
            let peeked = unsafe {
                PeekNamedPipe(
                    handle,
                    null_mut(),
                    0,
                    null_mut(),
                    &mut available,
                    null_mut(),
                )
            };
            if peeked == FALSE {
                return Ok(());
            }
            if available == 0 {
                return Ok(());
            }
            read_pipe(handle, output, available.min(READ_BUFFER_BYTES as u32))?;
        }
    }

    fn drain_to_end(handle: HANDLE, output: &mut CapturedOutput) -> Result<(), WindowsError> {
        loop {
            let before = output.bytes.len();
            drain_available(handle, output)?;
            if output.bytes.len() == before {
                return Ok(());
            }
        }
    }

    fn read_pipe(
        handle: HANDLE,
        output: &mut CapturedOutput,
        bytes_to_read: u32,
    ) -> Result<(), WindowsError> {
        let mut buffer = vec![0u8; bytes_to_read as usize];
        let mut read = 0u32;
        // SAFETY: buffer is valid for bytes_to_read bytes; read is a valid out pointer.
        let ok = unsafe {
            ReadFile(
                handle,
                buffer.as_mut_ptr(),
                bytes_to_read,
                &mut read,
                null_mut(),
            )
        };
        if ok == FALSE || read == 0 {
            return Ok(());
        }
        output.append(&buffer[..read as usize]);
        Ok(())
    }

    fn command_cwd(request: &CommandRequest) -> PathBuf {
        let workspace = normalize_path(Path::new(&request.filesystem.workspace_root));
        let cwd = Path::new(&request.cwd);
        let path = if cwd.is_absolute() {
            normalize_path(cwd)
        } else {
            normalize_path(&workspace.join(cwd))
        };
        win32_process_path(&path)
    }

    fn win32_process_path(path: &Path) -> PathBuf {
        let value = path.as_os_str().to_string_lossy();
        if let Some(rest) = value.strip_prefix(WINDOWS_VERBATIM_UNC_PREFIX) {
            return PathBuf::from(format!(r"\\{rest}"));
        }
        if let Some(rest) = value.strip_prefix(WINDOWS_VERBATIM_PREFIX) {
            return PathBuf::from(rest);
        }
        path.to_path_buf()
    }

    fn command_line(argv: &[String]) -> Vec<u16> {
        wide_null(OsStr::new(
            &argv
                .iter()
                .map(|arg| quote_windows_arg(arg))
                .collect::<Vec<_>>()
                .join(" "),
        ))
    }

    fn quote_windows_arg(arg: &str) -> String {
        if arg.is_empty() {
            return "\"\"".to_string();
        }
        if !arg.chars().any(|ch| ch.is_whitespace() || ch == '"') {
            return arg.to_string();
        }
        let mut quoted = String::from("\"");
        let mut backslashes = 0usize;
        for ch in arg.chars() {
            match ch {
                '\\' => backslashes += 1,
                '"' => {
                    quoted.push_str(&"\\".repeat(backslashes * 2 + 1));
                    quoted.push('"');
                    backslashes = 0;
                }
                _ => {
                    quoted.push_str(&"\\".repeat(backslashes));
                    backslashes = 0;
                    quoted.push(ch);
                }
            }
        }
        quoted.push_str(&"\\".repeat(backslashes * 2));
        quoted.push('"');
        quoted
    }

    fn environment_block() -> Vec<u16> {
        let mut entries = std::env::vars()
            .filter(|(name, _value)| env_name_allowed(name) && !is_secret_env_name(name))
            .collect::<Vec<_>>();
        entries.sort_by(|left, right| {
            left.0
                .to_ascii_uppercase()
                .cmp(&right.0.to_ascii_uppercase())
        });
        let mut block = Vec::new();
        for (name, value) in entries {
            block.extend(OsStr::new(&format!("{name}={value}")).encode_wide());
            block.push(0);
        }
        block.push(0);
        block
    }

    fn env_name_allowed(name: &str) -> bool {
        const ALLOWLIST: [&str; 12] = [
            "APPDATA",
            "COMSPEC",
            "LOCALAPPDATA",
            "PATH",
            "PATHEXT",
            "PROGRAMFILES",
            "PROGRAMFILES(X86)",
            "PROGRAMW6432",
            "SYSTEMDRIVE",
            "SYSTEMROOT",
            "TEMP",
            "TMP",
        ];
        ALLOWLIST
            .iter()
            .any(|allowed| name.eq_ignore_ascii_case(allowed))
    }

    fn wide_null(value: &OsStr) -> Vec<u16> {
        value.encode_wide().chain(std::iter::once(0)).collect()
    }

    fn elapsed_ms(start: Instant) -> u64 {
        start.elapsed().as_millis().try_into().unwrap_or(u64::MAX)
    }

    fn exit_code_to_i32(exit_code: u32) -> i32 {
        i32::try_from(exit_code).unwrap_or(i32::MAX)
    }

    fn last_error_code() -> u32 {
        // SAFETY: GetLastError reads the thread-local Windows last-error value.
        unsafe { GetLastError() }
    }
}
