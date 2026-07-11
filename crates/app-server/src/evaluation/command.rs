use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::Serialize;
use singularity_evaluation::{BlockerKind, CommandSpec, EvaluationBlocker};
use singularity_policy::NetworkAccess;
use singularity_tools::{
    CommandExecutionStatus, CommandRequest, CommandResult, CommandSemanticStatus,
    SandboxFilesystemMode, SandboxNetworkMode,
};

use super::workspace::canonical_or_original;
use super::{SharedSandboxBackend, evaluation_blocker};

static COMMAND_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Serialize)]
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
}

impl CommandDiagnostic {
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
        }
    }

    pub(super) fn is_strictly_sandboxed(&self) -> bool {
        !self.local_process_fallback
            && self.sandbox_enforcement != singularity_tools::SandboxBackendEnforcement::Unavailable
    }
}

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
    let mut request = CommandRequest::project_verification(
        next_command_id(),
        argv,
        cwd.to_string_lossy().into_owned(),
        workspace.to_string_lossy().into_owned(),
    );
    request.timeout_seconds = timeout_seconds;
    request.network.mode = network;
    request.filesystem.mode = SandboxFilesystemMode::WorkspaceWrite;
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

pub(super) fn command_succeeded(result: &CommandResult) -> bool {
    result.execution_status == CommandExecutionStatus::Completed
        && result.semantic_status == CommandSemanticStatus::Succeeded
        && !result.sandbox.local_process_fallback
        && result.sandbox.enforcement != singularity_tools::SandboxBackendEnforcement::Unavailable
}

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
    if result.sandbox.enforcement == singularity_tools::SandboxBackendEnforcement::Unavailable {
        return Some(evaluation_blocker(
            BlockerKind::Sandbox,
            format!("{context}: sandbox enforcement is unavailable"),
        ));
    }
    match result.execution_status {
        CommandExecutionStatus::Completed => None,
        CommandExecutionStatus::BackendError
        | CommandExecutionStatus::PolicyDenied
        | CommandExecutionStatus::ReviewRequired
        | CommandExecutionStatus::Unsupported => Some(evaluation_blocker(
            BlockerKind::Sandbox,
            format!("{context}: {}", result.stderr_preview),
        )),
        CommandExecutionStatus::SpawnFailed
        | CommandExecutionStatus::TimedOut
        | CommandExecutionStatus::Cancelled => Some(evaluation_blocker(
            BlockerKind::Environment,
            format!("{context}: {}", result.stderr_preview),
        )),
    }
}

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
    use super::*;
    use singularity_tools::SandboxBackendEnforcement;

    #[test]
    fn missing_host_executable_is_an_environment_blocker() {
        let result = CommandResult::spawn_failed(
            "missing_tool",
            "required executable 'python' was not found on host PATH",
        )
        .with_sandbox_execution("windows", SandboxBackendEnforcement::Strict);

        let blocker = infrastructure_blocker(&result, "setup command failed")
            .expect("spawn failure must block");

        assert_eq!(blocker.kind, BlockerKind::Environment);
        assert!(blocker.message.contains("'python'"));
        assert!(!blocker.message.contains("C:\\"));
    }
}
