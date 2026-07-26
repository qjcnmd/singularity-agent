//! Evaluation command 的可信执行、scope digest 和 sandbox 诊断辅助逻辑。

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::Serialize;
use singularity_evaluation::{BlockerKind, CommandSpec, EvaluationBlocker};
use singularity_policy::NetworkAccess;
use singularity_tools::{
    CommandEnvironmentPolicy, CommandExecutionStatus, CommandRequest, CommandResult,
    CommandSemanticStatus, SandboxFilesystemMode, SandboxNetworkMode, command_scope_digest,
};

use super::workspace::canonical_or_original;
use super::{SharedSandboxBackend, evaluation_blocker};

static COMMAND_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Serialize)]
/// Evaluation command 的脱敏执行诊断。
pub(super) struct CommandDiagnostic {
    pub(super) phase: String,
    execution_status: CommandExecutionStatus,
    semantic_status: CommandSemanticStatus,
    exit_code: Option<i32>,
    duration_ms: u64,
    timed_out: bool,
    output_truncated: bool,
    stdout_preview: String,
    stderr_preview: String,
    sandbox_backend: String,
    sandbox_enforcement: singularity_tools::SandboxBackendEnforcement,
    pub(super) local_process_fallback: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) scope_digest: Option<String>,
}

impl CommandDiagnostic {
    /// 从命令结果构造阶段诊断。
    pub(super) fn new(phase: impl Into<String>, result: &CommandResult) -> Self {
        Self {
            phase: phase.into(),
            execution_status: result.execution_status.clone(),
            semantic_status: result.semantic_status.clone(),
            exit_code: result.exit_code,
            duration_ms: result.duration_ms,
            timed_out: result.timed_out,
            output_truncated: result.output_truncated,
            stdout_preview: result.stdout_preview.clone(),
            stderr_preview: result.stderr_preview.clone(),
            sandbox_backend: result.sandbox.backend.clone(),
            sandbox_enforcement: result.sandbox.enforcement.clone(),
            local_process_fallback: result.sandbox.local_process_fallback,
            scope_digest: None,
        }
    }

    /// 为 manifest 命令补充稳定 scope digest。
    pub(super) fn for_spec(
        phase: impl Into<String>,
        workspace: &Path,
        command: &CommandSpec,
        default_timeout_seconds: u64,
        result: &CommandResult,
    ) -> Self {
        let mut diagnostic = Self::new(phase, result);
        diagnostic.scope_digest =
            command_scope_digest_for_spec(workspace, command, default_timeout_seconds).ok();
        diagnostic
    }

    /// 判断命令是否由严格 sandbox 执行。
    pub(super) fn is_strictly_sandboxed(&self) -> bool {
        !self.local_process_fallback
            && self.sandbox_enforcement == singularity_tools::SandboxBackendEnforcement::Strict
    }
}

/// 计算 manifest 命令的稳定 scope digest。
pub(super) fn command_scope_digest_for_spec(
    workspace: &Path,
    command: &CommandSpec,
    default_timeout_seconds: u64,
) -> Result<String, String> {
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
    request.environment = CommandEnvironmentPolicy::EvaluationIsolated;
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
        return Some(evaluation_blocker(
            BlockerKind::Sandbox,
            format!("{context}: strict sandbox enforcement is required"),
        ));
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
            assert_eq!(
                request.environment,
                CommandEnvironmentPolicy::EvaluationIsolated
            );
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
            assert_eq!(
                request.environment,
                CommandEnvironmentPolicy::EvaluationIsolated
            );
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

            assert_eq!(
                CommandDiagnostic::new("verification", &result).is_strictly_sandboxed(),
                expected
            );
            assert_eq!(command_succeeded(&result), expected);
            assert_eq!(
                infrastructure_blocker(&result, "verification command").is_none(),
                expected
            );
        }
    }
}
