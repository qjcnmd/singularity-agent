#[cfg(test)]
use std::cell::Cell;
use std::collections::{HashMap, VecDeque};
use std::ffi::OsStr;
use std::iter::once;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use std::os::windows::ffi::OsStrExt;

use super::workspace_change::{
    CachedProtectedPath, capture_cached_protected_paths, validate_cached_protected_paths,
    validate_cached_workspace_root,
};
use super::{
    COMMAND_CANCELLED, COMMAND_TIMED_OUT, CancellationToken, CommandEnvironmentPolicy,
    CommandExecutionStatus, CommandRequest, CommandResult, CommandScriptRequest,
    CommandSemanticStatus, ExecutableAvailability, IncrementalSnapshot, SandboxBackend,
    SandboxBackendEnforcement, SandboxCapabilities, SandboxFilesystemMode, SandboxNetworkMode,
    SandboxPreflightFact, SandboxPreflightReport, WorkspaceChangeSummary, WorkspaceMutation,
    WorkspaceObservationMetrics, WorkspaceObservationMode, WorkspaceObservationPhaseMetrics,
    WorkspaceSnapshot, command_request_denial, command_script_request_denial, is_secret_env_name,
    path_has_sensitive_component, snapshot_trusted_workspace,
    snapshot_trusted_workspace_from_handle, snapshot_workspace_as_sandbox_user,
    snapshot_workspace_as_sandbox_user_for_cached_root, update_workspace_snapshot_as_sandbox_user,
};
use singularity_core::{
    PROTECTED_METADATA_PATH_NAMES, PROTECTED_PATH_CONTAINS_MARKERS, PROTECTED_PATH_EXACT_MARKERS,
    PROTECTED_PATH_PREFIXES, PROTECTED_PATH_SUFFIXES,
};
use singularity_windows_sandbox::{
    AbsolutePathBuf, ElevatedSandboxProfileCaptureRequest, FileSystemAccessMode, FileSystemPath,
    FileSystemSandboxEntry, FileSystemSandboxPolicy, ManagedFileSystemPermissions,
    NetworkSandboxPolicy, PermissionProfile, TrustedWorkspaceError, TrustedWorkspaceLease,
    WindowsSandboxCancellationToken, WorkspaceChangeMonitor, WorkspaceChangeObservation,
    WorkspaceRootLease, resolve_windows_deny_read_paths_for_controller,
    resolve_windows_deny_read_paths_for_controller_with_pinned_workspace_root,
    resolve_windows_deny_read_paths_from_validated_workspace,
    run_windows_sandbox_capture_for_permission_profile_elevated,
    run_windows_sandbox_capture_for_permission_profile_with_observations_elevated,
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
const MAX_WORKSPACE_OBSERVATION_SESSIONS: usize = 16;
const WORKSPACE_OBSERVATION_CONTRACT: &str = "windows_workspace_observation/v1";

#[cfg(test)]
thread_local! {
    static FULL_PROTECTED_RESOLVER_SCANS: Cell<usize> = const { Cell::new(0) };
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct WorkspaceObservationSessionKey {
    root: PathBuf,
    contract: String,
}

#[derive(Clone)]
struct BeforeSnapshotSeed {
    snapshot: WorkspaceSnapshot,
    observation: WorkspaceChangeObservation,
}

struct WorkspaceObservationPreparation {
    seed: Option<BeforeSnapshotSeed>,
    monitor: WorkspaceChangeMonitor,
    cached_protected_paths: Option<Vec<AbsolutePathBuf>>,
    workspace_root_lease: WorkspaceRootLease,
}

impl WorkspaceObservationPreparation {
    /// Keep the live monitor for execution independently of protected-path cache reuse.
    fn into_execution_parts(
        self,
    ) -> (
        Option<BeforeSnapshotSeed>,
        Option<WorkspaceChangeMonitor>,
        Option<Vec<AbsolutePathBuf>>,
        Option<WorkspaceRootLease>,
    ) {
        (
            self.seed,
            Some(self.monitor),
            self.cached_protected_paths,
            Some(self.workspace_root_lease),
        )
    }
}

/// One atomically published observation checkpoint and its concrete protected-path fact.
struct WorkspaceObservationCache {
    snapshot: WorkspaceSnapshot,
    protected_paths: Vec<CachedProtectedPath>,
}

struct WorkspaceObservationSession {
    root: PathBuf,
    monitor: Option<WorkspaceChangeMonitor>,
    cache: Option<WorkspaceObservationCache>,
}

impl WorkspaceObservationSession {
    fn start(root: PathBuf) -> Result<Self, String> {
        let monitor = WorkspaceChangeMonitor::start(&root)
            .map_err(|error| format!("workspace observation session failed to start: {error}"))?;
        Ok(Self {
            root,
            monitor: Some(monitor),
            cache: None,
        })
    }

    fn prepare_for_command(&mut self) -> Result<WorkspaceObservationPreparation, String> {
        let workspace_root_lease = WorkspaceRootLease::acquire(&self.root)
            .map_err(|error| format!("workspace root lease acquisition failed: {error:#}"))?;
        let Some(previous) = self.monitor.take() else {
            return Err("workspace observation monitor is unavailable".to_string());
        };
        let Some(cache) = self.cache.as_ref() else {
            return Ok(WorkspaceObservationPreparation {
                seed: None,
                monitor: previous,
                cached_protected_paths: None,
                workspace_root_lease,
            });
        };
        let snapshot = cache.snapshot.clone();
        let next = WorkspaceChangeMonitor::start(&self.root)
            .map_err(|error| format!("workspace observation rollover failed: {error}"))?;
        let observation = previous
            .finish()
            .map_err(|error| format!("workspace observation checkpoint failed: {error}"))?;
        let can_reuse = matches!(observation, WorkspaceChangeObservation::Unchanged)
            && validate_cached_workspace_root(&self.root, &snapshot).is_ok()
            && validate_cached_protected_paths(&self.root, &cache.protected_paths).is_ok();
        let cached_protected_paths = can_reuse.then(|| {
            cache
                .protected_paths
                .iter()
                .map(|cached| cached.path.clone())
                .collect()
        });
        let seed = can_reuse.then_some(BeforeSnapshotSeed {
            snapshot,
            observation,
        });
        if !can_reuse {
            self.cache = None;
        }
        Ok(WorkspaceObservationPreparation {
            seed,
            monitor: next,
            cached_protected_paths,
            workspace_root_lease,
        })
    }

    #[cfg(test)]
    fn before_seed(&mut self) -> Result<Option<BeforeSnapshotSeed>, String> {
        let preparation = self.prepare_for_command()?;
        self.monitor = Some(preparation.monitor);
        Ok(preparation.seed)
    }

    #[cfg(test)]
    fn publish(
        &mut self,
        snapshot: WorkspaceSnapshot,
        continuation: Option<WorkspaceChangeMonitor>,
    ) {
        let _ = self.publish_with_protected_paths(snapshot, Vec::new(), continuation);
    }

    fn publish_with_protected_paths(
        &mut self,
        snapshot: WorkspaceSnapshot,
        protected_paths: Vec<AbsolutePathBuf>,
        continuation: Option<WorkspaceChangeMonitor>,
    ) -> bool {
        if let Some(continuation) = continuation {
            self.monitor = Some(continuation);
        }
        match capture_cached_protected_paths(&self.root, &protected_paths) {
            Ok(protected_paths) => {
                self.cache = Some(WorkspaceObservationCache {
                    snapshot,
                    protected_paths,
                });
                true
            }
            Err(_) => {
                // A capture failure is a cache miss, not a successful reuse. The caller records
                // this bounded reason in the existing sandbox trace while the next command falls
                // back to the full resolver.
                self.cache = None;
                false
            }
        }
    }

    fn invalidate(&mut self) {
        self.cache = None;
    }
}

#[derive(Default)]
struct WorkspaceObservationSessions {
    sessions: HashMap<WorkspaceObservationSessionKey, Arc<Mutex<WorkspaceObservationSession>>>,
    insertion_order: VecDeque<WorkspaceObservationSessionKey>,
}

struct WorkspaceSnapshots {
    before: WorkspaceSnapshot,
    after: WorkspaceSnapshot,
    observation: WorkspaceChangeObservation,
    protected_paths: Vec<AbsolutePathBuf>,
    metrics: WorkspaceObservationMetrics,
}
type ObservedWorkspaceSnapshots = Option<WorkspaceSnapshots>;

#[derive(Debug)]
struct ObservedWorkspaceSnapshot {
    snapshot: WorkspaceSnapshot,
    metrics: WorkspaceObservationPhaseMetrics,
}

const WORKSPACE_CHANGE_SUMMARY_UNAVAILABLE: &str =
    "capability_not_supported:workspace_change_summary";
const TRUSTED_WORKSPACE_ROLLBACK_FAILED: &str = "trusted_workspace_rollback_failed";

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

#[derive(Clone, Default)]
/// Windows 严格 sandbox backend。
pub struct WindowsSandboxBackend {
    observation_sessions: Arc<Mutex<WorkspaceObservationSessions>>,
}

impl std::fmt::Debug for WindowsSandboxBackend {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WindowsSandboxBackend")
            .finish_non_exhaustive()
    }
}

impl WindowsSandboxBackend {
    /// 创建 Windows sandbox backend。
    pub fn new() -> Self {
        Self::default()
    }

    fn observation_session(
        &self,
        key: Option<WorkspaceObservationSessionKey>,
    ) -> Option<Arc<Mutex<WorkspaceObservationSession>>> {
        let key = key?;
        let mut sessions = self.observation_sessions.lock().ok()?;
        if let Some(session) = sessions.sessions.get(&key) {
            return Some(Arc::clone(session));
        }
        let session = Arc::new(Mutex::new(
            WorkspaceObservationSession::start(key.root.clone()).ok()?,
        ));
        if sessions.sessions.len() >= MAX_WORKSPACE_OBSERVATION_SESSIONS {
            let removable = sessions.insertion_order.iter().position(|oldest| {
                sessions
                    .sessions
                    .get(oldest)
                    .is_some_and(|session| Arc::strong_count(session) == 1)
            })?;
            let oldest = sessions.insertion_order.remove(removable)?;
            sessions.sessions.remove(&oldest);
        }
        sessions.insertion_order.push_back(key.clone());
        sessions.sessions.insert(key, Arc::clone(&session));
        Some(session)
    }

    /// Release every cached observation contract rooted at one canonical workspace.
    fn release_workspace_observation_for_root(&self, workspace: &Path) -> Result<(), String> {
        let root = release_observation_root(workspace)?;
        let mut sessions = self
            .observation_sessions
            .lock()
            .map_err(|_| "workspace observation session cache lock poisoned".to_string())?;
        let keys = sessions
            .sessions
            .keys()
            .filter(|key| key.root == root)
            .cloned()
            .collect::<Vec<_>>();

        for key in &keys {
            let session = sessions.sessions.get(key).ok_or_else(|| {
                "workspace observation session cache changed during release".to_string()
            })?;
            if Arc::strong_count(session) != 1 {
                return Err(format!(
                    "workspace observation session for {} still has active owners",
                    root.display()
                ));
            }
            let session_guard = session
                .lock()
                .map_err(|_| "workspace observation session lock poisoned".to_string())?;
            drop(session_guard);
        }

        for key in keys {
            sessions.sessions.remove(&key);
            sessions.insertion_order.retain(|entry| entry != &key);
        }
        Ok(())
    }

    fn probe_network_denied(
        &self,
        workspace: &Path,
        cancellation: &CancellationToken,
    ) -> Result<(), &'static str> {
        use std::net::TcpListener;

        if cancellation.is_cancelled() {
            return Err("sandbox_preflight_cancelled");
        }
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .map_err(|_| "sandbox_preflight_network_probe_unavailable")?;
        listener
            .set_nonblocking(true)
            .map_err(|_| "sandbox_preflight_network_probe_unavailable")?;
        let port = listener
            .local_addr()
            .map_err(|_| "sandbox_preflight_network_probe_unavailable")?
            .port();
        let script = format!(
            "$ErrorActionPreference='Stop'; $client=New-Object System.Net.Sockets.TcpClient; try {{ $client.Connect('127.0.0.1',{port}); exit 17 }} catch {{ exit 0 }}"
        );
        let result = self.run_preflight_script(
            workspace,
            "sandbox_preflight_network_denied",
            script,
            SandboxFilesystemMode::ReadOnly,
            SandboxNetworkMode::Denied,
            cancellation,
        );
        let connected = listener.accept().is_ok();
        if connected {
            return Err("sandbox_preflight_network_denied_unverified");
        }
        if cancellation.is_cancelled() {
            return Err("sandbox_preflight_cancelled");
        }
        if !strict_elevated_result(&result)
            || result.semantic_status != CommandSemanticStatus::Succeeded
        {
            return Err("sandbox_preflight_network_denied_unverified");
        }
        Ok(())
    }

    fn probe_protected_paths(
        &self,
        workspace: &Path,
        cancellation: &CancellationToken,
    ) -> Result<(), &'static str> {
        let protected = workspace.join(".git");
        let sentinel = protected.join("singularity-preflight-protected.txt");
        std::fs::create_dir_all(&protected)
            .map_err(|_| "sandbox_preflight_protected_probe_unavailable")?;
        std::fs::write(&sentinel, "singularity-protected-sentinel")
            .map_err(|_| "sandbox_preflight_protected_probe_unavailable")?;
        let read_result = self.run_preflight_script(
            workspace,
            "sandbox_preflight_protected_read",
            "$ErrorActionPreference='Stop'; try { $value=Get-Content -LiteralPath '.git\\singularity-preflight-protected.txt' -Raw; if ($value -match 'singularity-protected-sentinel') { exit 17 }; exit 18 } catch { exit 0 }".to_string(),
            SandboxFilesystemMode::ReadOnly,
            SandboxNetworkMode::Denied,
            cancellation,
        );
        if cancellation.is_cancelled() {
            return Err("sandbox_preflight_cancelled");
        }
        if !strict_elevated_result(&read_result)
            || read_result.semantic_status != CommandSemanticStatus::Succeeded
            || read_result
                .stdout_preview
                .contains("singularity-protected-sentinel")
        {
            return Err("sandbox_preflight_protected_probe_failed");
        }
        let write_result = self.run_preflight_script(
            workspace,
            "sandbox_preflight_protected_write",
            "$ErrorActionPreference='Stop'; try { Set-Content -LiteralPath '.git\\singularity-preflight-protected.txt' -Value 'tampered'; exit 17 } catch { exit 0 }".to_string(),
            SandboxFilesystemMode::WorkspaceWrite,
            SandboxNetworkMode::Denied,
            cancellation,
        );
        let preserved = std::fs::read_to_string(&sentinel).ok().as_deref()
            == Some("singularity-protected-sentinel");
        if cancellation.is_cancelled() {
            return Err("sandbox_preflight_cancelled");
        }
        if !preserved
            || !strict_elevated_result(&write_result)
            || write_result.semantic_status != CommandSemanticStatus::Succeeded
            || write_result.workspace_mutation == WorkspaceMutation::Changed
        {
            return Err("sandbox_preflight_protected_probe_failed");
        }
        Ok(())
    }

    fn probe_trusted_transaction(
        &self,
        workspace: &Path,
        cancellation: &CancellationToken,
    ) -> Result<(), &'static str> {
        if cancellation.is_cancelled() {
            return Err("sandbox_preflight_cancelled");
        }
        let mut request = CommandRequest::trusted_workspace_preparation(
            "sandbox_preflight_trusted_transaction",
            vec![
                "git".to_string(),
                "init".to_string(),
                "--quiet".to_string(),
                "source".to_string(),
            ],
            workspace.to_string_lossy().into_owned(),
            workspace.to_string_lossy().into_owned(),
        );
        request.timeout_seconds = 30;
        request.network.mode = SandboxNetworkMode::Denied;
        request.environment = CommandEnvironmentPolicy::Isolated;
        let result = self.execute_cancellable(&request, cancellation);
        if cancellation.is_cancelled() {
            return Err("sandbox_preflight_cancelled");
        }
        if !strict_elevated_result(&result)
            || result.semantic_status != CommandSemanticStatus::Succeeded
            || result.workspace_mutation != WorkspaceMutation::Changed
            || result.workspace_change_summary.is_none()
        {
            return Err("sandbox_preflight_trusted_preparation_unverified");
        }
        Ok(())
    }

    fn run_preflight_script(
        &self,
        workspace: &Path,
        command_id: &str,
        script: String,
        filesystem: SandboxFilesystemMode,
        network: SandboxNetworkMode,
        cancellation: &CancellationToken,
    ) -> CommandResult {
        let mut request = CommandScriptRequest::agent_requested_with_policy(
            command_id,
            script,
            workspace.to_string_lossy().into_owned(),
            workspace.to_string_lossy().into_owned(),
            filesystem,
            network,
        );
        request.environment = CommandEnvironmentPolicy::Isolated;
        self.execute_script_cancellable(&request, cancellation)
    }
}

fn strict_elevated_result(result: &CommandResult) -> bool {
    result.sandbox.backend == ELEVATED_BACKEND_NAME
        && result.sandbox.enforcement == SandboxBackendEnforcement::Strict
        && !result.sandbox.local_process_fallback
}

impl SandboxBackend for WindowsSandboxBackend {
    fn name(&self) -> &'static str {
        BACKEND_NAME
    }

    fn capabilities(&self) -> SandboxCapabilities {
        SandboxCapabilities::strict().with_change_detection()
    }

    fn preflight(
        &self,
        workspace: &Path,
        cancellation: &CancellationToken,
    ) -> SandboxPreflightReport {
        let mut report = super::baseline_sandbox_preflight(self);
        report.os = "windows".to_string();
        report.kernel = windows_kernel_fact();
        report.filesystem = windows_filesystem_fact(workspace);
        if report.outcome == super::SandboxPreflightOutcome::Unsupported {
            return report;
        }
        if let Some(code) = windows_filesystem_gate_error(report.filesystem.as_deref()) {
            report.unsupported(code, &["ntfs_workspace"]);
            return report;
        }
        if cancellation.is_cancelled() {
            report.unsupported("sandbox_preflight_cancelled", &["cancellation"]);
            return report;
        }
        if let Err(code) = self.probe_network_denied(workspace, cancellation) {
            report.unsupported(code, &["network_denied"]);
            return report;
        }
        report.network_denied = SandboxPreflightFact::Passed;
        if let Err(code) = self.probe_trusted_transaction(workspace, cancellation) {
            report.unsupported(
                code,
                &["transactional_workspace", "trusted_workspace_preparation"],
            );
            return report;
        }
        report.transactional_workspace = SandboxPreflightFact::Passed;
        if let Err(code) = self.probe_protected_paths(workspace, cancellation) {
            report.unsupported(code, &["protected_metadata_admission"]);
            return report;
        }
        report.protected_paths = SandboxPreflightFact::Passed;
        report.outcome = super::SandboxPreflightOutcome::Supported;
        report.error_code = None;
        report
    }

    fn probe_executable(
        &self,
        workspace: &Path,
        executable: &str,
        environment: &CommandEnvironmentPolicy,
    ) -> ExecutableAvailability {
        let Ok(cwd) = canonical_directory(workspace) else {
            return ExecutableAvailability::Unknown;
        };
        let Ok(env) = child_environment(environment, &cwd) else {
            return ExecutableAvailability::Unknown;
        };
        match resolve_executable(&[executable.to_string()], &cwd, &env) {
            Ok(_) => ExecutableAvailability::Available,
            Err(ExecutableResolutionError::Unavailable(_)) => ExecutableAvailability::Unavailable,
            Err(_) => ExecutableAvailability::Unknown,
        }
    }

    fn execute(&self, request: &CommandRequest) -> CommandResult {
        self.execute_cancellable(request, &CancellationToken::new())
    }

    fn release_workspace_observation(&self, workspace: &Path) -> Result<(), String> {
        self.release_workspace_observation_for_root(workspace)
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
        if let Some(code) =
            windows_workspace_filesystem_gate_error(Path::new(&request.filesystem.workspace_root))
        {
            return CommandResult::unsupported(&request.command_id, code)
                .with_workspace_mutation(WorkspaceMutation::Unknown)
                .with_sandbox_execution(self.name(), SandboxBackendEnforcement::Unavailable);
        }
        let observation_session_key = request_observation_session_key(request);
        let observation_session = self.observation_session(observation_session_key);
        let mut observation_session = observation_session
            .as_ref()
            .and_then(|session| session.lock().ok());
        let (before_seed, monitor, cached_protected_paths, workspace_root_lease) =
            match observation_session
                .as_deref_mut()
                .map(WorkspaceObservationSession::prepare_for_command)
            {
                Some(Ok(preparation)) => preparation.into_execution_parts(),
                Some(Err(_)) => {
                    if let Some(session) = observation_session.as_deref_mut() {
                        session.invalidate();
                    }
                    (None, None, None, None)
                }
                None => (None, None, None, None),
            };
        let prepared = match PreparedCommand::from_request(
            request,
            cached_protected_paths.as_deref(),
            workspace_root_lease,
        ) {
            Ok(prepared) => prepared,
            Err(PrepareCommandError::Executable(error)) => {
                if let Some(session) = observation_session.as_deref_mut() {
                    session.invalidate();
                }
                return error
                    .into_command_result(&request.command_id)
                    .with_sandbox_execution(self.name(), SandboxBackendEnforcement::Strict);
            }
            Err(PrepareCommandError::Backend(error)) => {
                if let Some(session) = observation_session.as_deref_mut() {
                    session.invalidate();
                }
                return CommandResult::backend_error(&request.command_id, error)
                    .with_workspace_mutation(WorkspaceMutation::Unknown)
                    .with_sandbox_execution(self.name(), SandboxBackendEnforcement::Unavailable);
            }
            Err(PrepareCommandError::ProtectedPaths(error)) => {
                if let Some(session) = observation_session.as_deref_mut() {
                    session.invalidate();
                }
                return CommandResult::backend_error(&request.command_id, error)
                    .with_workspace_mutation(WorkspaceMutation::Unknown)
                    .with_sandbox_execution(self.name(), SandboxBackendEnforcement::Unavailable);
            }
            Err(PrepareCommandError::WorkspaceObservation) => {
                if let Some(session) = observation_session.as_deref_mut() {
                    session.invalidate();
                }
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
            should_monitor_workspace_change(request),
            monitor,
            before_seed,
            cached_protected_paths,
            observation_session.as_deref_mut(),
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
        if let Some(code) =
            windows_workspace_filesystem_gate_error(Path::new(&request.filesystem.workspace_root))
        {
            return CommandResult::unsupported(&request.command_id, code)
                .with_workspace_mutation(WorkspaceMutation::Unknown)
                .with_sandbox_execution(self.name(), SandboxBackendEnforcement::Unavailable);
        }
        let observation_session_key = script_observation_session_key(request);
        let observation_session = self.observation_session(observation_session_key);
        let mut observation_session = observation_session
            .as_ref()
            .and_then(|session| session.lock().ok());
        let (before_seed, monitor, cached_protected_paths, workspace_root_lease) =
            match observation_session
                .as_deref_mut()
                .map(WorkspaceObservationSession::prepare_for_command)
            {
                Some(Ok(preparation)) => preparation.into_execution_parts(),
                Some(Err(_)) => {
                    if let Some(session) = observation_session.as_deref_mut() {
                        session.invalidate();
                    }
                    (None, None, None, None)
                }
                None => (None, None, None, None),
            };
        let prepared = match PreparedCommand::from_script_request(
            request,
            cached_protected_paths.as_deref(),
            workspace_root_lease,
        ) {
            Ok(prepared) => prepared,
            Err(PrepareCommandError::Executable(error)) => {
                if let Some(session) = observation_session.as_deref_mut() {
                    session.invalidate();
                }
                return error
                    .into_command_result(&request.command_id)
                    .with_sandbox_execution(self.name(), SandboxBackendEnforcement::Strict);
            }
            Err(PrepareCommandError::Backend(error)) => {
                if let Some(session) = observation_session.as_deref_mut() {
                    session.invalidate();
                }
                return CommandResult::backend_error(&request.command_id, error)
                    .with_workspace_mutation(WorkspaceMutation::Unknown)
                    .with_sandbox_execution(self.name(), SandboxBackendEnforcement::Unavailable);
            }
            Err(PrepareCommandError::ProtectedPaths(error)) => {
                if let Some(session) = observation_session.as_deref_mut() {
                    session.invalidate();
                }
                return CommandResult::backend_error(&request.command_id, error)
                    .with_workspace_mutation(WorkspaceMutation::Unknown)
                    .with_sandbox_execution(self.name(), SandboxBackendEnforcement::Unavailable);
            }
            Err(PrepareCommandError::WorkspaceObservation) => {
                if let Some(session) = observation_session.as_deref_mut() {
                    session.invalidate();
                }
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
            monitor,
            before_seed,
            cached_protected_paths,
            observation_session.as_deref_mut(),
        )
    }
}

#[cfg(windows)]
fn windows_filesystem_fact(workspace: &Path) -> Option<String> {
    use windows_sys::Win32::Storage::FileSystem::{GetVolumeInformationW, GetVolumePathNameW};

    let path = workspace
        .as_os_str()
        .encode_wide()
        .chain(once(0))
        .collect::<Vec<_>>();
    let mut volume = [0u16; 261];
    let volume_len = u32::try_from(volume.len()).ok()?;
    // SAFETY: all pointers refer to writable buffers with explicit lengths and are NUL
    // terminated where required by the Win32 API.
    let resolved = unsafe { GetVolumePathNameW(path.as_ptr(), volume.as_mut_ptr(), volume_len) };
    if resolved == 0 {
        return None;
    }
    let mut filesystem = [0u16; 64];
    let filesystem_len = u32::try_from(filesystem.len()).ok()?;
    // SAFETY: `volume` and `filesystem` are valid NUL-terminated/writable buffers for the
    // duration of this call; unused serial/flags outputs are passed as null pointers.
    let queried = unsafe {
        GetVolumeInformationW(
            volume.as_ptr(),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            filesystem.as_mut_ptr(),
            filesystem_len,
        )
    };
    if queried == 0 {
        return None;
    }
    let length = filesystem
        .iter()
        .position(|value| *value == 0)
        .unwrap_or(filesystem.len());
    Some(
        String::from_utf16_lossy(&filesystem[..length])
            .chars()
            .take(64)
            .collect(),
    )
}

fn windows_filesystem_gate_error(filesystem: Option<&str>) -> Option<&'static str> {
    match filesystem {
        Some(filesystem) if filesystem.eq_ignore_ascii_case("NTFS") => None,
        Some(_) => Some("sandbox_unsupported_filesystem"),
        None => Some("sandbox_filesystem_unknown"),
    }
}

fn windows_workspace_filesystem_gate_error(workspace: &Path) -> Option<&'static str> {
    windows_filesystem_gate_error(windows_filesystem_fact(workspace).as_deref())
}

#[cfg(windows)]
fn windows_kernel_fact() -> Option<String> {
    #[repr(C)]
    struct RtlOsVersionInfo {
        size: u32,
        major: u32,
        minor: u32,
        build: u32,
        platform: u32,
        service_pack: [u16; 128],
    }

    #[link(name = "ntdll")]
    unsafe extern "system" {
        fn RtlGetVersion(info: *mut RtlOsVersionInfo) -> i32;
    }

    let mut info = RtlOsVersionInfo {
        size: std::mem::size_of::<RtlOsVersionInfo>() as u32,
        major: 0,
        minor: 0,
        build: 0,
        platform: 0,
        service_pack: [0; 128],
    };
    // SAFETY: `info` is initialized with its structure size and remains valid for the call.
    let status = unsafe { RtlGetVersion(&mut info) };
    (status == 0).then(|| format!("{}.{}.{}", info.major, info.minor, info.build))
}

fn should_monitor_workspace_change(request: &CommandRequest) -> bool {
    matches!(
        request.filesystem.mode,
        SandboxFilesystemMode::WorkspaceWrite
    ) && !request.is_trusted_workspace_preparation()
}

fn request_observation_session_key(
    request: &CommandRequest,
) -> Option<WorkspaceObservationSessionKey> {
    if !should_monitor_workspace_change(request) {
        return None;
    }
    observation_session_key(
        &request.filesystem.workspace_root,
        &request.filesystem.mode,
        &request.network.mode,
        &request.environment,
    )
}

fn script_observation_session_key(
    request: &CommandScriptRequest,
) -> Option<WorkspaceObservationSessionKey> {
    if !matches!(
        request.filesystem.mode,
        SandboxFilesystemMode::WorkspaceWrite
    ) {
        return None;
    }
    observation_session_key(
        &request.filesystem.workspace_root,
        &request.filesystem.mode,
        &request.network.mode,
        &request.environment,
    )
}

fn observation_session_key(
    workspace_root: &str,
    filesystem: &SandboxFilesystemMode,
    network: &SandboxNetworkMode,
    environment: &CommandEnvironmentPolicy,
) -> Option<WorkspaceObservationSessionKey> {
    let root = canonical_directory(Path::new(workspace_root)).ok()?;
    let contract = serde_json::to_string(&(
        filesystem,
        network,
        environment,
        WORKSPACE_OBSERVATION_CONTRACT,
    ))
    .ok()?;
    Some(WorkspaceObservationSessionKey { root, contract })
}

#[allow(clippy::too_many_arguments)]
fn execute_prepared_command(
    command_id: &str,
    cancellation: &CancellationToken,
    prepared: PreparedCommand,
    observe_workspace_change: bool,
    monitor: Option<WorkspaceChangeMonitor>,
    before_seed: Option<BeforeSnapshotSeed>,
    cached_protected_paths: Option<Vec<AbsolutePathBuf>>,
    mut observation_session: Option<&mut WorkspaceObservationSession>,
) -> CommandResult {
    let mut prepared = prepared;
    let workspace = prepared.workspace_roots[0].as_path().to_path_buf();
    let sandbox_home_for_observation_log = prepared.sandbox_home.clone();
    let before = prepared.before.clone();
    let protect_workspace_metadata = prepared.protect_workspace_metadata;
    let mut trusted_lease = prepared.trusted_workspace.take();
    let trusted_workspace = trusted_lease.is_some();
    let mut monitor = monitor;
    let (result, observed_snapshots) = match execute_windows_sandbox(
        command_id,
        cancellation,
        prepared,
        trusted_lease.as_ref(),
        observe_workspace_change.then_some(&mut monitor),
        before_seed,
        cached_protected_paths,
    ) {
        Ok(result) => result,
        Err(error) => {
            if let Some(session) = observation_session.as_deref_mut() {
                session.invalidate();
            }
            if let Some(mut lease) = trusted_lease.take()
                && let Err(rollback) = lease.rollback()
            {
                return CommandResult::backend_error(
                    command_id,
                    format!("{TRUSTED_WORKSPACE_ROLLBACK_FAILED}: {}", rollback.code()),
                )
                .with_workspace_mutation(WorkspaceMutation::Unknown)
                .with_sandbox_execution(BACKEND_NAME, SandboxBackendEnforcement::Unavailable);
            }
            return CommandResult::backend_error(command_id, error)
                .with_workspace_mutation(WorkspaceMutation::Unknown)
                .with_sandbox_execution(BACKEND_NAME, SandboxBackendEnforcement::Unavailable);
        }
    };
    let result = match observed_snapshots.as_ref() {
        Some(snapshots) => result.with_workspace_observation_metrics(snapshots.metrics.clone()),
        None => result,
    };
    if matches!(
        result.execution_status,
        CommandExecutionStatus::Cancelled | CommandExecutionStatus::TimedOut
    ) {
        if let Some(session) = observation_session.as_deref_mut() {
            session.invalidate();
        }
    } else if let (Some(session), Some(snapshots)) =
        (observation_session, observed_snapshots.as_ref())
    {
        if monitor.is_some() {
            let cached = session.publish_with_protected_paths(
                snapshots.after.clone(),
                snapshots.protected_paths.clone(),
                monitor.take(),
            );
            if !cached {
                singularity_windows_sandbox::log_note(
                    "OBSERVATION_CACHE protected_path_capture_failed; next command uses full resolver",
                    Some(&sandbox_home_for_observation_log),
                );
            }
        } else {
            session.invalidate();
        }
    }
    if trusted_workspace {
        let Some(mut lease) = trusted_lease.take() else {
            return CommandResult::backend_error(
                command_id,
                TrustedWorkspaceError::AlreadyFinalized.code(),
            )
            .with_workspace_mutation(WorkspaceMutation::Unknown)
            .with_sandbox_execution(BACKEND_NAME, SandboxBackendEnforcement::Unavailable);
        };
        if !trusted_command_succeeded(&result) {
            if let Err(rollback) = lease.rollback() {
                return CommandResult::backend_error(
                    command_id,
                    format!("{TRUSTED_WORKSPACE_ROLLBACK_FAILED}: {}", rollback.code()),
                )
                .with_workspace_mutation(WorkspaceMutation::Unknown)
                .with_sandbox_execution(BACKEND_NAME, SandboxBackendEnforcement::Unavailable);
            }
            return result.with_workspace_mutation(WorkspaceMutation::Unknown);
        }
        let summary = before
            .as_ref()
            .ok_or_else(|| "trusted workspace baseline unavailable".to_string())
            .and_then(|before| {
                lease
                    .duplicate_root_handle()
                    .map_err(|error| error.code().to_string())
                    .and_then(|handle| snapshot_trusted_workspace_from_handle(&handle))
                    .and_then(|after| before.trusted_change_summary(&after))
            });
        let summary = match summary {
            Ok(summary) => summary,
            Err(_) => {
                return match lease.rollback() {
                    Ok(()) => CommandResult::backend_error(
                        command_id,
                        TrustedWorkspaceError::RootDrift.code(),
                    )
                    .with_workspace_mutation(WorkspaceMutation::Unknown)
                    .with_sandbox_execution(BACKEND_NAME, SandboxBackendEnforcement::Unavailable),
                    Err(rollback) => CommandResult::backend_error(
                        command_id,
                        format!("{TRUSTED_WORKSPACE_ROLLBACK_FAILED}: {}", rollback.code()),
                    )
                    .with_workspace_mutation(WorkspaceMutation::Unknown)
                    .with_sandbox_execution(BACKEND_NAME, SandboxBackendEnforcement::Unavailable),
                };
            }
        };
        if let Err(error) = lease.commit() {
            return match lease.rollback() {
                Ok(()) => CommandResult::backend_error(command_id, error.code())
                    .with_workspace_mutation(WorkspaceMutation::Unknown)
                    .with_sandbox_execution(BACKEND_NAME, SandboxBackendEnforcement::Unavailable),
                Err(rollback) => CommandResult::backend_error(
                    command_id,
                    format!("{TRUSTED_WORKSPACE_ROLLBACK_FAILED}: {}", rollback.code()),
                )
                .with_workspace_mutation(WorkspaceMutation::Unknown)
                .with_sandbox_execution(BACKEND_NAME, SandboxBackendEnforcement::Unavailable),
            };
        }
        return match summary {
            Some(summary) => result
                .with_workspace_mutation(WorkspaceMutation::Changed)
                .with_workspace_change_summary(summary),
            None => result.with_workspace_mutation(WorkspaceMutation::Unchanged),
        };
    }
    let observation = observed_snapshots
        .as_ref()
        .map(|snapshots| Ok(snapshots.observation.clone()))
        .or_else(|| monitor.map(|monitor| monitor.finish().map_err(|_| ())));
    let (mutation, summary) = if protect_workspace_metadata {
        let snapshot_change =
            observed_snapshots.map(|snapshots| snapshots.before.observed_change(&snapshots.after));
        reconcile_workspace_change(observation, snapshot_change)
    } else {
        let mutation = match (before, snapshot_trusted_workspace(&workspace)) {
            (Some(before), Ok(after)) if before == after => WorkspaceMutation::Unchanged,
            (Some(_), Ok(_)) => WorkspaceMutation::Changed,
            _ => WorkspaceMutation::Unknown,
        };
        (mutation, None)
    };
    let result = result.with_workspace_mutation(mutation);
    match summary {
        Some(summary) => result.with_workspace_change_summary(summary),
        None => result,
    }
}

fn trusted_command_succeeded(result: &CommandResult) -> bool {
    result.execution_status == CommandExecutionStatus::Completed
        && result.semantic_status == CommandSemanticStatus::Succeeded
        && result.sandbox.backend == ELEVATED_BACKEND_NAME
        && result.sandbox.enforcement == SandboxBackendEnforcement::Strict
        && !result.sandbox.local_process_fallback
}

fn reconcile_workspace_change(
    observation: Option<Result<WorkspaceChangeObservation, ()>>,
    snapshot_change: Option<Result<(bool, Option<WorkspaceChangeSummary>), String>>,
) -> (WorkspaceMutation, Option<WorkspaceChangeSummary>) {
    match (observation, snapshot_change) {
        (None, Some(Ok((false, None)))) => (WorkspaceMutation::Unchanged, None),
        (None, Some(Ok((true, summary)))) => (WorkspaceMutation::Changed, summary),
        (Some(Ok(WorkspaceChangeObservation::Unchanged)), Some(Ok((false, None)))) => {
            (WorkspaceMutation::Unchanged, None)
        }
        (
            Some(Ok(WorkspaceChangeObservation::Changed(_) | WorkspaceChangeObservation::Unknown))
            | Some(Err(())),
            Some(Ok((true, summary))),
        ) => (WorkspaceMutation::Changed, summary),
        _ => (WorkspaceMutation::Unknown, None),
    }
}

/// Reconcile setup notifications against the complete workspace snapshot.
///
/// Security-descriptor churn from protected ACL setup is allowed only when the snapshot proves
/// that content, structure, and object identity stayed unchanged.
fn reconcile_protected_setup_change(
    before: &WorkspaceSnapshot,
    after: &WorkspaceSnapshot,
) -> Result<(), String> {
    let (changed, _) = before.observed_change(after)?;
    if changed {
        Err("workspace changed during protected setup".to_string())
    } else {
        Ok(())
    }
}

/// 将 core 的 protected path 规则投影为 resolver 可展开的 workspace glob。
fn resolve_existing_protected_paths(
    workspace_root: &AbsolutePathBuf,
    trusted_workspace: Option<&TrustedWorkspaceLease>,
) -> Result<(Vec<AbsolutePathBuf>, bool), String> {
    #[cfg(test)]
    FULL_PROTECTED_RESOLVER_SCANS.with(|count| count.set(count.get().saturating_add(1)));
    let entries = protected_path_glob_entries(workspace_root);
    let policy = FileSystemSandboxPolicy::restricted(entries);
    if let Some(trusted_workspace) = trusted_workspace {
        resolve_windows_deny_read_paths_for_controller_with_pinned_workspace_root(
            &policy,
            workspace_root,
            trusted_workspace,
        )
    } else {
        resolve_windows_deny_read_paths_for_controller(&policy, workspace_root)
    }
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
            missing_path_behavior: None,
        })
        .collect()
}

fn format_workspace_protected_glob(
    workspace_root: &AbsolutePathBuf,
    component_pattern: &str,
) -> String {
    let root = workspace_root.to_string_lossy().replace('\\', "/");
    let root = escape_glob_literal(&root);
    let separator = if root.ends_with('/') { "" } else { "/" };
    format!("{root}{separator}**/{component_pattern}")
}

fn escape_glob_literal(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        if matches!(character, '?' | '*' | '[' | ']' | '{' | '}') {
            escaped.push('[');
            escaped.push(character);
            escaped.push(']');
        } else {
            escaped.push(character);
        }
    }
    escaped
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
    trusted_deny_write_paths: Vec<AbsolutePathBuf>,
    protected_path_scan_incomplete: bool,
    protect_workspace_metadata: bool,
    before: Option<WorkspaceSnapshot>,
    trusted_workspace: Option<TrustedWorkspaceLease>,
    workspace_root_lease: Option<WorkspaceRootLease>,
}

impl PreparedCommand {
    fn from_request(
        request: &CommandRequest,
        cached_protected_paths: Option<&[AbsolutePathBuf]>,
        workspace_root_lease: Option<WorkspaceRootLease>,
    ) -> Result<Self, PrepareCommandError> {
        let workspace_root = canonical_directory(Path::new(&request.filesystem.workspace_root))
            .map_err(PrepareCommandError::Backend)?;
        let protect_workspace_metadata = !request.is_trusted_workspace_preparation();
        let cwd =
            canonical_directory(Path::new(&request.cwd)).map_err(PrepareCommandError::Backend)?;
        let workspace_write = matches!(
            request.filesystem.mode,
            SandboxFilesystemMode::WorkspaceWrite
        );
        let workspace_root_lease = if request.is_trusted_workspace_preparation() && workspace_write
        {
            // Trusted workspace-write preparation has its own DELETE-capable lease; never overlap
            // it with the controller observation lease, whose no-delete handle would reject
            // acquisition. Trusted read-only requests still need the ordinary ancestor lease.
            drop(workspace_root_lease);
            None
        } else {
            match workspace_root_lease {
                Some(lease) => Some(lease),
                None => Some(
                    WorkspaceRootLease::acquire(&workspace_root)
                        .map_err(|error| PrepareCommandError::Backend(error.to_string()))?,
                ),
            }
        };
        let mut trusted_workspace = if request.is_trusted_workspace_preparation() && workspace_write
        {
            Some(
                TrustedWorkspaceLease::acquire(&workspace_root)
                    .map_err(|error| PrepareCommandError::Backend(error.code().to_string()))?,
            )
        } else {
            None
        };
        let env_map = child_environment(&request.environment, &workspace_root)
            .map_err(PrepareCommandError::Backend)?;
        let resolved = resolve_executable(&request.argv, &cwd, &env_map)
            .map_err(PrepareCommandError::Executable)?;
        let workspace_root =
            AbsolutePathBuf::from_absolute_path_checked(&workspace_root).map_err(|error| {
                PrepareCommandError::Backend(format!("invalid workspace root: {error}"))
            })?;
        let (resolved_protected_paths, protected_path_scan_incomplete) =
            if let Some(cached) = cached_protected_paths {
                (cached.to_vec(), false)
            } else if protect_workspace_metadata
                || matches!(
                    request.filesystem.mode,
                    SandboxFilesystemMode::WorkspaceWrite
                )
            {
                resolve_existing_protected_paths(&workspace_root, trusted_workspace.as_ref())
                    .map_err(PrepareCommandError::ProtectedPaths)?
            } else {
                (Vec::new(), false)
            };
        if protected_path_scan_incomplete && !protect_workspace_metadata {
            return Err(PrepareCommandError::ProtectedPaths(
                "protected path scan requires the sandbox identity".to_string(),
            ));
        }
        let protected_deny_read_paths = if protect_workspace_metadata {
            resolved_protected_paths.clone()
        } else {
            Vec::new()
        };
        let protected_deny_write_paths = if matches!(
            request.filesystem.mode,
            SandboxFilesystemMode::WorkspaceWrite
        ) && protect_workspace_metadata
        {
            resolved_protected_paths.clone()
        } else {
            Vec::new()
        };
        let trusted_deny_write_paths = if request.is_trusted_workspace_preparation()
            && matches!(
                request.filesystem.mode,
                SandboxFilesystemMode::WorkspaceWrite
            ) {
            resolved_protected_paths
        } else {
            Vec::new()
        };
        let workspace_roots = vec![workspace_root.clone()];
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
        let resolved_permissions = singularity_windows_sandbox::ResolvedWindowsSandboxPermissions::try_from_permission_profile_for_workspace_roots(
            &permission_profile,
            &workspace_roots,
        )
        .map_err(|error| {
            PrepareCommandError::Backend(format!(
                "invalid Windows sandbox permissions: {error}"
            ))
        })?;
        let restricted_token_fallback = resolved_permissions.supports_restricted_token_fallback()
            && protected_deny_read_paths.is_empty()
            && protect_workspace_metadata;
        let sandbox_home = sandbox_home().map_err(PrepareCommandError::Backend)?;
        let before = if matches!(
            request.filesystem.mode,
            SandboxFilesystemMode::WorkspaceWrite
        ) {
            if protect_workspace_metadata {
                None
            } else {
                match trusted_workspace
                    .as_ref()
                    .ok_or_else(|| "trusted workspace lease unavailable".to_string())
                    .and_then(|lease| {
                        lease
                            .duplicate_root_handle()
                            .map_err(|error| error.code().to_string())
                    })
                    .and_then(|handle| snapshot_trusted_workspace_from_handle(&handle))
                {
                    Ok(snapshot) => Some(snapshot),
                    Err(_) => {
                        if let Some(lease) = trusted_workspace.as_mut()
                            && let Err(error) = lease.rollback()
                        {
                            return Err(PrepareCommandError::Backend(format!(
                                "{TRUSTED_WORKSPACE_ROLLBACK_FAILED}: {}",
                                error.code()
                            )));
                        }
                        return Err(PrepareCommandError::WorkspaceObservation);
                    }
                }
            }
        } else {
            None
        };
        Ok(Self {
            permission_profile,
            workspace_roots,
            sandbox_home,
            cwd,
            env_map,
            timeout_ms: request.timeout_seconds.saturating_mul(1_000),
            restricted_token_fallback,
            argv: resolved.argv,
            read_roots: resolved.read_roots,
            protected_deny_read_paths,
            protected_deny_write_paths,
            trusted_deny_write_paths,
            protected_path_scan_incomplete,
            protect_workspace_metadata,
            before,
            trusted_workspace,
            workspace_root_lease,
        })
    }

    fn from_script_request(
        request: &CommandScriptRequest,
        cached_protected_paths: Option<&[AbsolutePathBuf]>,
        workspace_root_lease: Option<WorkspaceRootLease>,
    ) -> Result<Self, PrepareCommandError> {
        let workspace_root = canonical_directory(Path::new(&request.filesystem.workspace_root))
            .map_err(PrepareCommandError::Backend)?;
        let before = None;
        let cwd =
            canonical_directory(Path::new(&request.cwd)).map_err(PrepareCommandError::Backend)?;
        let env_map = child_environment(&request.environment, &workspace_root)
            .map_err(PrepareCommandError::Backend)?;
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
        let workspace_root_lease = match workspace_root_lease {
            Some(lease) => Some(lease),
            None => Some(
                WorkspaceRootLease::acquire(workspace_root.as_path())
                    .map_err(|error| PrepareCommandError::Backend(error.to_string()))?,
            ),
        };
        let (protected_deny_read_paths, protected_path_scan_incomplete) =
            if let Some(cached) = cached_protected_paths {
                (cached.to_vec(), false)
            } else {
                resolve_existing_protected_paths(&workspace_root, None)
                    .map_err(PrepareCommandError::ProtectedPaths)?
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
        let resolved_permissions = singularity_windows_sandbox::ResolvedWindowsSandboxPermissions::try_from_permission_profile_for_workspace_roots(
            &permission_profile,
            &workspace_roots,
        )
        .map_err(|error| {
            PrepareCommandError::Backend(format!(
                "invalid Windows sandbox permissions: {error}"
            ))
        })?;
        let restricted_token_fallback = resolved_permissions.supports_restricted_token_fallback()
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
            trusted_deny_write_paths: Vec::new(),
            protected_path_scan_incomplete,
            protect_workspace_metadata: true,
            before,
            trusted_workspace: None,
            workspace_root_lease,
        })
    }
}

/// 将准备阶段的 deny-read 集合绑定到现有 elevated capture 请求。
fn elevated_capture_request<'a>(
    prepared: &'a PreparedCommand,
    windows_cancellation: WindowsSandboxCancellationToken,
    trusted_workspace: Option<&'a TrustedWorkspaceLease>,
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
    elevated.protected_path_scan_incomplete = prepared.protected_path_scan_incomplete;
    elevated.trusted_deny_write_paths_override = &prepared.trusted_deny_write_paths;
    elevated.protect_workspace_metadata = prepared.protect_workspace_metadata;
    elevated.trusted_workspace = trusted_workspace;
    elevated.workspace_root_lease = prepared.workspace_root_lease.as_ref();
    elevated.workspace_change_monitor = workspace_change_monitor;
    elevated
}

fn execute_windows_sandbox(
    command_id: &str,
    cancellation: &CancellationToken,
    prepared: PreparedCommand,
    trusted_workspace: Option<&TrustedWorkspaceLease>,
    workspace_change_monitor: Option<&mut Option<WorkspaceChangeMonitor>>,
    before_seed: Option<BeforeSnapshotSeed>,
    cached_protected_paths: Option<Vec<AbsolutePathBuf>>,
) -> Result<(CommandResult, ObservedWorkspaceSnapshots), String> {
    let started = Instant::now();
    let observe_final_snapshot =
        prepared.protect_workspace_metadata && workspace_change_monitor.is_some();
    let workspace = prepared.workspace_roots[0].as_path().to_path_buf();
    let windows_cancellation = WindowsSandboxCancellationToken::new({
        let cancellation = cancellation.clone();
        move || cancellation.is_cancelled()
    });
    let elevated =
        if observe_final_snapshot {
            let workspace_for_observer = workspace.clone();
            let setup_before = resolve_before_workspace_snapshot(
                &workspace,
                before_seed.clone(),
                &prepared
                    .protected_deny_read_paths
                    .iter()
                    .map(|path| path.as_path().to_path_buf())
                    .collect::<Vec<_>>(),
            )?;
            let before_snapshot = Arc::new(Mutex::new(None::<WorkspaceSnapshot>));
            let before_snapshot_for_observer = Arc::clone(&before_snapshot);
            let before_snapshot_for_after = Arc::clone(&before_snapshot);
            let observation_protected_paths = Arc::new(Mutex::new(Vec::<PathBuf>::new()));
            let protected_paths_for_resolver = Arc::clone(&observation_protected_paths);
            let protected_paths_for_after = Arc::clone(&observation_protected_paths);
            let mut accumulated_after_metrics = None;
            run_windows_sandbox_capture_for_permission_profile_with_observations_elevated(
                elevated_capture_request(
                    &prepared,
                    windows_cancellation.clone(),
                    trusted_workspace,
                    workspace_change_monitor,
                ),
                {
                    let workspace = prepared.workspace_roots[0].clone();
                    // The controller resolved these objects before sandbox setup. Seed the
                    // sandbox-side bounded scans with that same protected set so a later command
                    // does not try to enumerate a directory that an earlier command already made
                    // unreadable to the sandbox principal. The setup transaction still pins and
                    // revalidates every returned object before the child can start.
                    let mut previously_protected = prepared.protected_deny_read_paths.clone();
                    let cached_protected_paths = cached_protected_paths.clone();
                    move || {
                        let protected = if let Some(cached) = cached_protected_paths.as_ref() {
                            cached.clone()
                        } else {
                            let entries = protected_path_glob_entries(&workspace);
                            let policy = FileSystemSandboxPolicy::restricted(entries);
                            resolve_windows_deny_read_paths_from_validated_workspace(
                                &policy,
                                &workspace,
                                &previously_protected,
                            )?
                        }
                        .into_iter()
                        .filter_map(|path| match std::fs::symlink_metadata(path.as_path()) {
                            Ok(_) => Some(Ok(path)),
                            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                                Some(Ok(path))
                            }
                            Err(error) => Some(Err(format!(
                                "protected workspace path revalidation failed: {error}"
                            ))),
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                        previously_protected = protected.clone();
                        *protected_paths_for_resolver.lock().map_err(|_| {
                            "protected workspace observation path lock poisoned".to_string()
                        })? = protected
                            .iter()
                            .map(|path| path.as_path().to_path_buf())
                            .collect();
                        Ok((protected.clone(), protected))
                    }
                },
                {
                    let mut setup_before = Some(setup_before);
                    move || {
                        let observed = setup_before.take().ok_or_else(|| {
                            "workspace before snapshot already captured".to_string()
                        })?;
                        *before_snapshot_for_observer.lock().map_err(|_| {
                            "workspace snapshot handoff lock poisoned".to_string()
                        })? = Some(observed.snapshot.clone());
                        Ok(observed)
                    }
                },
                {
                    let mut setup_reconciliation_pending = true;
                    move |observation| {
                        let before = before_snapshot_for_after
                            .lock()
                            .map_err(|_| "workspace snapshot handoff lock poisoned".to_string())?
                            .as_ref()
                            .cloned()
                            .ok_or_else(|| "workspace before snapshot unavailable".to_string())?;
                        let protected_paths = protected_paths_for_after
                            .lock()
                            .map_err(|_| {
                                "protected workspace observation path lock poisoned".to_string()
                            })?
                            .clone();
                        let mut observed = resolve_after_workspace_snapshot(
                            &workspace_for_observer,
                            &before,
                            &observation,
                            &protected_paths,
                        )?;
                        if setup_reconciliation_pending {
                            reconcile_protected_setup_change(&before, &observed.snapshot)?;
                            setup_reconciliation_pending = false;
                        }
                        *before_snapshot_for_after.lock().map_err(|_| {
                            "workspace snapshot handoff lock poisoned".to_string()
                        })? = Some(observed.snapshot.clone());
                        observed.metrics = match accumulated_after_metrics {
                            Some(previous) => {
                                merge_observation_phase_metrics(previous, observed.metrics)
                            }
                            None => observed.metrics,
                        };
                        accumulated_after_metrics = Some(observed.metrics);
                        Ok(observed)
                    }
                },
            )
            .and_then(|(capture, before, after, observation)| {
                let protected_paths = observation_protected_paths
                    .lock()
                    .map_err(|_| {
                        std::io::Error::other("protected workspace observation path lock poisoned")
                    })?
                    .iter()
                    .map(|path| {
                        AbsolutePathBuf::from_absolute_path_checked(path).map_err(|error| {
                            std::io::Error::other(format!(
                                "protected workspace path is invalid: {error}"
                            ))
                        })
                    })
                    .collect::<Result<Vec<_>, std::io::Error>>()?;
                Ok((
                    capture,
                    Some(WorkspaceSnapshots {
                        before: before.snapshot,
                        after: after.snapshot,
                        observation,
                        protected_paths,
                        metrics: WorkspaceObservationMetrics {
                            contract: WORKSPACE_OBSERVATION_CONTRACT.to_string(),
                            before: before.metrics,
                            after: after.metrics,
                        },
                    }),
                ))
            })
        } else {
            run_windows_sandbox_capture_for_permission_profile_elevated(elevated_capture_request(
                &prepared,
                windows_cancellation.clone(),
                trusted_workspace,
                workspace_change_monitor,
            ))
            .map(|capture| (capture, None))
        };
    match elevated {
        Ok((capture, after)) => Ok((
            command_result_from_capture(command_id, capture, started)
                .with_sandbox_execution(ELEVATED_BACKEND_NAME, SandboxBackendEnforcement::Strict),
            after,
        )),
        Err(elevated_error)
            if !observe_final_snapshot
                && prepared.restricted_token_fallback
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
            Ok((
                command_result_from_capture(command_id, capture, started).with_sandbox_execution(
                    RESTRICTED_TOKEN_BACKEND_NAME,
                    SandboxBackendEnforcement::RestrictedToken,
                ),
                None,
            ))
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

fn elapsed_ms(started: Instant) -> u64 {
    started.elapsed().as_millis().try_into().unwrap_or(u64::MAX)
}

fn merge_observation_phase_metrics(
    first: WorkspaceObservationPhaseMetrics,
    second: WorkspaceObservationPhaseMetrics,
) -> WorkspaceObservationPhaseMetrics {
    let mode = match (first.mode, second.mode) {
        (WorkspaceObservationMode::Full, _) | (_, WorkspaceObservationMode::Full) => {
            WorkspaceObservationMode::Full
        }
        (WorkspaceObservationMode::Incremental, _) | (_, WorkspaceObservationMode::Incremental) => {
            WorkspaceObservationMode::Incremental
        }
        (WorkspaceObservationMode::Reused, WorkspaceObservationMode::Reused) => {
            WorkspaceObservationMode::Reused
        }
    };
    WorkspaceObservationPhaseMetrics {
        mode,
        duration_ms: first.duration_ms.saturating_add(second.duration_ms),
        entries_read: first.entries_read.saturating_add(second.entries_read),
        content_bytes_read: first
            .content_bytes_read
            .saturating_add(second.content_bytes_read),
    }
}

fn resolve_before_workspace_snapshot(
    workspace: &Path,
    seed: Option<BeforeSnapshotSeed>,
    protected_paths: &[PathBuf],
) -> Result<ObservedWorkspaceSnapshot, String> {
    let started = Instant::now();
    if let Some(seed) = seed {
        match update_workspace_snapshot_as_sandbox_user(
            workspace,
            &seed.snapshot,
            &seed.observation,
            protected_paths,
        )? {
            IncrementalSnapshot::Updated(snapshot, work) => {
                let mode = if matches!(seed.observation, WorkspaceChangeObservation::Unchanged) {
                    WorkspaceObservationMode::Reused
                } else {
                    WorkspaceObservationMode::Incremental
                };
                return Ok(ObservedWorkspaceSnapshot {
                    snapshot,
                    metrics: WorkspaceObservationPhaseMetrics {
                        mode,
                        duration_ms: elapsed_ms(started),
                        entries_read: work.entries_read,
                        content_bytes_read: work.content_bytes_read,
                    },
                });
            }
            IncrementalSnapshot::FullRequired => {}
        }
        let snapshot = snapshot_workspace_as_sandbox_user_for_cached_root(
            workspace,
            &seed.snapshot,
            protected_paths,
        )?;
        let work = snapshot.full_scan_work();
        return Ok(ObservedWorkspaceSnapshot {
            snapshot,
            metrics: WorkspaceObservationPhaseMetrics {
                mode: WorkspaceObservationMode::Full,
                duration_ms: elapsed_ms(started),
                entries_read: work.entries_read,
                content_bytes_read: work.content_bytes_read,
            },
        });
    }
    let snapshot = snapshot_workspace_as_sandbox_user(workspace, protected_paths)?;
    let work = snapshot.full_scan_work();
    Ok(ObservedWorkspaceSnapshot {
        snapshot,
        metrics: WorkspaceObservationPhaseMetrics {
            mode: WorkspaceObservationMode::Full,
            duration_ms: elapsed_ms(started),
            entries_read: work.entries_read,
            content_bytes_read: work.content_bytes_read,
        },
    })
}

fn resolve_after_workspace_snapshot(
    workspace: &Path,
    before: &WorkspaceSnapshot,
    observation: &WorkspaceChangeObservation,
    protected_paths: &[PathBuf],
) -> Result<ObservedWorkspaceSnapshot, String> {
    let started = Instant::now();
    match update_workspace_snapshot_as_sandbox_user(
        workspace,
        before,
        observation,
        protected_paths,
    )? {
        IncrementalSnapshot::Updated(snapshot, work) => {
            let mode = if matches!(observation, WorkspaceChangeObservation::Unchanged) {
                WorkspaceObservationMode::Reused
            } else {
                WorkspaceObservationMode::Incremental
            };
            Ok(ObservedWorkspaceSnapshot {
                snapshot,
                metrics: WorkspaceObservationPhaseMetrics {
                    mode,
                    duration_ms: elapsed_ms(started),
                    entries_read: work.entries_read,
                    content_bytes_read: work.content_bytes_read,
                },
            })
        }
        IncrementalSnapshot::FullRequired => {
            let snapshot = snapshot_workspace_as_sandbox_user_for_cached_root(
                workspace,
                before,
                protected_paths,
            )?;
            let work = snapshot.full_scan_work();
            Ok(ObservedWorkspaceSnapshot {
                snapshot,
                metrics: WorkspaceObservationPhaseMetrics {
                    mode: WorkspaceObservationMode::Full,
                    duration_ms: elapsed_ms(started),
                    entries_read: work.entries_read,
                    content_bytes_read: work.content_bytes_read,
                },
            })
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

/// Resolve the controller's canonical root, retaining its key after same-backend quarantine.
fn release_observation_root(path: &Path) -> Result<PathBuf, String> {
    if !path.is_absolute() {
        return Err("workspace observation root must be absolute".to_string());
    }
    if path
        .components()
        .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
    {
        return Err(
            "workspace observation root must not contain relative path components".to_string(),
        );
    }

    match dunce::canonicalize(path) {
        Ok(canonical) if canonical.is_dir() => Ok(canonical),
        Ok(_) => Err(format!("path is not a directory: {}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let parent = path
                .parent()
                .ok_or_else(|| "workspace observation root has no parent".to_string())?;
            let name = path
                .file_name()
                .ok_or_else(|| "workspace observation root has no final component".to_string())?;
            let canonical_parent = dunce::canonicalize(parent).map_err(|parent_error| {
                format!(
                    "workspace observation root and parent are unavailable: {}; {}",
                    error, parent_error
                )
            })?;
            if !canonical_parent.is_dir() {
                return Err(format!(
                    "workspace observation root parent is not a directory: {}",
                    parent.display()
                ));
            }
            Ok(canonical_parent.join(name))
        }
        Err(error) => Err(format!("failed to resolve {}: {error}", path.display())),
    }
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

fn child_environment(
    policy: &CommandEnvironmentPolicy,
    workspace: &Path,
) -> Result<HashMap<String, String>, String> {
    child_environment_from(std::env::vars(), policy, workspace)
}

fn child_environment_from(
    environment: impl IntoIterator<Item = (String, String)>,
    policy: &CommandEnvironmentPolicy,
    workspace: &Path,
) -> Result<HashMap<String, String>, String> {
    let mut env_map = filtered_child_environment(environment, policy);
    let temp = env_value(&env_map, "TEMP")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            let temp = std::env::temp_dir();
            temp.is_absolute().then_some(temp)
        });
    if let Some(temp) = temp.as_ref() {
        let cache = temp.join("singularity-tool-cache");
        set_environment_value(
            &mut env_map,
            "PIP_CACHE_DIR",
            &cache.join("pip").to_string_lossy(),
        );
        set_environment_value(
            &mut env_map,
            "NPM_CONFIG_CACHE",
            &cache.join("npm").to_string_lossy(),
        );
        set_environment_value(
            &mut env_map,
            "PYTHONPYCACHEPREFIX",
            &cache.join("python").to_string_lossy(),
        );
        let pytest_cache = cache.join("pytest").to_string_lossy().replace('\\', "/");
        let pytest_addopts = env_value(&env_map, "PYTEST_ADDOPTS")
            .map(|value| format!("{value} "))
            .unwrap_or_default();
        set_environment_value(
            &mut env_map,
            "PYTEST_ADDOPTS",
            &format!("{pytest_addopts}-o \"cache_dir={pytest_cache}\""),
        );
    }
    if policy == &CommandEnvironmentPolicy::Isolated {
        let temp = temp.ok_or_else(|| {
            "isolated command environment has no absolute TEMP cache root".to_string()
        })?;
        let temp = dunce::canonicalize(&temp)
            .map_err(|error| format!("isolated TEMP cache root is unavailable: {error}"))?;
        if !temp.is_absolute() {
            return Err("isolated TEMP cache root is not absolute".to_string());
        }
        let workspace = dunce::canonicalize(workspace)
            .map_err(|error| format!("isolated workspace root is unavailable: {error}"))?;
        let target = temp
            .join("singularity-tool-cache")
            .join("cargo")
            .join(super::workspace_tool_cache_digest(&workspace));
        if target.starts_with(&workspace) {
            return Err(
                "isolated Cargo target directory would be inside the workspace".to_string(),
            );
        }
        set_environment_value(&mut env_map, "CARGO_TARGET_DIR", &target.to_string_lossy());
    }
    Ok(env_map)
}

fn set_environment_value(env_map: &mut HashMap<String, String>, name: &str, value: &str) {
    let matching_keys = env_map
        .keys()
        .filter(|key| key.eq_ignore_ascii_case(name))
        .cloned()
        .collect::<Vec<_>>();
    for key in matching_keys {
        env_map.remove(&key);
    }
    env_map.insert(name.to_string(), value.to_string());
}

fn filtered_child_environment(
    environment: impl IntoIterator<Item = (String, String)>,
    policy: &CommandEnvironmentPolicy,
) -> HashMap<String, String> {
    let mut filtered = HashMap::new();
    for (name, value) in environment {
        if is_secret_env_name(&name)
            || (policy == &CommandEnvironmentPolicy::Isolated
                && is_isolated_host_environment(&name))
        {
            continue;
        }
        set_environment_value(&mut filtered, &name, &value);
    }
    filtered
}

fn is_isolated_host_environment(name: &str) -> bool {
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
    use crate::SandboxPreflightOutcome;
    use singularity_windows_sandbox::WorkspacePathChange;
    use std::fs;
    use std::io::Write;

    fn create_test_file(path: &Path, contents: &str) {
        let mut file = fs::File::create(path).expect("create test file");
        file.write_all(contents.as_bytes())
            .expect("write test file");
    }

    #[test]
    fn native_preflight_reports_verified_controls_or_typed_blocker() {
        let workspace = tempfile::tempdir().expect("workspace");
        let report =
            WindowsSandboxBackend::new().preflight(workspace.path(), &CancellationToken::new());

        if report.outcome == SandboxPreflightOutcome::Supported {
            assert_eq!(report.error_code, None);
            assert_eq!(report.transactional_workspace, SandboxPreflightFact::Passed);
            assert_eq!(report.network_denied, SandboxPreflightFact::Passed);
            assert_eq!(report.protected_paths, SandboxPreflightFact::Passed);
            assert!(report.proves_supported_contract_for("windows"));
        } else {
            let code = report.error_code.as_deref().expect("typed blocker");
            assert!(!code.is_empty());
            assert!(!report.missing_capabilities.is_empty());
            assert!(!report.proves_supported_contract_for("windows"));
        }
        assert!(!workspace.path().join("singularity-preflight.txt").exists());
    }

    #[test]
    fn ntfs_filesystem_gate_rejects_unknown_and_refs_without_starting_a_child() {
        assert_eq!(windows_filesystem_gate_error(Some("NTFS")), None);
        assert_eq!(
            windows_filesystem_gate_error(Some("ReFS")),
            Some("sandbox_unsupported_filesystem")
        );
        assert_eq!(
            windows_filesystem_gate_error(None),
            Some("sandbox_filesystem_unknown")
        );
    }

    #[test]
    fn readonly_preparation_retains_a_workspace_root_lease() {
        let temp = tempfile::tempdir().expect("workspace parent");
        let parent = temp.path().join("parent");
        let workspace = parent.join("workspace");
        fs::create_dir_all(&workspace).expect("workspace");
        let comspec = std::env::var_os("ComSpec").expect("ComSpec");
        let mut request = CommandRequest::project_verification(
            "readonly-root-lease",
            vec![PathBuf::from(comspec).to_string_lossy().into_owned()],
            workspace.to_string_lossy(),
            workspace.to_string_lossy(),
        );
        request.filesystem.mode = SandboxFilesystemMode::ReadOnly;

        let prepared = PreparedCommand::from_request(&request, None, None)
            .expect("read-only command preparation");
        let lease = prepared
            .workspace_root_lease
            .as_ref()
            .expect("read-only preparation must retain the root lease");
        let displaced = temp.path().join("parent-displaced");
        let replaced = fs::rename(&parent, &displaced).is_ok();
        if replaced {
            fs::create_dir_all(&workspace).expect("replacement workspace");
            assert!(
                lease.verify().is_err(),
                "read-only preparation must reject a replaced parent chain"
            );
        } else {
            lease.verify().expect("unchanged read-only root lease");
        }
    }

    #[test]
    fn trusted_readonly_preparation_retains_a_workspace_root_lease() {
        let temp = tempfile::tempdir().expect("workspace parent");
        let parent = temp.path().join("parent");
        let workspace = parent.join("workspace");
        fs::create_dir_all(&workspace).expect("workspace");
        let comspec = std::env::var_os("ComSpec").expect("ComSpec");
        let mut request = CommandRequest::trusted_workspace_preparation(
            "trusted-readonly-root-lease",
            vec![PathBuf::from(comspec).to_string_lossy().into_owned()],
            workspace.to_string_lossy(),
            workspace.to_string_lossy(),
        );
        request.filesystem.mode = SandboxFilesystemMode::ReadOnly;

        let prepared = PreparedCommand::from_request(&request, None, None)
            .expect("trusted read-only command preparation");
        let lease = prepared
            .workspace_root_lease
            .as_ref()
            .expect("trusted read-only preparation must retain the root lease");
        let displaced = temp.path().join("parent-displaced");
        let replaced = fs::rename(&parent, &displaced).is_ok();
        if replaced {
            fs::create_dir_all(&workspace).expect("replacement workspace");
            assert!(
                lease.verify().is_err(),
                "trusted read-only preparation must reject a replaced parent chain"
            );
        } else {
            lease
                .verify()
                .expect("unchanged trusted read-only root lease");
        }
    }

    #[test]
    fn workspace_monitor_failure_cannot_be_reconciled_as_unchanged() {
        let (mutation, summary) =
            reconcile_workspace_change(Some(Err(())), Some(Ok((false, None))));

        assert_eq!(mutation, WorkspaceMutation::Unknown);
        assert!(summary.is_none());
    }

    #[test]
    fn protected_setup_security_notification_requires_unchanged_snapshot() {
        let workspace = tempfile::tempdir().expect("workspace");
        let value = workspace.path().join("value.txt");
        create_test_file(&value, "stable");
        let before = snapshot_workspace_as_sandbox_user(workspace.path(), &[]).expect("snapshot");
        let after = resolve_after_workspace_snapshot(
            workspace.path(),
            &before,
            &WorkspaceChangeObservation::Changed(vec![WorkspacePathChange {
                path: "value.txt".to_string(),
                kind: singularity_windows_sandbox::WorkspacePathChangeKind::Modified,
            }]),
            &[],
        )
        .expect("security notification refresh");

        reconcile_protected_setup_change(&before, &after.snapshot)
            .expect("ACL-only notification with unchanged snapshot must be accepted");

        std::fs::write(&value, b"changed").expect("external setup write");
        let changed = resolve_after_workspace_snapshot(
            workspace.path(),
            &before,
            &WorkspaceChangeObservation::Changed(vec![WorkspacePathChange {
                path: "value.txt".to_string(),
                kind: singularity_windows_sandbox::WorkspacePathChangeKind::Modified,
            }]),
            &[],
        )
        .expect("changed setup refresh");
        assert!(reconcile_protected_setup_change(&before, &changed.snapshot).is_err());
    }

    #[test]
    fn protected_setup_structure_notification_is_rejected() {
        let workspace = tempfile::tempdir().expect("workspace");
        let before = snapshot_workspace_as_sandbox_user(workspace.path(), &[]).expect("snapshot");
        std::fs::create_dir(workspace.path().join("created")).expect("external setup directory");
        let changed = resolve_after_workspace_snapshot(
            workspace.path(),
            &before,
            &WorkspaceChangeObservation::Changed(vec![WorkspacePathChange {
                path: "created".to_string(),
                kind: singularity_windows_sandbox::WorkspacePathChangeKind::Added,
            }]),
            &[],
        )
        .expect("structure setup refresh");

        assert!(reconcile_protected_setup_change(&before, &changed.snapshot).is_err());
    }

    #[test]
    fn second_command_before_snapshot_reuses_session_without_a_full_scan() {
        let workspace = tempfile::tempdir().expect("workspace");
        create_test_file(&workspace.path().join("value.txt"), "value");
        let root = dunce::canonicalize(workspace.path()).expect("canonical workspace");
        let mut session =
            WorkspaceObservationSession::start(root.clone()).expect("observation session");

        let first = resolve_before_workspace_snapshot(
            &root,
            session.before_seed().expect("first seed"),
            &[],
        )
        .expect("first before snapshot");
        assert_eq!(first.metrics.mode, WorkspaceObservationMode::Full);
        assert_eq!(first.metrics.entries_read, 2);
        assert_eq!(first.metrics.content_bytes_read, 5);
        session.publish(first.snapshot, None);

        let second_seed = session.before_seed().expect("second seed");
        let second = resolve_before_workspace_snapshot(&root, second_seed, &[])
            .expect("second before snapshot");
        assert_eq!(second.metrics.mode, WorkspaceObservationMode::Reused);
        assert_eq!(second.metrics.entries_read, 1);
        assert_eq!(second.metrics.content_bytes_read, 0);
        session.publish(second.snapshot.clone(), None);
        assert_eq!(
            session.cache.as_ref().map(|cache| &cache.snapshot),
            Some(&second.snapshot)
        );
    }

    #[test]
    fn protected_path_cache_skips_full_resolver_on_second_unchanged_command() {
        let workspace = tempfile::tempdir().expect("workspace");
        create_test_file(&workspace.path().join(".env"), "opaque");
        let root = dunce::canonicalize(workspace.path()).expect("canonical workspace");
        let comspec = std::env::var_os("ComSpec").expect("ComSpec");
        let request = CommandRequest::project_verification(
            "cached-protected-paths",
            vec![
                PathBuf::from(comspec).to_string_lossy().into_owned(),
                "/c".to_string(),
                "echo ready".to_string(),
            ],
            root.to_string_lossy(),
            root.to_string_lossy(),
        );
        let mut session = WorkspaceObservationSession::start(root.clone()).expect("session");
        let first_preparation = session.prepare_for_command().expect("first preparation");
        let (first_seed, first_monitor, first_cached_protected_paths, first_workspace_root_lease) =
            first_preparation.into_execution_parts();
        assert!(first_seed.is_none());
        assert!(first_monitor.is_some());
        assert!(first_cached_protected_paths.is_none());
        FULL_PROTECTED_RESOLVER_SCANS.with(|count| count.set(0));
        let first = PreparedCommand::from_request(
            &request,
            first_cached_protected_paths.as_deref(),
            first_workspace_root_lease,
        )
        .expect("first command");
        assert_eq!(FULL_PROTECTED_RESOLVER_SCANS.with(Cell::get), 1);
        let first_snapshot = snapshot_workspace_as_sandbox_user(&root, &[]).expect("snapshot");
        session.publish_with_protected_paths(
            first_snapshot,
            first.protected_deny_read_paths.clone(),
            first_monitor,
        );

        let second_preparation = session.prepare_for_command().expect("second preparation");
        let (
            second_seed,
            second_monitor,
            second_cached_protected_paths,
            second_workspace_root_lease,
        ) = second_preparation.into_execution_parts();
        assert!(second_seed.is_some());
        assert!(second_monitor.is_some());
        assert_eq!(
            second_cached_protected_paths.as_ref().map(Vec::len),
            Some(first.protected_deny_read_paths.len())
        );
        let _second = PreparedCommand::from_request(
            &request,
            second_cached_protected_paths.as_deref(),
            second_workspace_root_lease,
        )
        .expect("cached command");
        assert_eq!(FULL_PROTECTED_RESOLVER_SCANS.with(Cell::get), 1);
    }

    #[test]
    fn protected_path_cache_is_cleared_when_the_checkpoint_reports_changed() {
        let workspace = tempfile::tempdir().expect("workspace");
        let protected = workspace.path().join(".env");
        create_test_file(&protected, "opaque");
        let root = dunce::canonicalize(workspace.path()).expect("canonical workspace");
        let mut session = WorkspaceObservationSession::start(root.clone()).expect("session");
        let first = session.prepare_for_command().expect("first preparation");
        let snapshot = snapshot_workspace_as_sandbox_user(&root, &[]).expect("snapshot");
        let protected_path =
            AbsolutePathBuf::from_absolute_path_checked(&protected).expect("protected path");
        session.publish_with_protected_paths(snapshot, vec![protected_path], Some(first.monitor));
        std::fs::write(&protected, "changed").expect("change protected path");

        let second = session.prepare_for_command().expect("changed preparation");
        assert!(second.cached_protected_paths.is_none());
        assert!(session.cache.is_none());
    }

    #[test]
    fn protected_path_identity_rejects_same_content_replacement() {
        let workspace = tempfile::tempdir().expect("workspace");
        let root = dunce::canonicalize(workspace.path()).expect("canonical workspace");
        let protected = root.join(".env");
        create_test_file(&protected, "opaque");
        let path = AbsolutePathBuf::from_absolute_path_checked(&protected).expect("protected path");
        let cached = capture_cached_protected_paths(&root, std::slice::from_ref(&path))
            .expect("capture identity");
        let displaced = root.join(".env.old");
        std::fs::rename(&protected, displaced).expect("displace protected path");
        create_test_file(&protected, "opaque");

        assert!(validate_cached_protected_paths(&root, &cached).is_err());
    }

    #[test]
    fn between_command_out_of_band_write_invalidates_the_cached_baseline() {
        let workspace = tempfile::tempdir().expect("workspace");
        let value = workspace.path().join("value.txt");
        create_test_file(&value, "before");
        let root = dunce::canonicalize(workspace.path()).expect("canonical workspace");
        let mut session =
            WorkspaceObservationSession::start(root.clone()).expect("observation session");
        let before =
            resolve_before_workspace_snapshot(&root, None, &[]).expect("first before snapshot");
        session.publish(before.snapshot, None);

        create_test_file(&value, "after");
        let seed = session.before_seed().expect("checkpoint");
        assert!(seed.is_none(), "out-of-band writes must invalidate reuse");
        let after = resolve_before_workspace_snapshot(&root, seed, &[]).expect("full refresh");

        assert_eq!(after.metrics.mode, WorkspaceObservationMode::Full);
        assert_eq!(after.metrics.entries_read, 2);
        assert_eq!(after.metrics.content_bytes_read, 5);
        assert_ne!(
            session.cache.as_ref().map(|cache| &cache.snapshot),
            Some(&after.snapshot)
        );
    }

    #[test]
    fn large_out_of_band_added_subtree_invalidates_the_cached_baseline() {
        const FILES: usize = 5_000;
        let workspace = tempfile::tempdir().expect("workspace");
        create_test_file(&workspace.path().join("stable.txt"), "stable");
        let root = dunce::canonicalize(workspace.path()).expect("canonical workspace");
        let mut session =
            WorkspaceObservationSession::start(root.clone()).expect("observation session");
        let before =
            resolve_before_workspace_snapshot(&root, None, &[]).expect("first before snapshot");
        session.publish(before.snapshot, None);

        let added = workspace.path().join(".environment");
        std::fs::create_dir(&added).expect("added directory");
        for index in 0..FILES {
            create_test_file(&added.join(format!("file-{index:04}.txt")), "x");
        }
        let seed = session.before_seed().expect("checkpoint");
        assert!(seed.is_none(), "out-of-band writes must invalidate reuse");
        let after = resolve_before_workspace_snapshot(&root, seed, &[]).expect("full refresh");

        assert_eq!(after.metrics.mode, WorkspaceObservationMode::Full);
        assert_eq!(after.metrics.entries_read, FILES + 3);
        assert_eq!(after.metrics.content_bytes_read, FILES as u64 + 6);
        assert_eq!(
            after.snapshot,
            snapshot_workspace_as_sandbox_user(&root, &[]).expect("full comparison")
        );
    }

    #[test]
    fn observation_contract_change_starts_a_distinct_session() {
        let workspace = tempfile::tempdir().expect("workspace");
        let root = workspace.path().to_string_lossy();
        let first_key = observation_session_key(
            &root,
            &SandboxFilesystemMode::WorkspaceWrite,
            &SandboxNetworkMode::Denied,
            &CommandEnvironmentPolicy::Isolated,
        )
        .expect("first key");
        let second_key = observation_session_key(
            &root,
            &SandboxFilesystemMode::WorkspaceWrite,
            &SandboxNetworkMode::Allowed,
            &CommandEnvironmentPolicy::Isolated,
        )
        .expect("second key");
        assert_ne!(first_key, second_key);

        let backend = WindowsSandboxBackend::new();
        let first = backend
            .observation_session(Some(first_key))
            .expect("first session");
        let second = backend
            .observation_session(Some(second_key))
            .expect("second session");
        assert!(!Arc::ptr_eq(&first, &second));
        assert!(first.lock().expect("first lock").cache.is_none());
        assert!(second.lock().expect("second lock").cache.is_none());
    }

    #[test]
    fn release_workspace_observation_drops_exact_root_sessions_before_parent_rename() {
        let temp = tempfile::tempdir().expect("workspace");
        let parent = temp.path().join("parent");
        let first_root = parent.join("first");
        let second_root = parent.join("second");
        fs::create_dir_all(&first_root).expect("first workspace");
        fs::create_dir_all(&second_root).expect("second workspace");
        let first_root = dunce::canonicalize(first_root).expect("first canonical workspace");
        let second_root = dunce::canonicalize(second_root).expect("second canonical workspace");
        let key_for = |root: &Path| {
            let root = root.to_string_lossy();
            observation_session_key(
                &root,
                &SandboxFilesystemMode::WorkspaceWrite,
                &SandboxNetworkMode::Denied,
                &CommandEnvironmentPolicy::Isolated,
            )
            .expect("observation session key")
        };
        let backend = WindowsSandboxBackend::new();
        let first = backend
            .observation_session(Some(key_for(&first_root)))
            .expect("first observation session");
        let first_snapshot = resolve_before_workspace_snapshot(&first_root, None, &[])
            .expect("first snapshot")
            .snapshot;
        first
            .lock()
            .expect("first session lock")
            .publish_with_protected_paths(first_snapshot, Vec::new(), None);
        let first_other_key = {
            let root = first_root.to_string_lossy();
            observation_session_key(
                &root,
                &SandboxFilesystemMode::WorkspaceWrite,
                &SandboxNetworkMode::Allowed,
                &CommandEnvironmentPolicy::Isolated,
            )
            .expect("second first-root observation session key")
        };
        let first_other = backend
            .observation_session(Some(first_other_key))
            .expect("second first-root observation session");
        drop(first_other);
        let active_error = backend
            .release_workspace_observation(&first_root)
            .expect_err("active observation owner must block release");
        assert!(active_error.contains("active owners"));
        drop(first);
        let second = backend
            .observation_session(Some(key_for(&second_root)))
            .expect("second observation session");
        drop(second);

        let first_displaced = parent.join("first-displaced");
        fs::rename(&first_root, &first_displaced).expect("quarantine first workspace");
        assert!(!first_root.exists());
        assert!(first_displaced.is_dir());

        backend
            .release_workspace_observation(&first_root)
            .expect("release first observation root");
        {
            let sessions = backend
                .observation_sessions
                .lock()
                .expect("observation session cache");
            assert!(!sessions.sessions.keys().any(|key| key.root == first_root));
            assert!(sessions.sessions.keys().any(|key| key.root == second_root));
        }
        backend
            .release_workspace_observation(&second_root)
            .expect("release second observation root");

        let displaced = temp.path().join("parent-displaced");
        fs::rename(parent, displaced).expect("parent quarantine rename after release");
    }

    #[test]
    fn cancelled_command_invalidation_forces_the_next_full_before_snapshot() {
        let workspace = tempfile::tempdir().expect("workspace");
        create_test_file(&workspace.path().join("value.txt"), "value");
        let root = dunce::canonicalize(workspace.path()).expect("canonical workspace");
        let mut session =
            WorkspaceObservationSession::start(root.clone()).expect("observation session");
        let before =
            resolve_before_workspace_snapshot(&root, None, &[]).expect("first before snapshot");
        session.publish(before.snapshot, None);

        session.invalidate();
        let after_cancel = resolve_before_workspace_snapshot(
            &root,
            session.before_seed().expect("invalidated seed"),
            &[],
        )
        .expect("full snapshot after cancellation");

        assert_eq!(after_cancel.metrics.mode, WorkspaceObservationMode::Full);
        assert!(after_cancel.snapshot.full_scan_work().entries_read > 1);
    }

    #[test]
    fn unknown_session_observation_forces_a_full_before_snapshot() {
        let workspace = tempfile::tempdir().expect("workspace");
        create_test_file(&workspace.path().join("value.txt"), "value");
        let root = dunce::canonicalize(workspace.path()).expect("canonical workspace");
        let snapshot = snapshot_workspace_as_sandbox_user(&root, &[]).expect("published snapshot");

        let observed = resolve_before_workspace_snapshot(
            &root,
            Some(BeforeSnapshotSeed {
                snapshot,
                observation: WorkspaceChangeObservation::Unknown,
            }),
            &[],
        )
        .expect("full snapshot");

        assert_eq!(observed.metrics.mode, WorkspaceObservationMode::Full);
    }

    #[test]
    fn cached_session_rejects_same_content_root_replacement() {
        let parent = tempfile::tempdir().expect("parent");
        let root = parent.path().join("workspace");
        let displaced = parent.path().join("displaced");
        fs::create_dir(&root).expect("workspace");
        create_test_file(&root.join("value.txt"), "value");
        let snapshot = snapshot_workspace_as_sandbox_user(&root, &[]).expect("snapshot");
        fs::rename(&root, &displaced).expect("displace root");
        fs::create_dir(&root).expect("replacement root");
        create_test_file(&root.join("value.txt"), "value");

        let error = resolve_before_workspace_snapshot(
            &root,
            Some(BeforeSnapshotSeed {
                snapshot,
                observation: WorkspaceChangeObservation::Changed(vec![
                    singularity_windows_sandbox::WorkspacePathChange {
                        path: "value.txt".to_string(),
                        kind: singularity_windows_sandbox::WorkspacePathChangeKind::Modified,
                    },
                ]),
            }),
            &[],
        )
        .expect_err("replacement root must not become a new baseline");

        assert!(error.contains("root identity or behavior drifted"));
    }

    #[test]
    fn metadata_only_notification_without_snapshot_delta_fails_closed() {
        let observation = WorkspaceChangeObservation::Changed(vec![
            singularity_windows_sandbox::WorkspacePathChange {
                path: "value.txt".to_string(),
                kind: singularity_windows_sandbox::WorkspacePathChangeKind::Modified,
            },
        ]);
        let (mutation, summary) =
            reconcile_workspace_change(Some(Ok(observation)), Some(Ok((false, None))));

        assert_eq!(mutation, WorkspaceMutation::Unknown);
        assert!(summary.is_none());
    }

    #[test]
    fn complete_snapshot_proves_large_change_when_monitor_overflows() {
        let (mutation, summary) = reconcile_workspace_change(
            Some(Ok(WorkspaceChangeObservation::Unknown)),
            Some(Ok((true, None))),
        );

        assert_eq!(mutation, WorkspaceMutation::Changed);
        assert!(summary.is_none());
    }

    #[test]
    fn trusted_workspace_preparation_uses_snapshot_without_protected_monitor_noise() {
        let workspace = tempfile::tempdir().expect("workspace");
        let trusted = CommandRequest::trusted_workspace_preparation(
            "trusted",
            vec!["git".to_string(), "init".to_string()],
            workspace.path().to_string_lossy().into_owned(),
            workspace.path().to_string_lossy().into_owned(),
        );
        let ordinary = CommandRequest::project_verification(
            "ordinary",
            vec!["git".to_string(), "init".to_string()],
            workspace.path().to_string_lossy().into_owned(),
            workspace.path().to_string_lossy().into_owned(),
        );

        assert!(!should_monitor_workspace_change(&trusted));
        assert!(should_monitor_workspace_change(&ordinary));
    }

    #[test]
    fn protected_globs_treat_workspace_root_metacharacters_as_literals() {
        let parent = tempfile::tempdir().expect("parent");
        let workspace = parent.path().join("repo[bar]");
        std::fs::create_dir(&workspace).expect("workspace");
        let protected = workspace.join(".env");
        std::fs::write(&protected, "secret").expect("protected file");
        let workspace =
            AbsolutePathBuf::from_absolute_path_checked(&workspace).expect("absolute workspace");

        let (resolved, incomplete) =
            resolve_existing_protected_paths(&workspace, None).expect("protected paths");

        assert!(!incomplete);
        assert!(
            resolved
                .iter()
                .any(|path| path.as_path() == protected.as_path())
        );
    }

    #[test]
    fn isolated_environment_removes_host_build_overrides_but_keeps_tool_discovery() {
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

        let isolated =
            filtered_child_environment(environment.clone(), &CommandEnvironmentPolicy::Isolated);
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
    fn command_environment_keeps_pytest_options_and_externalizes_tool_caches() {
        let temp_root = tempfile::tempdir().expect("temp root");
        let workspace = tempfile::tempdir().expect("workspace");
        let temp_path = temp_root.path().to_string_lossy().into_owned();
        let canonical_temp = dunce::canonicalize(temp_root.path()).expect("canonical temp root");
        let cache_root = temp_root.path().join("singularity-tool-cache");
        let pip_cache = cache_root.join("pip").to_string_lossy().into_owned();
        let npm_cache = cache_root.join("npm").to_string_lossy().into_owned();
        let python_cache = cache_root.join("python").to_string_lossy().into_owned();
        let pytest_cache = cache_root
            .join("pytest")
            .to_string_lossy()
            .replace('\\', "/");
        let expected_pytest = format!("--maxfail=1 -o \"cache_dir={pytest_cache}\"");
        let environment = [
            ("Path".to_string(), "C:\\tools".to_string()),
            ("TEMP".to_string(), temp_path.clone()),
            ("temp".to_string(), temp_path),
            ("PYTEST_ADDOPTS".to_string(), "--maxfail=1".to_string()),
            (
                "CARGO_TARGET_DIR".to_string(),
                "D:\\host-target".to_string(),
            ),
            ("SINGULARITY_MODEL".to_string(), "host-model".to_string()),
        ];

        for policy in [
            CommandEnvironmentPolicy::HostSanitized,
            CommandEnvironmentPolicy::Isolated,
        ] {
            let values = child_environment_from(environment.clone(), &policy, workspace.path())
                .expect("child environment");
            assert_eq!(env_value(&values, "PATH"), Some("C:\\tools"));
            assert_eq!(
                env_value(&values, "PIP_CACHE_DIR"),
                Some(pip_cache.as_str())
            );
            assert_eq!(
                env_value(&values, "NPM_CONFIG_CACHE"),
                Some(npm_cache.as_str())
            );
            assert_eq!(
                env_value(&values, "PYTHONPYCACHEPREFIX"),
                Some(python_cache.as_str())
            );
            assert_eq!(
                env_value(&values, "PYTEST_ADDOPTS"),
                Some(expected_pytest.as_str())
            );
            assert_eq!(
                values
                    .keys()
                    .filter(|key| key.eq_ignore_ascii_case("TEMP"))
                    .count(),
                1
            );
            if policy == CommandEnvironmentPolicy::Isolated {
                let target = PathBuf::from(
                    env_value(&values, "CARGO_TARGET_DIR").expect("isolated Cargo target"),
                );
                assert!(target.is_absolute());
                assert!(target.starts_with(&canonical_temp));
                assert!(!target.starts_with(workspace.path()));
                assert_eq!(
                    target,
                    canonical_temp
                        .join("singularity-tool-cache")
                        .join("cargo")
                        .join(super::super::workspace_tool_cache_digest(
                            &dunce::canonicalize(workspace.path()).expect("canonical workspace")
                        ))
                );
                assert!(env_value(&values, "SINGULARITY_MODEL").is_none());
            } else {
                assert_eq!(
                    env_value(&values, "CARGO_TARGET_DIR"),
                    Some("D:\\host-target")
                );
                assert_eq!(env_value(&values, "SINGULARITY_MODEL"), Some("host-model"));
            }
        }
    }

    #[test]
    fn isolated_command_environment_fails_closed_for_unusable_cache_roots() {
        let workspace = tempfile::tempdir().expect("workspace");
        let inside_workspace = workspace.path().to_string_lossy().into_owned();
        let error = child_environment_from(
            [("TEMP".to_string(), inside_workspace)],
            &CommandEnvironmentPolicy::Isolated,
            workspace.path(),
        )
        .expect_err("workspace TEMP must be rejected");
        assert_eq!(
            error,
            "isolated Cargo target directory would be inside the workspace"
        );

        let external = tempfile::tempdir().expect("external root");
        let missing = external.path().join("missing");
        let error = child_environment_from(
            [("TEMP".to_string(), missing.to_string_lossy().into_owned())],
            &CommandEnvironmentPolicy::Isolated,
            workspace.path(),
        )
        .expect_err("missing TEMP must be rejected");
        assert!(
            error.starts_with("isolated TEMP cache root is unavailable:"),
            "{error}"
        );
    }

    #[test]
    fn executable_probe_uses_windows_command_resolution() {
        let workspace = tempfile::tempdir().expect("workspace");
        let executable = std::env::current_exe().expect("current executable");
        let missing = workspace.path().join("missing-executable.exe");
        let backend = WindowsSandboxBackend::new();

        assert_eq!(
            backend.probe_executable(
                workspace.path(),
                &executable.to_string_lossy(),
                &CommandEnvironmentPolicy::Isolated,
            ),
            ExecutableAvailability::Available
        );
        assert_eq!(
            backend.probe_executable(
                workspace.path(),
                &missing.to_string_lossy(),
                &CommandEnvironmentPolicy::Isolated,
            ),
            ExecutableAvailability::Unavailable
        );
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

        let prepared = PreparedCommand::from_request(&request, None, None)
            .expect("workspace-write command preparation");
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
        assert_eq!(
            elevated.trusted_deny_write_paths_override,
            prepared.trusted_deny_write_paths.as_slice()
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

        let prepared = PreparedCommand::from_request(&request, None, None)
            .expect("trusted workspace preparation");

        assert!(request.is_trusted_workspace_preparation());
        assert!(prepared.protected_deny_read_paths.is_empty());
        assert!(prepared.protected_deny_write_paths.is_empty());
        assert!(!prepared.trusted_deny_write_paths.is_empty());
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

        let prepared = PreparedCommand::from_script_request(&request, None, None)
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

        let prepared = match PreparedCommand::from_script_request(&request, None, None) {
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
        assert_eq!(
            elevated.trusted_deny_write_paths_override,
            prepared.trusted_deny_write_paths.as_slice()
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
