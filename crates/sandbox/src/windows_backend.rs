use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::Instant;

use super::{
    COMMAND_CANCELLED, COMMAND_TIMED_OUT, CancellationToken, CommandEnvironmentPolicy,
    CommandExecutionStatus, CommandRequest, CommandResult, CommandScriptRequest,
    CommandSemanticStatus, SandboxBackend, SandboxBackendEnforcement, SandboxCapabilities,
    SandboxFilesystemMode, SandboxNetworkMode, WorkspaceChangeSummary, WorkspaceMutation,
    WorkspaceSnapshot, command_request_denial, command_script_request_denial, is_secret_env_name,
    path_has_sensitive_component, snapshot_workspace,
};
use singularity_core::{
    PROTECTED_METADATA_PATH_NAMES, PROTECTED_PATH_CONTAINS_MARKERS, PROTECTED_PATH_EXACT_MARKERS,
    PROTECTED_PATH_PREFIXES, PROTECTED_PATH_SUFFIXES,
};
use singularity_windows_sandbox::{
    AbsolutePathBuf, ElevatedSandboxProfileCaptureRequest, FileSystemAccessMode, FileSystemPath,
    FileSystemSandboxEntry, FileSystemSandboxPolicy, ManagedFileSystemPermissions,
    NetworkSandboxPolicy, PermissionProfile, WindowsSandboxCancellationToken,
    WorkspaceChangeMonitor, WorkspaceChangeObservation, resolve_windows_deny_read_paths,
    run_windows_sandbox_capture_for_permission_profile_elevated,
    run_windows_sandbox_capture_with_filesystem_overrides, safe_windows_error_summary,
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
const WORKSPACE_CHANGE_SUMMARY_UNAVAILABLE: &str =
    "capability_not_supported:workspace_change_summary";

#[derive(Debug)]
struct ResolvedExecutable {
    argv: Vec<String>,
    read_roots: Vec<PathBuf>,
}

#[derive(Debug)]
enum PrepareCommandError {
    Executable(ExecutableResolutionError),
    Backend(String),
    ProtectedPaths(String),
    WorkspaceObservation,
}

#[derive(Debug, PartialEq, Eq)]
enum ExecutableResolutionError {
    Unavailable(String),
    NotPermitted(String),
    Unsupported(String),
}

impl ExecutableResolutionError {
    fn into_command_result(self, command_id: &str) -> CommandResult {
        let result = match self {
            Self::Unavailable(message) => {
                CommandResult::executable_unavailable(command_id, message)
            }
            Self::Unsupported(message) => CommandResult::unsupported(command_id, message),
            Self::NotPermitted(message) => CommandResult::policy_denied(command_id, message),
        };
        result.with_workspace_mutation(WorkspaceMutation::Unknown)
    }
}

#[derive(Debug, Clone, Default)]
/// Windows 严格 sandbox backend。
pub struct WindowsSandboxBackend;

impl WindowsSandboxBackend {
    /// 创建 Windows sandbox backend。
    pub fn new() -> Self {
        Self
    }
}

impl SandboxBackend for WindowsSandboxBackend {
    fn name(&self) -> &'static str {
        BACKEND_NAME
    }

    fn capabilities(&self) -> SandboxCapabilities {
        SandboxCapabilities::strict().with_change_detection()
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
                .with_workspace_mutation(WorkspaceMutation::Unknown)
                .with_sandbox_execution(self.name(), SandboxBackendEnforcement::Strict);
        }
        if let Some(denied) = command_request_denial(request) {
            return denied
                .with_workspace_mutation(WorkspaceMutation::Unknown)
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
                    .with_workspace_mutation(WorkspaceMutation::Unknown)
                    .with_sandbox_execution(self.name(), SandboxBackendEnforcement::Unavailable);
            }
            Err(PrepareCommandError::ProtectedPaths(error)) => {
                return CommandResult::backend_error(&request.command_id, error)
                    .with_workspace_mutation(WorkspaceMutation::Unknown)
                    .with_sandbox_execution(self.name(), SandboxBackendEnforcement::Unavailable);
            }
            Err(PrepareCommandError::WorkspaceObservation) => {
                return CommandResult::unsupported(
                    &request.command_id,
                    WORKSPACE_CHANGE_SUMMARY_UNAVAILABLE,
                )
                .with_workspace_mutation(WorkspaceMutation::Unknown)
                .with_sandbox_execution(self.name(), SandboxBackendEnforcement::Unavailable);
            }
        };
        execute_prepared_command(
            &request.command_id,
            cancellation,
            prepared,
            matches!(
                request.filesystem.mode,
                SandboxFilesystemMode::WorkspaceWrite
            ),
        )
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
                .with_workspace_mutation(WorkspaceMutation::Unknown)
                .with_sandbox_execution(self.name(), SandboxBackendEnforcement::Strict);
        }
        if let Some(denied) = command_script_request_denial(request) {
            return denied
                .with_workspace_mutation(WorkspaceMutation::Unknown)
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
                    .with_workspace_mutation(WorkspaceMutation::Unknown)
                    .with_sandbox_execution(self.name(), SandboxBackendEnforcement::Unavailable);
            }
            Err(PrepareCommandError::ProtectedPaths(error)) => {
                return CommandResult::backend_error(&request.command_id, error)
                    .with_workspace_mutation(WorkspaceMutation::Unknown)
                    .with_sandbox_execution(self.name(), SandboxBackendEnforcement::Unavailable);
            }
            Err(PrepareCommandError::WorkspaceObservation) => {
                return CommandResult::unsupported(
                    &request.command_id,
                    WORKSPACE_CHANGE_SUMMARY_UNAVAILABLE,
                )
                .with_workspace_mutation(WorkspaceMutation::Unknown)
                .with_sandbox_execution(self.name(), SandboxBackendEnforcement::Unavailable);
            }
        };
        execute_prepared_command(
            &request.command_id,
            cancellation,
            prepared,
            matches!(
                request.filesystem.mode,
                SandboxFilesystemMode::WorkspaceWrite
            ),
        )
    }
}

fn execute_prepared_command(
    command_id: &str,
    cancellation: &CancellationToken,
    prepared: PreparedCommand,
    observe_workspace_change: bool,
) -> CommandResult {
    let workspace = prepared.workspace_roots[0].as_path().to_path_buf();
    let before = prepared.before.clone();
    let mut monitor = None;
    let result = match execute_windows_sandbox(
        command_id,
        cancellation,
        prepared,
        observe_workspace_change.then_some(&mut monitor),
    ) {
        Ok(result) => result,
        Err(error) => {
            return CommandResult::backend_error(command_id, error)
                .with_workspace_mutation(WorkspaceMutation::Unknown)
                .with_sandbox_execution(BACKEND_NAME, SandboxBackendEnforcement::Unavailable);
        }
    };
    let observation = monitor.and_then(|monitor| monitor.finish().ok());
    let snapshot_change = before.map(|before| {
        snapshot_workspace(&workspace).and_then(|after| before.change_summary(&after))
    });
    let (mutation, summary) = reconcile_workspace_change(observation, snapshot_change);
    let result = result.with_workspace_mutation(mutation);
    match summary {
        Some(summary) => result.with_workspace_change_summary(summary),
        None => result,
    }
}

fn reconcile_workspace_change(
    observation: Option<WorkspaceChangeObservation>,
    snapshot_change: Option<Result<Option<WorkspaceChangeSummary>, String>>,
) -> (WorkspaceMutation, Option<WorkspaceChangeSummary>) {
    match (observation, snapshot_change) {
        (None, Some(Ok(None))) => (WorkspaceMutation::Unchanged, None),
        (None, Some(Ok(Some(summary)))) => (WorkspaceMutation::Changed, Some(summary)),
        (Some(WorkspaceChangeObservation::Unchanged), Some(Ok(None))) => {
            (WorkspaceMutation::Unchanged, None)
        }
        (Some(WorkspaceChangeObservation::Changed), Some(Ok(Some(summary)))) => {
            (WorkspaceMutation::Changed, Some(summary))
        }
        _ => (WorkspaceMutation::Unknown, None),
    }
}

/// 将 core 的 protected path 规则投影为 resolver 可展开的 workspace glob。
fn resolve_existing_protected_paths(
    workspace_root: &AbsolutePathBuf,
) -> Result<Vec<AbsolutePathBuf>, String> {
    let entries = protected_path_glob_entries(workspace_root);
    let policy = FileSystemSandboxPolicy::restricted(entries);
    resolve_windows_deny_read_paths(&policy, workspace_root)
}

fn protected_path_glob_entries(workspace_root: &AbsolutePathBuf) -> Vec<FileSystemSandboxEntry> {
    let mut patterns = Vec::new();
    for marker in PROTECTED_METADATA_PATH_NAMES {
        patterns.push(format_workspace_protected_glob(workspace_root, marker));
    }
    for marker in PROTECTED_PATH_EXACT_MARKERS {
        patterns.push(format_workspace_protected_glob(workspace_root, marker));
    }
    for prefix in PROTECTED_PATH_PREFIXES {
        patterns.push(format_workspace_protected_glob(workspace_root, prefix));
        patterns.push(format_workspace_protected_glob(
            workspace_root,
            &format!("{prefix}.*"),
        ));
    }
    for suffix in PROTECTED_PATH_SUFFIXES {
        patterns.push(format_workspace_protected_glob(
            workspace_root,
            &format!("*{suffix}"),
        ));
    }
    for marker in PROTECTED_PATH_CONTAINS_MARKERS {
        patterns.push(format_workspace_protected_glob(
            workspace_root,
            &format!("*{marker}*"),
        ));
    }
    patterns
        .into_iter()
        .map(|pattern| FileSystemSandboxEntry {
            path: FileSystemPath::GlobPattern { pattern },
            access: FileSystemAccessMode::Deny,
        })
        .collect()
}

fn format_workspace_protected_glob(
    workspace_root: &AbsolutePathBuf,
    component_pattern: &str,
) -> String {
    let root = workspace_root.to_string_lossy().replace('\\', "/");
    let separator = if root.ends_with('/') { "" } else { "/" };
    format!("{root}{separator}**/{component_pattern}")
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
    protected_deny_write_paths: Vec<AbsolutePathBuf>,
    protect_workspace_metadata: bool,
    before: Option<WorkspaceSnapshot>,
}

impl PreparedCommand {
    fn from_request(request: &CommandRequest) -> Result<Self, PrepareCommandError> {
        let workspace_root = canonical_directory(Path::new(&request.filesystem.workspace_root))
            .map_err(PrepareCommandError::Backend)?;
        let before = matches!(
            request.filesystem.mode,
            SandboxFilesystemMode::WorkspaceWrite
        )
        .then(|| snapshot_workspace(&workspace_root))
        .transpose()
        .map_err(|_| PrepareCommandError::WorkspaceObservation)?;
        let cwd =
            canonical_directory(Path::new(&request.cwd)).map_err(PrepareCommandError::Backend)?;
        let env_map = child_environment(&request.environment);
        let resolved = resolve_executable(&request.argv, &cwd, &env_map)
            .map_err(PrepareCommandError::Executable)?;
        let workspace_root =
            AbsolutePathBuf::from_absolute_path_checked(&workspace_root).map_err(|error| {
                PrepareCommandError::Backend(format!("invalid workspace root: {error}"))
            })?;
        let protect_workspace_metadata = !request.is_trusted_workspace_preparation();
        let protected_deny_read_paths = if protect_workspace_metadata {
            resolve_existing_protected_paths(&workspace_root)
                .map_err(PrepareCommandError::ProtectedPaths)?
        } else {
            Vec::new()
        };
        let protected_deny_write_paths = if matches!(
            request.filesystem.mode,
            SandboxFilesystemMode::WorkspaceWrite
        ) {
            protected_deny_read_paths.clone()
        } else {
            Vec::new()
        };
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
            && protected_deny_read_paths.is_empty()
            && protect_workspace_metadata;
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
            protected_deny_read_paths,
            protected_deny_write_paths,
            protect_workspace_metadata,
            before,
        })
    }

    fn from_script_request(request: &CommandScriptRequest) -> Result<Self, PrepareCommandError> {
        let workspace_root = canonical_directory(Path::new(&request.filesystem.workspace_root))
            .map_err(PrepareCommandError::Backend)?;
        let before = matches!(
            request.filesystem.mode,
            SandboxFilesystemMode::WorkspaceWrite
        )
        .then(|| snapshot_workspace(&workspace_root))
        .transpose()
        .map_err(|_| PrepareCommandError::WorkspaceObservation)?;
        let cwd =
            canonical_directory(Path::new(&request.cwd)).map_err(PrepareCommandError::Backend)?;
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
        let workspace_root =
            AbsolutePathBuf::from_absolute_path_checked(&workspace_root).map_err(|error| {
                PrepareCommandError::Backend(format!("invalid workspace root: {error}"))
            })?;
        let protected_deny_read_paths = resolve_existing_protected_paths(&workspace_root)
            .map_err(PrepareCommandError::ProtectedPaths)?;
        let protected_deny_write_paths = if matches!(
            request.filesystem.mode,
            SandboxFilesystemMode::WorkspaceWrite
        ) {
            protected_deny_read_paths.clone()
        } else {
            Vec::new()
        };
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
            protected_deny_write_paths,
            protect_workspace_metadata: true,
            before,
        })
    }
}

/// 将准备阶段的 deny-read 集合绑定到现有 elevated capture 请求。
fn elevated_capture_request<'a>(
    prepared: &'a PreparedCommand,
    windows_cancellation: WindowsSandboxCancellationToken,
    workspace_change_monitor: Option<&'a mut Option<WorkspaceChangeMonitor>>,
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
    elevated.deny_write_paths_override = &prepared.protected_deny_write_paths;
    elevated.protect_workspace_metadata = prepared.protect_workspace_metadata;
    elevated.workspace_change_monitor = workspace_change_monitor;
    elevated
}

fn execute_windows_sandbox(
    command_id: &str,
    cancellation: &CancellationToken,
    prepared: PreparedCommand,
    workspace_change_monitor: Option<&mut Option<WorkspaceChangeMonitor>>,
) -> Result<CommandResult, String> {
    let started = Instant::now();
    let windows_cancellation = WindowsSandboxCancellationToken::new({
        let cancellation = cancellation.clone();
        move || cancellation.is_cancelled()
    });
    let elevated =
        run_windows_sandbox_capture_for_permission_profile_elevated(elevated_capture_request(
            &prepared,
            windows_cancellation.clone(),
            workspace_change_monitor,
        ));
    match elevated {
        Ok(capture) => Ok(command_result_from_capture(command_id, capture, started)
            .with_sandbox_execution(ELEVATED_BACKEND_NAME, SandboxBackendEnforcement::Strict)),
        Err(elevated_error)
            if prepared.restricted_token_fallback
                && prepared.protected_deny_read_paths.is_empty() =>
        {
            let elevated_error = windows_error_summary(&elevated_error);
            let capture = run_windows_sandbox_capture_with_filesystem_overrides(
                &prepared.permission_profile,
                &prepared.workspace_roots,
                &prepared.sandbox_home,
                prepared.argv.clone(),
                &prepared.cwd,
                prepared.env_map,
                Some(prepared.timeout_ms),
                Some(windows_cancellation),
                &[],
                &prepared.protected_deny_write_paths,
                true,
            )
            .map_err(|restricted_error| {
                let restricted_error = windows_error_summary(&restricted_error);
                format!(
                    "{ELEVATED_FAILURE_PREFIX}: {elevated_error}; {RESTRICTED_FAILURE_PREFIX}: {restricted_error}"
                )
            })?;
            Ok(
                command_result_from_capture(command_id, capture, started).with_sandbox_execution(
                    RESTRICTED_TOKEN_BACKEND_NAME,
                    SandboxBackendEnforcement::RestrictedToken,
                ),
            )
        }
        Err(error) => {
            if !prepared.protected_deny_read_paths.is_empty()
                || !prepared.protected_deny_write_paths.is_empty()
            {
                Err(format!(
                    "{PROTECTED_PATH_ENFORCEMENT_FAILED}: {}",
                    safe_windows_error_summary(&error)
                ))
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
    .with_workspace_mutation(WorkspaceMutation::Unknown)
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
    result.with_workspace_mutation(WorkspaceMutation::Unknown)
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

fn batch_argv(shell: &Path, script: &Path, arguments: &[String]) -> Result<Vec<String>, String> {
    let script = script.to_string_lossy();
    if !batch_argument_is_safe(&script) || arguments.iter().any(|arg| !batch_argument_is_safe(arg))
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

fn find_executable_on_path(requested: &str, env_map: &HashMap<String, String>) -> Option<PathBuf> {
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
        .ok_or_else(|| format!("{SANDBOX_HOME_ENV} and {USER_PROFILE_ENV} are both unavailable"))?;
    let home = if home.is_absolute() {
        home
    } else {
        std::env::current_dir()
            .map_err(|error| format!("failed to resolve sandbox home: {error}"))?
            .join(home)
    };
    std::fs::create_dir_all(&home)
        .map_err(|error| format!("failed to create sandbox home {}: {error}", home.display()))?;
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

        let resolved =
            resolve_executable(&["runner".to_string()], temp.path(), &env).expect("resolve runner");

        assert!(resolved.argv[0].to_ascii_lowercase().ends_with("cmd.exe"));
        assert!(resolved.argv[5].contains("runner.CMD"));
        assert!(
            !resolved
                .read_roots
                .contains(&dunce::canonicalize(second).expect("canonical unrelated PATH entry"))
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
    fn workspace_write_command_projects_existing_protected_paths_to_read_and_write_denies() {
        let workspace = tempfile::tempdir().expect("workspace");
        let env_file = workspace.path().join(".env");
        create_test_file(&env_file, "opaque");
        let missing_env = workspace.path().join(".env.future");
        let comspec = std::env::var_os("ComSpec").expect("ComSpec");
        let request = CommandRequest::project_verification(
            "command_protected_write",
            vec![
                PathBuf::from(comspec).to_string_lossy().into_owned(),
                "/c".to_string(),
                "echo ready".to_string(),
            ],
            workspace.path().to_string_lossy(),
            workspace.path().to_string_lossy(),
        );

        let prepared =
            PreparedCommand::from_request(&request).expect("workspace-write command preparation");
        let protected_paths = prepared
            .protected_deny_write_paths
            .iter()
            .map(|path| path.to_path_buf())
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(
            protected_paths,
            [dunce::canonicalize(&env_file).expect("canonical .env")]
                .into_iter()
                .collect()
        );
        assert!(!missing_env.exists());
        assert_eq!(
            prepared
                .protected_deny_read_paths
                .iter()
                .map(|path| path.to_path_buf())
                .collect::<std::collections::HashSet<_>>(),
            protected_paths
        );

        let elevated = elevated_capture_request(
            &prepared,
            WindowsSandboxCancellationToken::new(|| false),
            None,
        );
        assert_eq!(
            elevated.deny_read_paths_override,
            prepared.protected_deny_read_paths.as_slice()
        );
        assert_eq!(
            elevated.deny_write_paths_override,
            prepared.protected_deny_write_paths.as_slice()
        );
    }

    #[test]
    fn trusted_workspace_preparation_does_not_project_protected_paths() {
        let workspace = tempfile::tempdir().expect("workspace");
        fs::create_dir(workspace.path().join(".git")).expect("git directory");
        fs::create_dir(workspace.path().join(".agents")).expect("agents directory");
        fs::create_dir(workspace.path().join(".singularity")).expect("singularity directory");
        let comspec = std::env::var_os("ComSpec").expect("ComSpec");
        let mut request = CommandRequest::trusted_workspace_preparation(
            "trusted_workspace_preparation",
            vec![
                PathBuf::from(comspec).to_string_lossy().into_owned(),
                "/c".to_string(),
                "echo ready".to_string(),
            ],
            workspace.path().to_string_lossy(),
            workspace.path().to_string_lossy(),
        );
        request.network.mode = SandboxNetworkMode::Allowed;

        let prepared =
            PreparedCommand::from_request(&request).expect("trusted workspace preparation");

        assert!(request.is_trusted_workspace_preparation());
        assert!(prepared.protected_deny_read_paths.is_empty());
        assert!(prepared.protected_deny_write_paths.is_empty());
        assert!(!prepared.protect_workspace_metadata);
        assert!(!prepared.restricted_token_fallback);
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
    fn ordinary_reparse_does_not_reject_script_preparation() {
        use std::os::windows::fs::symlink_dir;

        let workspace = tempfile::tempdir().expect("workspace");
        let target = workspace.path().join("ordinary-target");
        let alias = workspace.path().join("ordinary-link");
        fs::create_dir(&target).expect("create target");
        if symlink_dir(&target, &alias).is_err() {
            return;
        }
        let request = CommandScriptRequest::agent_requested(
            "script_reparse_boundary",
            "Write-Output ready",
            workspace.path().to_string_lossy(),
            workspace.path().to_string_lossy(),
        );

        let prepared = PreparedCommand::from_script_request(&request)
            .expect("ordinary reparse must not reject script preparation");
        assert!(prepared.protected_deny_read_paths.is_empty());
    }

    #[test]
    fn existing_protected_path_disables_restricted_token_fallback() {
        let workspace = tempfile::tempdir().expect("workspace");
        let nested = workspace.path().join("nested");
        fs::create_dir_all(&nested).expect("nested directory");
        fs::create_dir(workspace.path().join(".git")).expect("git directory");
        fs::create_dir(workspace.path().join(".agents")).expect("agents directory");
        fs::create_dir(workspace.path().join(".singularity")).expect("singularity directory");
        create_test_file(&workspace.path().join(".env.local"), "opaque");
        create_test_file(&nested.join("private-key.pem"), "opaque");
        create_test_file(&nested.join("server.pem"), "opaque");
        create_test_file(&nested.join("client.p12"), "opaque");
        create_test_file(&nested.join("client-secret.txt"), "opaque");
        let request = CommandScriptRequest::agent_requested_with_policy(
            "script_protected_path",
            "Join-Path 'nested' (Get-Random) | Out-Null",
            workspace.path().to_string_lossy(),
            workspace.path().to_string_lossy(),
            SandboxFilesystemMode::WorkspaceWrite,
            SandboxNetworkMode::Allowed,
        );

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
                workspace.path().join(".agents"),
                workspace.path().join(".singularity"),
                workspace.path().join(".env.local"),
                nested.join("private-key.pem"),
                nested.join("server.pem"),
                nested.join("client.p12"),
                nested.join("client-secret.txt"),
            ]
            .into_iter()
            .map(|path| dunce::canonicalize(path).expect("canonical protected path"))
            .collect()
        );
        let elevated = elevated_capture_request(
            &prepared,
            WindowsSandboxCancellationToken::new(|| false),
            None,
        );
        assert_eq!(
            elevated.deny_read_paths_override,
            prepared.protected_deny_read_paths.as_slice()
        );
        assert_eq!(
            elevated.deny_write_paths_override,
            prepared.protected_deny_write_paths.as_slice()
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
