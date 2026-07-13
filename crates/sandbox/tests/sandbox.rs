use schemars::schema_for;
use singularity_sandbox::{
    CommandExecutionStatus, CommandRequest, CommandResult, CommandSemanticStatus, SandboxBackend,
    SandboxBackendEnforcement, SandboxCapabilities, WindowsSandboxBackend, bound_command_output,
};
#[cfg(windows)]
use singularity_sandbox::{SandboxFilesystemMode, SandboxNetworkMode};
use std::path::Path;

const SANDBOX_SRC: &str = include_str!("../src/lib.rs");
const FORBIDDEN_LOCAL_PROCESS_SURFACES: [&str; 11] = [
    "CommandExecutor",
    "PatchExecutor",
    "ProcessManager",
    "run_local",
    "capture_pipe",
    "kill_process_tree",
    "fs::write",
    "fs::remove_file",
    "std::process::Command",
    ".spawn()",
    "taskkill",
];
const FORBIDDEN_RELAXED_SANDBOX_CONTRACTS: [&str; 2] = ["HostWorkspace", "Relaxed"];

#[test]
fn command_request_and_result_are_schema_backed_boundaries() {
    let request = CommandRequest::project_verification(
        "command_1",
        vec!["cargo".to_string(), "test".to_string()],
        ".",
        "C:/repo",
    );
    let result = CommandResult::completed("command_1", "passed");

    let request_value = serde_json::to_value(&request).expect("serialize command request");
    let result_value = serde_json::to_value(&result).expect("serialize command result");

    assert_eq!(request_value["network"]["mode"], "denied");
    assert_eq!(result_value["semantic_status"], "succeeded");
    assert_eq!(result_value["redacted"], true);
    assert_eq!(request.permission_resource(), "cargo test");
    assert_eq!(
        schema_for!(CommandRequest)
            .schema
            .metadata
            .unwrap()
            .title
            .unwrap(),
        "CommandRequest"
    );
    assert_eq!(
        schema_for!(CommandResult)
            .schema
            .metadata
            .unwrap()
            .title
            .unwrap(),
        "CommandResult"
    );
}

#[test]
fn command_resource_normalization_belongs_to_command_boundary() {
    let request = CommandRequest::project_verification(
        "command_1",
        vec![
            "cmd.exe".to_string(),
            "/c".to_string(),
            "cargo".to_string(),
            "test".to_string(),
        ],
        ".",
        "C:/repo",
    );

    assert_eq!(request.permission_resource(), "cargo test");
}

#[test]
fn command_executor_fails_closed_when_sandbox_is_required_without_backend() {
    let request =
        CommandRequest::project_verification("command_1", vec!["git".to_string()], ".", "C:/repo");
    assert!(request.requires_sandbox());

    let result = CommandResult::sandbox_backend_unavailable(&request.command_id);

    assert_eq!(
        result.execution_status,
        CommandExecutionStatus::BackendError
    );
    assert_eq!(result.semantic_status, CommandSemanticStatus::PolicyBlocked);
    assert!(result.stderr_preview.contains("sandbox-required"));
}

#[test]
fn cancelled_command_result_is_distinct_from_timeout() {
    let result = CommandResult::cancelled("command_cancelled", 25);

    assert_eq!(result.execution_status, CommandExecutionStatus::Cancelled);
    assert_eq!(result.semantic_status, CommandSemanticStatus::Cancelled);
    assert_eq!(result.duration_ms, 25);
    assert!(!result.timed_out);
    assert!(result.stderr_preview.contains("cancelled"));
}

#[test]
fn command_boundary_does_not_expose_host_workspace_local_process_executor() {
    for forbidden in FORBIDDEN_LOCAL_PROCESS_SURFACES
        .iter()
        .chain(FORBIDDEN_RELAXED_SANDBOX_CONTRACTS.iter())
    {
        assert!(
            !SANDBOX_SRC.contains(forbidden),
            "forbidden local process surface remains: {forbidden}",
        );
    }
}

#[test]
fn command_policy_denied_result_does_not_require_process_or_backend_execution() {
    let result = CommandResult::policy_denied("command_1", "policy denied command execution");

    assert_eq!(
        result.execution_status,
        CommandExecutionStatus::PolicyDenied
    );
    assert_eq!(result.semantic_status, CommandSemanticStatus::PolicyBlocked);
    assert!(result.stderr_preview.contains("policy denied"));
}

#[test]
fn sandbox_capabilities_report_actual_enforcement_strength() {
    assert!(SandboxCapabilities::strict().supports_command_execution());
    assert_eq!(
        SandboxCapabilities::strict().enforcement(),
        SandboxBackendEnforcement::Strict
    );
    assert!(SandboxCapabilities::restricted_token().supports_command_execution());
    assert_eq!(
        SandboxCapabilities::restricted_token().enforcement(),
        SandboxBackendEnforcement::RestrictedToken
    );
    assert!(!SandboxCapabilities::unavailable().supports_command_execution());
    assert_eq!(
        SandboxCapabilities::unavailable().enforcement(),
        SandboxBackendEnforcement::Unavailable
    );

    let mut portable_strict = SandboxCapabilities::strict();
    portable_strict.restricted_token = false;
    portable_strict.job_object = false;
    assert!(portable_strict.supports_command_execution());
    assert_eq!(
        portable_strict.enforcement(),
        SandboxBackendEnforcement::Strict
    );
}

#[test]
fn command_output_is_a_safe_bounded_payload() {
    let output = bound_command_output("abcdef", 3);
    let redacted_result =
        CommandResult::completed("command_secret", "Authorization: Bearer abc123");
    let bounded_result = CommandResult::completed("command_long", "x".repeat(40_010));
    let blocked_result =
        CommandResult::policy_denied("command_blocked", "raw_prompt provider_response abc123");
    assert_eq!(output.preview, "abc");
    assert!(output.truncated);
    assert_eq!(
        redacted_result.stdout_preview,
        "[redacted sensitive command output]"
    );
    assert!(redacted_result.output_truncated);
    assert_eq!(bounded_result.stdout_preview.chars().count(), 40_000);
    assert!(bounded_result.output_truncated);
    assert_eq!(
        blocked_result.stderr_preview,
        "[redacted sensitive command output]"
    );
    assert_eq!(
        CommandResult::completed(
            "command_key",
            "provider returned sk-abcdefghijklmnopqrstuvwxyz"
        )
        .stdout_preview,
        "[redacted sensitive command output]"
    );
    assert_eq!(
        CommandResult::completed(
            "command_gh",
            "provider returned ghp_abcdefghijklmnopqrstuvwxyz123456"
        )
        .stdout_preview,
        "[redacted sensitive command output]"
    );
    assert_eq!(
        CommandResult::completed(
            "command_jwt",
            "bearer eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.signature123"
        )
        .stdout_preview,
        "[redacted sensitive command output]"
    );
    assert_eq!(
        CommandResult::completed(
            "command_tokens",
            "token count is 42 and token budget is 100"
        )
        .stdout_preview,
        "token count is 42 and token budget is 100"
    );
}

#[cfg(windows)]
#[test]
fn windows_backend_reports_strict_maximum_capabilities() {
    let backend = WindowsSandboxBackend::new();

    assert_eq!(backend.name(), "windows");
    assert_eq!(
        backend.capabilities().enforcement(),
        SandboxBackendEnforcement::Strict
    );
    assert!(backend.capabilities().filesystem_isolation);
    assert!(backend.capabilities().network_isolation);
}

#[cfg(windows)]
#[test]
fn windows_backend_denies_sensitive_cwd_before_execution() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sensitive = workspace.path().join(".ssh");
    std::fs::create_dir(&sensitive).expect("sensitive dir");
    let request = CommandRequest::project_verification(
        "command_sensitive_cwd",
        vec![
            "cmd.exe".to_string(),
            "/C".to_string(),
            "echo denied".to_string(),
        ],
        path_str(&sensitive),
        path_str(workspace.path()),
    );

    let result = WindowsSandboxBackend::new().execute(&request);

    assert_eq!(
        result.execution_status,
        CommandExecutionStatus::PolicyDenied
    );
    assert_eq!(result.semantic_status, CommandSemanticStatus::PolicyBlocked);
    assert_eq!(result.sandbox.backend, "windows");
    assert_eq!(
        result.sandbox.enforcement,
        SandboxBackendEnforcement::Unavailable
    );
}

#[cfg(windows)]
#[test]
fn windows_backend_denies_parent_traversal_before_execution() {
    let workspace = tempfile::tempdir().expect("workspace");
    let request = CommandRequest::project_verification(
        "command_parent_escape",
        vec![
            "cmd.exe".to_string(),
            "/C".to_string(),
            "type ..\\outside.txt".to_string(),
        ],
        path_str(workspace.path()),
        path_str(workspace.path()),
    );

    let result = WindowsSandboxBackend::new().execute(&request);

    assert_eq!(
        result.execution_status,
        CommandExecutionStatus::PolicyDenied
    );
    assert_eq!(result.semantic_status, CommandSemanticStatus::PolicyBlocked);
}

#[cfg(windows)]
#[test]
fn windows_backend_reports_missing_host_tool_as_spawn_failure() {
    let workspace = tempfile::tempdir().expect("workspace");
    let request = CommandRequest::project_verification(
        "command_missing_tool",
        vec!["singularity-tool-that-does-not-exist".to_string()],
        path_str(workspace.path()),
        path_str(workspace.path()),
    );

    let result = WindowsSandboxBackend::new().execute(&request);

    assert_eq!(result.execution_status, CommandExecutionStatus::SpawnFailed);
    assert_eq!(
        result.sandbox.enforcement,
        SandboxBackendEnforcement::Strict
    );
    assert_eq!(
        result.stderr_preview,
        "sandbox command spawn failed: required executable 'singularity-tool-that-does-not-exist' was not found on host PATH"
    );
    assert!(!result.sandbox.local_process_fallback);
}

#[cfg(windows)]
#[test]
fn windows_backend_rejects_danger_full_access_without_implicit_fallback() {
    let workspace = tempfile::tempdir().expect("workspace");
    let mut request = CommandRequest::project_verification(
        "command_danger",
        vec![
            "cmd.exe".to_string(),
            "/C".to_string(),
            "echo denied".to_string(),
        ],
        path_str(workspace.path()),
        path_str(workspace.path()),
    );
    request.filesystem.mode = SandboxFilesystemMode::DangerFullAccess;

    let result = WindowsSandboxBackend::new().execute(&request);

    assert_eq!(
        result.execution_status,
        CommandExecutionStatus::BackendError
    );
    assert!(result.stderr_preview.contains("danger-full-access"));
    assert!(!result.sandbox.local_process_fallback);
}

#[cfg(windows)]
#[test]
#[ignore = "requires first-run Windows UAC sandbox setup"]
fn windows_elevated_backend_executes_network_denied_command() {
    let workspace = tempfile::tempdir().expect("workspace");
    let mut request = CommandRequest::project_verification(
        "command_elevated",
        vec![
            "cmd.exe".to_string(),
            "/C".to_string(),
            "echo windows-elevated-ok".to_string(),
        ],
        path_str(workspace.path()),
        path_str(workspace.path()),
    );
    request.network.mode = SandboxNetworkMode::Denied;

    let result = WindowsSandboxBackend::new().execute(&request);

    assert_eq!(
        result.execution_status,
        CommandExecutionStatus::Completed,
        "{result:#?}"
    );
    assert_eq!(result.semantic_status, CommandSemanticStatus::Succeeded);
    assert_eq!(result.sandbox.backend, "windows_elevated");
    assert_eq!(
        result.sandbox.enforcement,
        SandboxBackendEnforcement::Strict
    );
    assert!(!result.sandbox.local_process_fallback);
}

#[cfg(not(windows))]
#[test]
fn windows_backend_is_unavailable_off_windows() {
    let workspace = tempfile::tempdir().expect("workspace");
    let request = CommandRequest::project_verification(
        "command_echo",
        vec!["echo".to_string(), "ok".to_string()],
        path_str(workspace.path()),
        path_str(workspace.path()),
    );
    let backend = WindowsSandboxBackend::new();

    let result = backend.execute(&request);

    assert_eq!(backend.name(), "windows");
    assert!(!backend.capabilities().supports_command_execution());
    assert_eq!(
        result.execution_status,
        CommandExecutionStatus::BackendError
    );
    assert_eq!(result.semantic_status, CommandSemanticStatus::PolicyBlocked);
}

fn path_str(path: &Path) -> &str {
    path.to_str().expect("utf8 path")
}
