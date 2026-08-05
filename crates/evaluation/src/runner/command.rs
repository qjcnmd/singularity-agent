//! Evaluation command 的可信执行、scope digest 和 sandbox 诊断辅助逻辑。

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::{Argv, BlockerKind, CommandSpec, EvaluationBlocker};
use serde::Serialize;
use singularity_policy::NetworkAccess;
use singularity_tools::{
    CommandEnvironmentPolicy, CommandExecutionStatus, CommandRequest, CommandResult,
    CommandSemanticStatus, SandboxBackendEnforcement, SandboxFilesystemMode, SandboxNetworkMode,
    ToolFailureKind, ToolResult, command_scope_digest,
};

use super::evidence::is_sha256_digest;
use super::workspace::canonical_or_original;
use super::{SharedSandboxBackend, evaluation_blocker};

static COMMAND_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Serialize)]
/// Evaluation command 的脱敏执行诊断。
pub(super) struct CommandDiagnostic {
    pub(super) phase: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) duration_ms: Option<u64>,
}

impl CommandDiagnostic {
    /// 从命令结果构造阶段诊断。
    pub(super) fn new(phase: impl Into<String>, result: &CommandResult) -> Self {
        Self {
            phase: phase.into(),
            exit_code: result.exit_code,
            duration_ms: Some(result.duration_ms),
        }
    }

    /// Project one producer-owned Agent command result without inventing process details.
    pub(super) fn from_agent_tool_result(result: &ToolResult) -> AgentCommandDiagnosticProjection {
        let Some(audit) = result.audit_metadata().and_then(|value| value.as_object()) else {
            return if result.result_id.is_none()
                && matches!(
                    result.failure_kind,
                    Some(
                        ToolFailureKind::Input
                            | ToolFailureKind::Visibility
                            | ToolFailureKind::Capability
                            | ToolFailureKind::Policy
                            | ToolFailureKind::PermissionProfile
                            | ToolFailureKind::WorkspaceBoundary
                            | ToolFailureKind::ProtectedPath
                            | ToolFailureKind::Approval
                            | ToolFailureKind::Cancelled
                    )
                ) {
                AgentCommandDiagnosticProjection::NotExecuted
            } else {
                AgentCommandDiagnosticProjection::Unknown
            };
        };
        if result.result_id.is_none()
            && (audit
                .get("executor_started")
                .and_then(serde_json::Value::as_bool)
                == Some(false)
                || audit
                    .get("sandbox_backend")
                    .and_then(serde_json::Value::as_str)
                    == Some("not_executed")
                || audit
                    .get("sandbox_enforcement")
                    .and_then(serde_json::Value::as_str)
                    == Some("not_executed"))
        {
            return AgentCommandDiagnosticProjection::NotExecuted;
        }
        let enforcement = match audit
            .get("sandbox_enforcement")
            .and_then(serde_json::Value::as_str)
        {
            Some("strict") => SandboxBackendEnforcement::Strict,
            Some("restricted_token") => SandboxBackendEnforcement::RestrictedToken,
            Some("unavailable") => SandboxBackendEnforcement::Unavailable,
            _ => return AgentCommandDiagnosticProjection::Unknown,
        };
        let Some(result_id) = result
            .result_id
            .as_deref()
            .filter(|id| is_sha256_digest(id))
        else {
            return AgentCommandDiagnosticProjection::Unknown;
        };
        let Some(scope_digest) = audit
            .get("command_scope_digest")
            .and_then(serde_json::Value::as_str)
            .filter(|digest| is_sha256_digest(digest))
        else {
            return AgentCommandDiagnosticProjection::Unknown;
        };
        if scope_digest != result_id
            || audit
                .get("command_provenance")
                .and_then(serde_json::Value::as_str)
                != Some("agent_requested")
            || audit
                .get("sandbox_backend")
                .and_then(serde_json::Value::as_str)
                .is_none_or(str::is_empty)
            || audit
                .get("local_process_fallback")
                .and_then(serde_json::Value::as_bool)
                .is_none()
            || audit
                .get("executor_started")
                .and_then(serde_json::Value::as_bool)
                == Some(false)
        {
            return AgentCommandDiagnosticProjection::Unknown;
        }
        let local_process_fallback = audit
            .get("local_process_fallback")
            .and_then(serde_json::Value::as_bool)
            .expect("fallback checked above");
        AgentCommandDiagnosticProjection::Executed {
            diagnostic: Self {
                phase: "agent.command".to_string(),
                exit_code: None,
                duration_ms: None,
            },
            strict_sandboxed: enforcement == SandboxBackendEnforcement::Strict
                && !local_process_fallback,
            local_process_fallback,
        }
    }

    pub(super) fn for_spec(phase: impl Into<String>, result: &CommandResult) -> Self {
        Self::new(phase, result)
    }
}

#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)]
pub(super) enum AgentCommandDiagnosticProjection {
    Executed {
        diagnostic: CommandDiagnostic,
        strict_sandboxed: bool,
        local_process_fallback: bool,
    },
    NotExecuted,
    Unknown,
}

/// 判断命令是否由严格 sandbox 执行；调用方必须传入真实 producer 结果。
pub(super) fn command_is_strictly_sandboxed(result: &CommandResult) -> bool {
    !result.sandbox.local_process_fallback
        && result.sandbox.enforcement == SandboxBackendEnforcement::Strict
}

/// 计算 manifest 命令的稳定 scope digest。
pub(super) fn command_scope_digest_for_spec(
    workspace: &Path,
    command: &CommandSpec,
    default_timeout_seconds: u64,
) -> Result<String, String> {
    let command = project_command_spec(workspace, command)?;
    let cwd = resolve_command_cwd(workspace, command.cwd.as_ref().map(|cwd| cwd.as_str()))?;
    Ok(command_scope_digest(
        command.argv.as_slice(),
        &cwd.to_string_lossy(),
        command.timeout_seconds.unwrap_or(default_timeout_seconds),
        &SandboxFilesystemMode::WorkspaceWrite,
        &sandbox_network_mode(command.network_access),
    ))
}

/// 将 manifest 命令转换为 sandbox 请求并执行。
pub(super) fn run_command_spec(
    workspace: &Path,
    command: &CommandSpec,
    default_timeout_seconds: u64,
    sandbox_backend: SharedSandboxBackend,
) -> Result<CommandResult, String> {
    let command = project_command_spec(workspace, command)?;
    let cwd = resolve_command_cwd(workspace, command.cwd.as_ref().map(|cwd| cwd.as_str()))?;
    Ok(run_raw_command(
        workspace,
        &cwd,
        command.argv.as_slice().to_vec(),
        command.timeout_seconds.unwrap_or(default_timeout_seconds),
        sandbox_network_mode(command.network_access),
        sandbox_backend,
    ))
}

/// Projects a manifest command onto an executable layout proven by the prepared workspace.
pub(super) fn project_command_spec(
    workspace: &Path,
    command: &CommandSpec,
) -> Result<CommandSpec, String> {
    let mut projected = command.clone();
    #[cfg(windows)]
    let argv = {
        let mut argv = command.argv.as_slice().to_vec();
        let cwd = projection_cwd(workspace, command.cwd.as_ref().map(|cwd| cwd.as_str()))?;
        if let Some(executable) = argv.first_mut()
            && let Some(cwd) = cwd.as_deref()
            && let Some(windows_executable) = windows_venv_executable(cwd, executable)
        {
            *executable = windows_executable;
        }
        argv
    };
    #[cfg(not(windows))]
    let argv = {
        let _ = workspace;
        command.argv.as_slice().to_vec()
    };
    projected.argv = Argv::new(argv)?;
    Ok(projected)
}

#[cfg(windows)]
fn projection_cwd(workspace: &Path, cwd: Option<&str>) -> Result<Option<PathBuf>, String> {
    let workspace = fs::canonicalize(workspace).map_err(|error| {
        format!(
            "failed to resolve workspace {} for command projection: {error}",
            workspace.display()
        )
    })?;
    let candidate = cwd
        .map(|cwd| workspace.join(cwd))
        .unwrap_or_else(|| workspace.clone());
    match fs::canonicalize(&candidate) {
        Ok(canonical) => {
            if !canonical.starts_with(&workspace) {
                return Err(format!(
                    "evaluation command cwd escapes workspace: {}",
                    canonical.display()
                ));
            }
            Ok(Some(canonical))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!(
            "failed to resolve command cwd {} for projection: {error}",
            candidate.display()
        )),
    }
}

#[cfg(windows)]
fn windows_venv_executable(cwd: &Path, executable: &str) -> Option<String> {
    use std::path::Component;

    let executable = Path::new(executable);
    if executable.is_absolute()
        || executable.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
        || !executable
            .file_name()
            .is_some_and(|name| name.eq_ignore_ascii_case("python"))
    {
        return None;
    }
    let binary_dir = executable.parent()?;
    if !binary_dir
        .file_name()
        .is_some_and(|name| name.eq_ignore_ascii_case("bin"))
    {
        return None;
    }
    let environment = binary_dir.parent()?;
    let environment_root = cwd.join(environment);
    if !is_plain_directory(&environment_root)
        || !is_plain_file(&environment_root.join("pyvenv.cfg"))
    {
        return None;
    }
    let windows_executable = environment.join("Scripts").join("python.exe");
    if !is_plain_file(&cwd.join(&windows_executable)) {
        return None;
    }
    Some(windows_executable.to_string_lossy().into_owned())
}

#[cfg(windows)]
fn is_plain_file(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .map(|metadata| metadata.file_type().is_file() && !metadata.file_type().is_symlink())
        .unwrap_or(false)
}

#[cfg(windows)]
fn is_plain_directory(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .map(|metadata| metadata.file_type().is_dir() && !metadata.file_type().is_symlink())
        .unwrap_or(false)
}

/// 执行已经解析的 Evaluation command 请求。
pub(super) fn run_raw_command(
    workspace: &Path,
    cwd: &Path,
    argv: Vec<String>,
    timeout_seconds: u64,
    network: SandboxNetworkMode,
    sandbox_backend: SharedSandboxBackend,
) -> CommandResult {
    let workspace = canonical_or_original(workspace);
    let cwd = canonical_or_original(cwd);
    let request = CommandRequest::project_verification(
        next_command_id(),
        argv,
        cwd.to_string_lossy().into_owned(),
        workspace.to_string_lossy().into_owned(),
    );
    execute_command_request(
        request,
        timeout_seconds,
        network,
        SandboxFilesystemMode::WorkspaceWrite,
        sandbox_backend,
    )
}

/// 在 materialized task-like workspace 中执行固定的普通 strict no-op。
///
/// 该探针故意复用 Evaluation manifest command 的普通 `project_verification` 来源，
/// 以便验证真实 task/trial/agent 路径，而不是只验证 trusted preparation workspace。
pub(super) fn run_task_workspace_preflight_command(
    workspace: &Path,
    sandbox_backend: SharedSandboxBackend,
) -> CommandResult {
    let argv = if cfg!(windows) {
        vec![
            "cmd.exe".to_string(),
            "/d".to_string(),
            "/c".to_string(),
            "exit".to_string(),
            "0".to_string(),
        ]
    } else {
        vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            "exit 0".to_string(),
        ]
    };
    run_raw_command(
        workspace,
        workspace,
        argv,
        30,
        SandboxNetworkMode::Denied,
        sandbox_backend,
    )
}

/// 执行产品控制面固定的工作区准备操作。
///
/// 该边界仅用于 remote source probe、clone/checkout/init 等 Evaluation 内部步骤；manifest 命令和模型脚本仍走
/// protected-path enforcement 完整开启的普通路径。
pub(super) fn run_workspace_preparation_command(
    workspace: &Path,
    cwd: &Path,
    argv: Vec<String>,
    timeout_seconds: u64,
    network: SandboxNetworkMode,
    sandbox_backend: SharedSandboxBackend,
) -> CommandResult {
    let workspace = canonical_or_original(workspace);
    let cwd = canonical_or_original(cwd);
    let request = CommandRequest::trusted_workspace_preparation(
        next_command_id(),
        argv,
        cwd.to_string_lossy().into_owned(),
        workspace.to_string_lossy().into_owned(),
    );
    execute_command_request(
        request,
        timeout_seconds,
        network,
        SandboxFilesystemMode::WorkspaceWrite,
        sandbox_backend,
    )
}

/// 执行只读的 Evaluation 远程 source 可达性探针。
///
/// 请求仍使用 trusted workspace-preparation 来源以复用同一严格 adapter，但只允许读取，
/// 因而不会把 `git ls-remote` 的网络探针变成 workspace 写入操作。
pub(super) fn run_workspace_preparation_read_only_command(
    workspace: &Path,
    cwd: &Path,
    argv: Vec<String>,
    timeout_seconds: u64,
    network: SandboxNetworkMode,
    sandbox_backend: SharedSandboxBackend,
) -> CommandResult {
    let workspace = canonical_or_original(workspace);
    let cwd = canonical_or_original(cwd);
    let request = CommandRequest::trusted_workspace_preparation(
        next_command_id(),
        argv,
        cwd.to_string_lossy().into_owned(),
        workspace.to_string_lossy().into_owned(),
    );
    execute_command_request(
        request,
        timeout_seconds,
        network,
        SandboxFilesystemMode::ReadOnly,
        sandbox_backend,
    )
}

fn execute_command_request(
    mut request: CommandRequest,
    timeout_seconds: u64,
    network: SandboxNetworkMode,
    filesystem: SandboxFilesystemMode,
    sandbox_backend: SharedSandboxBackend,
) -> CommandResult {
    request.timeout_seconds = timeout_seconds;
    request.network.mode = network;
    request.filesystem.mode = filesystem;
    request.environment = CommandEnvironmentPolicy::Isolated;
    sandbox_backend.execute(&request)
}

fn resolve_command_cwd(workspace: &Path, cwd: Option<&str>) -> Result<PathBuf, String> {
    let workspace = fs::canonicalize(workspace).map_err(|error| {
        format!(
            "failed to resolve workspace {}: {error}",
            workspace.display()
        )
    })?;
    let candidate = cwd
        .map(|cwd| workspace.join(cwd))
        .unwrap_or_else(|| workspace.clone());
    let candidate = fs::canonicalize(&candidate).map_err(|error| {
        format!(
            "failed to resolve command cwd {}: {error}",
            candidate.display()
        )
    })?;
    if !candidate.starts_with(&workspace) {
        return Err(format!(
            "evaluation command cwd escapes workspace: {}",
            candidate.display()
        ));
    }
    Ok(candidate)
}

/// 判断命令是否成功且满足 sandbox 门禁。
pub(super) fn command_succeeded(result: &CommandResult) -> bool {
    result.execution_status == CommandExecutionStatus::Completed
        && result.semantic_status == CommandSemanticStatus::Succeeded
        && result.workspace_mutation != singularity_tools::WorkspaceMutation::Unknown
        && !result.sandbox.local_process_fallback
        && result.sandbox.enforcement == singularity_tools::SandboxBackendEnforcement::Strict
}

/// 判断固定 no-op 是否在严格 sandbox 下完成且确认工作区未变化。
pub(super) fn unchanged_command_succeeded(result: &CommandResult) -> bool {
    command_succeeded(result)
        && result.workspace_mutation == singularity_tools::WorkspaceMutation::Unchanged
}

/// 将 sandbox/backend 状态映射为基础设施 blocker。
pub(super) fn infrastructure_blocker(
    result: &CommandResult,
    context: &str,
) -> Option<EvaluationBlocker> {
    if result.sandbox.local_process_fallback {
        return Some(evaluation_blocker(
            BlockerKind::Sandbox,
            format!("{context}: local process fallback is forbidden"),
        ));
    }
    if result.sandbox.enforcement != singularity_tools::SandboxBackendEnforcement::Strict {
        let detail = result.stderr_preview.trim();
        let message = if detail.is_empty() {
            format!("{context}: strict sandbox enforcement is required")
        } else {
            format!("{context}: strict sandbox enforcement is required: {detail}")
        };
        return Some(evaluation_blocker(BlockerKind::Sandbox, message));
    }
    match result.execution_status {
        CommandExecutionStatus::Completed
            if result.semantic_status == CommandSemanticStatus::Succeeded
                && result.workspace_mutation == singularity_tools::WorkspaceMutation::Unknown =>
        {
            Some(evaluation_blocker(
                BlockerKind::Sandbox,
                format!("{context}: workspace mutation could not be verified"),
            ))
        }
        CommandExecutionStatus::Completed => None,
        CommandExecutionStatus::BackendError
        | CommandExecutionStatus::PolicyDenied
        | CommandExecutionStatus::ReviewRequired
        | CommandExecutionStatus::Unsupported => Some(evaluation_blocker(
            BlockerKind::Sandbox,
            format!("{context}: {}", result.stderr_preview),
        )),
        CommandExecutionStatus::ExecutableUnavailable
        | CommandExecutionStatus::TimedOut
        | CommandExecutionStatus::Cancelled => Some(evaluation_blocker(
            BlockerKind::Environment,
            format!("{context}: {}", result.stderr_preview),
        )),
    }
}

/// 为命令结果生成最终 blocker。
pub(super) fn command_blocker(
    result: &CommandResult,
    default_kind: BlockerKind,
    context: &str,
) -> EvaluationBlocker {
    infrastructure_blocker(result, context).unwrap_or_else(|| {
        evaluation_blocker(
            default_kind,
            format!("{context}: {}", result.stderr_preview),
        )
    })
}

/// 将 Evaluation 网络策略投影为 sandbox 网络模式。
pub(super) fn sandbox_network_mode(network_access: NetworkAccess) -> SandboxNetworkMode {
    match network_access {
        NetworkAccess::Denied => SandboxNetworkMode::Denied,
        NetworkAccess::Allowed => SandboxNetworkMode::Allowed,
    }
}

fn next_command_id() -> String {
    let sequence = COMMAND_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("evaluation_command_{sequence}")
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use singularity_tools::{
        SandboxBackend, SandboxBackendEnforcement, SandboxCapabilities, WorkspaceMutation,
    };

    struct EnvironmentCaptureBackend;

    impl SandboxBackend for EnvironmentCaptureBackend {
        fn name(&self) -> &'static str {
            "environment_capture"
        }

        fn capabilities(&self) -> SandboxCapabilities {
            SandboxCapabilities::strict().with_change_detection()
        }

        fn execute(&self, request: &CommandRequest) -> CommandResult {
            assert!(!request.is_trusted_workspace_preparation());
            assert_eq!(request.environment, CommandEnvironmentPolicy::Isolated);
            CommandResult::completed(&request.command_id, "ok")
                .with_workspace_mutation(WorkspaceMutation::Unchanged)
                .with_sandbox_execution(self.name(), SandboxBackendEnforcement::Strict)
        }
    }

    #[test]
    fn evaluation_raw_commands_use_isolated_environment() {
        let workspace = tempfile::tempdir().expect("workspace");
        let result = run_raw_command(
            workspace.path(),
            workspace.path(),
            vec!["test-command".to_string()],
            30,
            SandboxNetworkMode::Denied,
            Arc::new(EnvironmentCaptureBackend),
        );

        assert!(command_succeeded(&result));
    }

    #[test]
    fn task_workspace_preflight_uses_an_ordinary_strict_noop() {
        let workspace = tempfile::tempdir().expect("workspace");
        let result = run_task_workspace_preflight_command(
            workspace.path(),
            Arc::new(EnvironmentCaptureBackend),
        );

        assert!(command_succeeded(&result));
    }

    struct WorkspacePreparationCaptureBackend;

    impl SandboxBackend for WorkspacePreparationCaptureBackend {
        fn name(&self) -> &'static str {
            "workspace_preparation_capture"
        }

        fn capabilities(&self) -> SandboxCapabilities {
            SandboxCapabilities::strict().with_change_detection()
        }

        fn execute(&self, request: &CommandRequest) -> CommandResult {
            assert!(request.is_trusted_workspace_preparation());
            assert_eq!(request.environment, CommandEnvironmentPolicy::Isolated);
            CommandResult::completed(&request.command_id, "ok")
                .with_workspace_mutation(WorkspaceMutation::Changed)
                .with_sandbox_execution(self.name(), SandboxBackendEnforcement::Strict)
        }
    }

    #[test]
    fn workspace_preparation_commands_use_internal_trusted_origin() {
        let workspace = tempfile::tempdir().expect("workspace");
        let result = run_workspace_preparation_command(
            workspace.path(),
            workspace.path(),
            vec!["git".to_string(), "init".to_string()],
            30,
            SandboxNetworkMode::Denied,
            Arc::new(WorkspacePreparationCaptureBackend),
        );

        assert!(command_succeeded(&result));
    }

    #[test]
    fn successful_exit_with_unknown_workspace_mutation_fails_closed() {
        let result = CommandResult::completed("unknown_mutation", "ok")
            .with_workspace_mutation(WorkspaceMutation::Unknown)
            .with_sandbox_execution("strict", SandboxBackendEnforcement::Strict);

        assert!(!command_succeeded(&result));
        assert_eq!(
            infrastructure_blocker(&result, "verification command failed")
                .expect("unknown mutation must block")
                .kind,
            BlockerKind::Sandbox
        );
    }

    #[test]
    fn strict_noop_requires_unchanged_workspace_mutation() {
        let result = CommandResult::completed("changed_mutation", "ok")
            .with_workspace_mutation(WorkspaceMutation::Changed)
            .with_sandbox_execution("strict", SandboxBackendEnforcement::Strict);

        assert!(!unchanged_command_succeeded(&result));
    }

    #[test]
    fn missing_host_executable_is_an_environment_blocker() {
        let result = CommandResult::executable_unavailable(
            "missing_tool",
            "required executable 'python' was not found on host PATH",
        )
        .with_sandbox_execution("windows", SandboxBackendEnforcement::Strict);

        let blocker = infrastructure_blocker(&result, "setup command failed")
            .expect("unavailable evaluation executable must block");

        assert_eq!(blocker.kind, BlockerKind::Environment);
        assert!(blocker.message.contains("'python'"));
        assert!(!blocker.message.contains("C:\\"));
    }

    #[test]
    fn unavailable_enforcement_preserves_bounded_backend_detail() {
        let result = CommandResult::backend_error(
            "unavailable",
            "open protected marker failed: capability unavailable",
        )
        .with_sandbox_execution("windows", SandboxBackendEnforcement::Unavailable);

        let blocker = infrastructure_blocker(&result, "task workspace preflight failed")
            .expect("unavailable enforcement must block");

        assert_eq!(blocker.kind, BlockerKind::Sandbox);
        assert!(
            blocker
                .message
                .contains("strict sandbox enforcement is required")
        );
        assert!(blocker.message.contains("open protected marker failed"));
    }

    #[test]
    fn completed_nonzero_errors_are_semantic_failures() {
        for mutation in [WorkspaceMutation::Unchanged, WorkspaceMutation::Unknown] {
            for (stdout, stderr) in [
                ("", "Filename too long"),
                ("THE FILENAME OR EXTENSION IS TOO LONG", ""),
                ("os error 206", ""),
                ("", "fatal error LNK1104: cannot open file 'missing.obj'"),
            ] {
                let result = CommandResult::executed("path_error", 101, 0, stdout, stderr, false)
                    .with_workspace_mutation(mutation)
                    .with_sandbox_execution("windows", SandboxBackendEnforcement::Strict);

                assert!(infrastructure_blocker(&result, "verification command failed").is_none());
            }
        }
    }

    #[test]
    fn strict_command_evidence_requires_strict_enforcement() {
        for (enforcement, expected) in [
            (SandboxBackendEnforcement::Strict, true),
            (SandboxBackendEnforcement::RestrictedToken, false),
            (SandboxBackendEnforcement::Unavailable, false),
        ] {
            let result = CommandResult::completed("command", "ok")
                .with_sandbox_execution("test", enforcement)
                .with_workspace_mutation(singularity_tools::WorkspaceMutation::Unchanged);

            assert_eq!(command_is_strictly_sandboxed(&result), expected);
            assert_eq!(command_succeeded(&result), expected);
            assert_eq!(
                infrastructure_blocker(&result, "verification command").is_none(),
                expected
            );
        }
    }

    #[cfg(windows)]
    struct ProjectedCommandCaptureBackend;

    #[cfg(windows)]
    impl SandboxBackend for ProjectedCommandCaptureBackend {
        fn name(&self) -> &'static str {
            "projected_command_capture"
        }

        fn capabilities(&self) -> SandboxCapabilities {
            SandboxCapabilities::strict().with_change_detection()
        }

        fn execute(&self, request: &CommandRequest) -> CommandResult {
            assert_eq!(
                Path::new(&request.argv[0]),
                Path::new(".venv").join("Scripts").join("python.exe")
            );
            assert_eq!(&request.argv[1..], ["-m", "pytest"]);
            CommandResult::completed(&request.command_id, "ok")
                .with_workspace_mutation(WorkspaceMutation::Unchanged)
                .with_sandbox_execution(self.name(), SandboxBackendEnforcement::Strict)
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_venv_projection_is_shared_by_execution_and_scope_digest() {
        let workspace = tempfile::tempdir().expect("workspace");
        std::fs::create_dir_all(workspace.path().join(".venv").join("Scripts"))
            .expect("venv scripts directory");
        std::fs::write(
            workspace.path().join(".venv").join("pyvenv.cfg"),
            "home = C:\\Python",
        )
        .expect("venv marker");
        std::fs::write(
            workspace
                .path()
                .join(".venv")
                .join("Scripts")
                .join("python.exe"),
            b"fixture",
        )
        .expect("venv executable");
        let command = CommandSpec {
            argv: Argv::new(vec![
                ".venv/bin/python".to_string(),
                "-m".to_string(),
                "pytest".to_string(),
            ])
            .expect("argv"),
            cwd: None,
            timeout_seconds: Some(45),
            network_access: NetworkAccess::Denied,
        };

        let projected = project_command_spec(workspace.path(), &command).expect("projection");
        assert_eq!(
            Path::new(&projected.argv.as_slice()[0]),
            Path::new(".venv").join("Scripts").join("python.exe")
        );
        assert_eq!(
            command_scope_digest_for_spec(workspace.path(), &command, 30).expect("scope"),
            command_scope_digest_for_spec(workspace.path(), &projected, 30).expect("scope")
        );
        let result = run_command_spec(
            workspace.path(),
            &command,
            30,
            Arc::new(ProjectedCommandCaptureBackend),
        )
        .expect("run projected command");
        assert!(command_succeeded(&result));
    }

    #[cfg(windows)]
    #[test]
    fn windows_non_venv_executable_is_not_rewritten() {
        let workspace = tempfile::tempdir().expect("workspace");
        let command = CommandSpec {
            argv: Argv::new(vec![
                "tools/bin/python".to_string(),
                "--version".to_string(),
            ])
            .expect("argv"),
            cwd: None,
            timeout_seconds: None,
            network_access: NetworkAccess::Denied,
        };

        let projected = project_command_spec(workspace.path(), &command).expect("projection");
        assert_eq!(projected, command);
    }

    #[cfg(windows)]
    #[test]
    fn windows_venv_projection_respects_command_cwd() {
        let workspace = tempfile::tempdir().expect("workspace");
        let nested = workspace.path().join("nested");
        std::fs::create_dir(&nested).expect("nested cwd");
        std::fs::create_dir_all(nested.join(".venv").join("Scripts"))
            .expect("nested venv scripts directory");
        std::fs::write(nested.join(".venv").join("pyvenv.cfg"), "home = C:\\Python")
            .expect("nested venv marker");
        std::fs::write(
            nested.join(".venv").join("Scripts").join("python.exe"),
            b"fixture",
        )
        .expect("nested venv executable");
        let command = CommandSpec {
            argv: Argv::new(vec![
                ".venv/bin/python".to_string(),
                "--version".to_string(),
            ])
            .expect("argv"),
            cwd: Some(crate::RelativePath::new("nested").expect("cwd")),
            timeout_seconds: None,
            network_access: NetworkAccess::Denied,
        };

        let projected = project_command_spec(workspace.path(), &command).expect("projection");
        assert_eq!(
            projected.argv.as_slice()[0].as_str(),
            Path::new(".venv")
                .join("Scripts")
                .join("python.exe")
                .to_string_lossy()
                .as_ref()
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_venv_projection_does_not_substitute_a_root_environment_for_nested_cwd() {
        let workspace = tempfile::tempdir().expect("workspace");
        let nested = workspace.path().join("nested");
        std::fs::create_dir(&nested).expect("nested cwd");
        std::fs::create_dir_all(workspace.path().join(".venv").join("Scripts"))
            .expect("root venv scripts directory");
        std::fs::write(
            workspace.path().join(".venv").join("pyvenv.cfg"),
            "home = C:\\Python",
        )
        .expect("root venv marker");
        std::fs::write(
            workspace
                .path()
                .join(".venv")
                .join("Scripts")
                .join("python.exe"),
            b"fixture",
        )
        .expect("root venv executable");
        let command = CommandSpec {
            argv: Argv::new(vec![
                ".venv/bin/python".to_string(),
                "--version".to_string(),
            ])
            .expect("argv"),
            cwd: Some(crate::RelativePath::new("nested").expect("cwd")),
            timeout_seconds: None,
            network_access: NetworkAccess::Denied,
        };

        let projected = project_command_spec(workspace.path(), &command).expect("projection");
        assert_eq!(projected, command);
    }
}
