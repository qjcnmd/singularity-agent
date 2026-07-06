#![forbid(unsafe_code)]

use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread::{JoinHandle, sleep, spawn};
use std::time::{Duration, Instant};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub const DEFAULT_COMMAND_TIMEOUT_SECONDS: u64 = 30;
pub const DEFAULT_MAX_OUTPUT_CHARS: usize = 40_000;
const COMMAND_POLL_INTERVAL_MILLIS: u64 = 10;
const REDACTED_COMMAND_OUTPUT: &str = "[redacted sensitive command output]";
const PATCH_PATH_OUTSIDE_WORKSPACE: &str = "patch path must stay inside workspace";
const SECRET_OUTPUT_MARKERS: [&str; 5] =
    ["api_key", "authorization", "password", "secret", "token"];
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
    HostWorkspace,
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
    pub fn local_process(
        command_id: impl Into<String>,
        argv: Vec<String>,
        cwd: impl Into<String>,
    ) -> Self {
        Self {
            command_id: command_id.into(),
            argv,
            cwd: cwd.into(),
            purpose: CommandPurpose::Unknown,
            timeout_seconds: DEFAULT_COMMAND_TIMEOUT_SECONDS,
            network: SandboxNetworkPolicy {
                mode: SandboxNetworkMode::Allowed,
                allowed_hosts: Vec::new(),
                denied_hosts: Vec::new(),
                require_hard_isolation: false,
            },
            filesystem: SandboxFilesystemPolicy {
                mode: SandboxFilesystemMode::HostWorkspace,
                workspace_root: String::new(),
                writable_paths: Vec::new(),
                readonly_paths: Vec::new(),
            },
        }
    }

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
        self.network.require_hard_isolation
            || !matches!(self.filesystem.mode, SandboxFilesystemMode::HostWorkspace)
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

    fn backend_error(command_id: impl Into<String>, reason: impl Into<String>) -> Self {
        Self::blocked(
            command_id,
            CommandExecutionStatus::BackendError,
            CommandSemanticStatus::PolicyBlocked,
            reason,
        )
    }

    fn spawn_failed(command_id: impl Into<String>, reason: impl Into<String>) -> Self {
        Self::blocked(
            command_id,
            CommandExecutionStatus::SpawnFailed,
            CommandSemanticStatus::ExitNonzero,
            reason,
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

#[derive(Debug, Clone)]
pub struct CommandExecutor {
    process_manager: ProcessManager,
}

impl Default for CommandExecutor {
    fn default() -> Self {
        Self::new()
    }
}

impl CommandExecutor {
    pub fn new() -> Self {
        Self {
            process_manager: ProcessManager::new(DEFAULT_MAX_OUTPUT_CHARS),
        }
    }

    pub fn run_local(&self, request: &CommandRequest) -> CommandResult {
        if request.requires_sandbox() {
            return CommandResult::backend_error(
                request.command_id.clone(),
                "sandbox-required command has no sandbox backend",
            );
        }
        self.process_manager.run(request)
    }
}

#[derive(Debug, Clone)]
struct ProcessManager {
    max_output_chars: usize,
}

impl ProcessManager {
    fn new(max_output_chars: usize) -> Self {
        Self { max_output_chars }
    }

    fn run(&self, request: &CommandRequest) -> CommandResult {
        if request.argv.is_empty() {
            return CommandResult::spawn_failed(
                request.command_id.clone(),
                "command argv must not be empty",
            );
        }

        let started = Instant::now();
        let mut command = Command::new(&request.argv[0]);
        command
            .args(&request.argv[1..])
            .current_dir(&request.cwd)
            .env_clear()
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                return CommandResult::spawn_failed(
                    request.command_id.clone(),
                    format!("failed to spawn command: {error}"),
                );
            }
        };
        let stdout = child
            .stdout
            .take()
            .map(|pipe| capture_pipe(pipe, self.max_output_chars));
        let stderr = child
            .stderr
            .take()
            .map(|pipe| capture_pipe(pipe, self.max_output_chars));
        let timeout = Duration::from_secs(request.timeout_seconds);

        loop {
            match child.try_wait() {
                Ok(Some(_status)) => {
                    return match child.wait() {
                        Ok(status) => {
                            self.output_result(request, started, status, stdout, stderr, false)
                        }
                        Err(error) => CommandResult::spawn_failed(
                            request.command_id.clone(),
                            format!("failed to collect command output: {error}"),
                        ),
                    };
                }
                Ok(None) => {}
                Err(error) => {
                    let _ = child.kill();
                    return CommandResult::spawn_failed(
                        request.command_id.clone(),
                        format!("failed to poll command status: {error}"),
                    );
                }
            }

            if started.elapsed() >= timeout {
                kill_process_tree(&mut child);
                return match child.wait() {
                    Ok(status) => {
                        self.output_result(request, started, status, stdout, stderr, true)
                    }
                    Err(error) => CommandResult::spawn_failed(
                        request.command_id.clone(),
                        format!("failed to collect timed-out command output: {error}"),
                    ),
                };
            }
            sleep(Duration::from_millis(COMMAND_POLL_INTERVAL_MILLIS));
        }
    }

    fn output_result(
        &self,
        request: &CommandRequest,
        started: Instant,
        status: ExitStatus,
        stdout: Option<JoinHandle<CapturedOutput>>,
        stderr: Option<JoinHandle<CapturedOutput>>,
        timed_out: bool,
    ) -> CommandResult {
        let stdout = join_capture(stdout);
        let stderr = join_capture(stderr);
        let (stdout_preview, stdout_truncated, stdout_redacted) = preview_output(&stdout);
        let (stderr_preview, stderr_truncated, stderr_redacted) = preview_output(&stderr);
        let exit_code = status.code();
        let semantic_status = if timed_out {
            CommandSemanticStatus::TimedOut
        } else if status.success() {
            CommandSemanticStatus::Succeeded
        } else {
            CommandSemanticStatus::ExitNonzero
        };

        CommandResult {
            command_id: request.command_id.clone(),
            execution_status: if timed_out {
                CommandExecutionStatus::TimedOut
            } else {
                CommandExecutionStatus::Completed
            },
            semantic_status,
            exit_code,
            duration_ms: duration_millis(started.elapsed()),
            timed_out,
            stdout_preview,
            stderr_preview,
            output_truncated: stdout_truncated || stderr_truncated,
            redacted: stdout_redacted || stderr_redacted,
            changed_files: Vec::new(),
        }
    }
}

#[derive(Debug, Default)]
struct CapturedOutput {
    bytes: Vec<u8>,
    truncated: bool,
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
    Relaxed,
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

impl PatchResult {
    fn applied(changed_files: Vec<String>) -> Self {
        Self {
            applied: true,
            changed_files,
            rolled_back: false,
            error: None,
        }
    }

    fn failed(error: impl Into<String>, rolled_back: bool) -> Self {
        Self {
            applied: false,
            changed_files: Vec::new(),
            rolled_back,
            error: Some(error.into()),
        }
    }
}

#[derive(Debug, Clone)]
pub struct PatchExecutor {
    workspace_root: PathBuf,
}

impl PatchExecutor {
    pub fn new(workspace_root: impl Into<PathBuf>) -> Self {
        Self {
            workspace_root: workspace_root.into(),
        }
    }

    pub fn apply(&self, changes: &[PatchChange]) -> PatchResult {
        let mut snapshots = Vec::new();
        let mut changed_files = Vec::new();

        for change in changes {
            let path = match resolve_workspace_path(&self.workspace_root, &change.path) {
                Ok(path) => path,
                Err(error) => {
                    rollback_snapshots(&snapshots);
                    return PatchResult::failed(error, !snapshots.is_empty());
                }
            };
            let before = fs::read_to_string(&path).ok();
            snapshots.push((path.clone(), before.clone()));
            let next_content =
                match next_file_content(&change.expected, before.as_deref(), &change.replacement) {
                    Ok(content) => content,
                    Err(error) => {
                        rollback_snapshots(&snapshots);
                        return PatchResult::failed(error, true);
                    }
                };
            if let Some(parent) = path.parent()
                && let Err(error) = fs::create_dir_all(parent)
            {
                rollback_snapshots(&snapshots);
                return PatchResult::failed(
                    format!("failed to create parent directory: {error}"),
                    true,
                );
            }
            if let Err(error) = fs::write(&path, next_content) {
                rollback_snapshots(&snapshots);
                return PatchResult::failed(format!("failed to write patch file: {error}"), true);
            }
            changed_files.push(change.path.clone());
        }

        PatchResult::applied(changed_files)
    }
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

fn capture_pipe(mut pipe: impl Read + Send + 'static, limit: usize) -> JoinHandle<CapturedOutput> {
    spawn(move || {
        let mut captured = Vec::new();
        let mut truncated = false;
        let mut buffer = [0; 4096];
        loop {
            let read = match pipe.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => read,
                Err(_) => break,
            };
            let remaining = limit.saturating_sub(captured.len());
            if remaining == 0 {
                truncated = true;
                continue;
            }
            let take = remaining.min(read);
            captured.extend_from_slice(&buffer[..take]);
            if take < read {
                truncated = true;
            }
        }
        CapturedOutput {
            bytes: captured,
            truncated,
        }
    })
}

fn join_capture(handle: Option<JoinHandle<CapturedOutput>>) -> CapturedOutput {
    handle
        .and_then(|handle| handle.join().ok())
        .unwrap_or_default()
}

fn preview_output(output: &CapturedOutput) -> (String, bool, bool) {
    let preview = String::from_utf8_lossy(&output.bytes).to_string();
    let lowered = preview.to_ascii_lowercase();
    if SECRET_OUTPUT_MARKERS
        .iter()
        .any(|marker| lowered.contains(marker))
    {
        (REDACTED_COMMAND_OUTPUT.to_string(), output.truncated, true)
    } else {
        (preview, output.truncated, false)
    }
}

#[cfg(windows)]
fn kill_process_tree(child: &mut Child) {
    let _ = Command::new("taskkill")
        .args(["/PID", &child.id().to_string(), "/T", "/F"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let _ = child.kill();
}

#[cfg(not(windows))]
fn kill_process_tree(child: &mut Child) {
    let _ = child.kill();
}

fn duration_millis(duration: Duration) -> u64 {
    duration.as_millis().try_into().unwrap_or(u64::MAX)
}

fn resolve_workspace_path(workspace_root: &Path, relative: &str) -> Result<PathBuf, String> {
    let relative = normalized_relative_patch_path(Path::new(relative))?;
    let root = workspace_root
        .canonicalize()
        .map_err(|error| format!("failed to resolve workspace root: {error}"))?;
    reject_symlink_components(&root, &relative)?;
    let target = root.join(relative);
    if let Ok(resolved_target) = target.canonicalize()
        && !resolved_target.starts_with(&root)
    {
        return Err(PATCH_PATH_OUTSIDE_WORKSPACE.to_string());
    }
    Ok(target)
}

fn normalized_relative_patch_path(path: &Path) -> Result<PathBuf, String> {
    let mut relative = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => relative.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(PATCH_PATH_OUTSIDE_WORKSPACE.to_string());
            }
        }
    }
    Ok(relative)
}

fn reject_symlink_components(root: &Path, relative: &Path) -> Result<(), String> {
    let mut current = root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(part) = component else {
            return Err(PATCH_PATH_OUTSIDE_WORKSPACE.to_string());
        };
        current.push(part);
        if let Ok(metadata) = fs::symlink_metadata(&current)
            && metadata.file_type().is_symlink()
        {
            return Err(PATCH_PATH_OUTSIDE_WORKSPACE.to_string());
        }
    }
    Ok(())
}

fn next_file_content(
    expected: &Option<String>,
    before: Option<&str>,
    replacement: &str,
) -> Result<String, String> {
    match (expected, before) {
        (Some(expected), Some(before)) if before.contains(expected) => {
            Ok(before.replacen(expected, replacement, 1))
        }
        (Some(_), Some(_)) => Err("expected patch text was not found".to_string()),
        (Some(_), None) => Err("cannot replace text in a missing file".to_string()),
        (None, None) => Ok(replacement.to_string()),
        (None, Some(_)) => Err("create patch target already exists".to_string()),
    }
}

fn rollback_snapshots(snapshots: &[(PathBuf, Option<String>)]) {
    for (path, before) in snapshots.iter().rev() {
        if let Some(content) = before {
            let _ = fs::write(path, content);
        } else {
            let _ = fs::remove_file(path);
        }
    }
}
