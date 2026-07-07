use schemars::schema_for;
use singularity_sandbox::{
    CommandExecutionStatus, CommandRequest, CommandResult, CommandSemanticStatus, SandboxBackend,
    SandboxBackendDescriptor, SandboxCapabilities, SandboxPolicy, bound_command_output,
    changed_files_inside_workspace, git_diff_request, git_status_request, redacted_child_env,
};
use std::collections::BTreeMap;

const SANDBOX_SRC: &str = include_str!("../src/lib.rs");
const FORBIDDEN_LOCAL_PROCESS_SURFACES: [&str; 12] = [
    "CommandExecutor",
    "PatchExecutor",
    "ProcessManager",
    "local_process",
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

struct TestBackend;

impl SandboxBackend for TestBackend {
    fn name(&self) -> &'static str {
        "test"
    }

    fn capabilities(&self) -> SandboxCapabilities {
        SandboxCapabilities::strict()
    }
}

#[test]
fn sandbox_policy_and_backend_contract_are_serializable() {
    let policy = SandboxPolicy::isolated_verification("C:/repo");
    let value = serde_json::to_value(&policy).expect("serialize sandbox policy");

    assert_eq!(value["profile"], "isolated_verification");
    assert_eq!(value["network"]["mode"], "denied");
    assert_eq!(TestBackend.name(), "test");
    assert!(TestBackend.capabilities().network_isolation);
}

#[test]
fn command_request_and_result_are_schema_backed_boundaries() {
    let request = CommandRequest::project_verification(
        "command_1",
        vec!["python".to_string(), "-m".to_string(), "pytest".to_string()],
        ".",
        "C:/repo",
    );
    let result = CommandResult::completed("command_1", "passed");

    let request_value = serde_json::to_value(&request).expect("serialize command request");
    let result_value = serde_json::to_value(&result).expect("serialize command result");

    assert_eq!(request_value["purpose"], "project_verification");
    assert_eq!(request_value["network"]["mode"], "denied");
    assert_eq!(result_value["semantic_status"], "succeeded");
    assert_eq!(result_value["redacted"], true);
    assert_eq!(request.permission_resource(), "python -m pytest");
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
            "python".to_string(),
            "-m".to_string(),
            "pytest".to_string(),
        ],
        ".",
        "C:/repo",
    );

    assert_eq!(request.permission_resource(), "python -m pytest");
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
fn git_helpers_create_sandboxed_command_requests_not_git_execution_wrappers() {
    let status = git_status_request("git_status", ".", "C:/repo");
    let diff = git_diff_request("git_diff", ".", "C:/repo");

    assert_eq!(status.permission_resource(), "git status --porcelain=v1");
    assert_eq!(diff.permission_resource(), "git diff --");
    assert!(status.requires_sandbox());
    assert!(diff.requires_sandbox());
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
fn patch_schema_objects_are_snapshotted() {
    assert_eq!(
        schema_for!(singularity_sandbox::PatchChange)
            .schema
            .metadata
            .unwrap()
            .title
            .unwrap(),
        "PatchChange"
    );
    assert_eq!(
        schema_for!(singularity_sandbox::PatchResult)
            .schema
            .metadata
            .unwrap()
            .title
            .unwrap(),
        "PatchResult"
    );
}

#[test]
fn sandbox_backend_descriptor_is_a_serializable_contract() {
    let descriptor = SandboxBackendDescriptor::strict("windows_elevated");
    let value = serde_json::to_value(&descriptor).expect("serialize backend descriptor");
    let schema = schema_for!(SandboxBackendDescriptor);
    let schema_text = serde_json::to_string(&schema).expect("serialize schema");

    assert_eq!(value["backend"], "windows_elevated");
    assert_eq!(value["enforcement"], "strict");
    assert!(!schema_text.contains("reduced"));
    assert!(
        value["capabilities"]["filesystem_isolation"]
            .as_bool()
            .unwrap()
    );
    assert_eq!(
        schema_for!(SandboxBackendDescriptor)
            .schema
            .metadata
            .unwrap()
            .title
            .unwrap(),
        "SandboxBackendDescriptor"
    );
}

#[test]
fn command_output_and_child_environment_are_safe_bounded_projections() {
    let output = bound_command_output("abcdef", 3);
    let redacted_result =
        CommandResult::completed("command_secret", "Authorization: Bearer abc123");
    let bounded_result = CommandResult::completed("command_long", "x".repeat(40_010));
    let blocked_result =
        CommandResult::policy_denied("command_blocked", "raw_prompt provider_response abc123");
    let mut env = BTreeMap::new();
    env.insert("PATH".to_string(), "C:/Windows".to_string());
    env.insert("SINGULARITY_API_KEY".to_string(), "secret".to_string());

    let child_env = redacted_child_env(&env);

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
    assert_eq!(
        child_env.get("PATH").map(String::as_str),
        Some("C:/Windows")
    );
    assert!(!child_env.contains_key("SINGULARITY_API_KEY"));
}

#[test]
fn changed_file_detection_never_reports_paths_outside_workspace() {
    let files = changed_files_inside_workspace(
        "C:/repo",
        &[
            "C:/repo/src/lib.rs".to_string(),
            "C:/repo/../secrets.txt".to_string(),
            "D:/outside.txt".to_string(),
        ],
    );

    assert_eq!(files, vec!["src/lib.rs"]);
}
