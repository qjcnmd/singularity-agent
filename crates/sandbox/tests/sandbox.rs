use schemars::schema_for;
use singularity_sandbox::{
    CommandExecutionStatus, CommandRequest, CommandResult, CommandSemanticStatus, SandboxBackend,
    SandboxBackendDescriptor, SandboxCapabilities, SandboxFilesystemMode, SandboxNetworkMode,
    SandboxPolicy, UnavailableSandboxBackend, WindowsRestrictedTokenSandboxBackend,
    bound_command_output, changed_files_inside_workspace, git_diff_request, git_status_request,
    redacted_child_env,
};
use std::collections::BTreeMap;
use std::path::Path;

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

    fn execute(&self, request: &CommandRequest) -> CommandResult {
        CommandResult::sandbox_backend_unavailable(&request.command_id)
    }
}

#[test]
fn sandbox_policy_and_backend_contract_are_serializable() {
    let policy = SandboxPolicy::isolated_verification("C:/repo");
    let value = serde_json::to_value(&policy).expect("serialize sandbox policy");

    assert_eq!(value["profile"], "isolated_verification");
    assert_eq!(value["network"]["mode"], "denied");
    assert_eq!(TestBackend.name(), "test");
    assert!(TestBackend.capabilities().restricted_token);
    assert!(TestBackend.capabilities().job_object);
    assert!(TestBackend.capabilities().path_admission);
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
    assert_eq!(request_value["network"]["mode"], "allowed");
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
    let descriptor = SandboxBackendDescriptor::strict("windows_restricted_token");
    let value = serde_json::to_value(&descriptor).expect("serialize backend descriptor");
    let schema = schema_for!(SandboxBackendDescriptor);
    let schema_text = serde_json::to_string(&schema).expect("serialize schema");

    assert_eq!(value["backend"], "windows_restricted_token");
    assert_eq!(value["enforcement"], "strict");
    assert!(!schema_text.contains("reduced"));
    assert!(value["capabilities"]["restricted_token"].as_bool().unwrap());
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
fn sandbox_capabilities_distinguish_strict_command_execution_support() {
    assert!(SandboxCapabilities::strict().supports_strict_command_execution());
    assert!(!SandboxCapabilities::unavailable().supports_strict_command_execution());
}

#[test]
fn command_output_and_child_environment_are_safe_bounded_payloads() {
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

#[test]
fn unavailable_sandbox_backend_fails_closed_without_spawning() {
    let workspace = tempfile::tempdir().expect("workspace");
    let request = CommandRequest::project_verification(
        "command_echo",
        vec![
            "python".to_string(),
            "-c".to_string(),
            "print('ok')".to_string(),
        ],
        path_str(workspace.path()),
        path_str(workspace.path()),
    );
    let backend = UnavailableSandboxBackend;

    let result = backend.execute(&request);

    assert_eq!(
        result.execution_status,
        CommandExecutionStatus::BackendError
    );
    assert_eq!(result.semantic_status, CommandSemanticStatus::PolicyBlocked);
    assert_eq!(result.exit_code, None);
    assert!(result.stderr_preview.contains("sandbox backend"));
    assert!(result.redacted);
}

#[cfg(windows)]
#[test]
fn windows_restricted_token_backend_captures_output_with_controlled_process() {
    let workspace = tempfile::tempdir().expect("workspace");
    let request = CommandRequest::project_verification(
        "command_echo",
        vec![
            "cmd.exe".to_string(),
            "/C".to_string(),
            "echo sandbox-ok".to_string(),
        ],
        path_str(workspace.path()),
        path_str(workspace.path()),
    );
    let backend = WindowsRestrictedTokenSandboxBackend::new();

    let result = backend.execute(&request);

    assert_eq!(backend.name(), "windows_restricted_token");
    assert!(backend.capabilities().restricted_token);
    assert!(backend.capabilities().job_object);
    assert!(backend.capabilities().path_admission);
    assert_eq!(result.execution_status, CommandExecutionStatus::Completed);
    assert_eq!(result.semantic_status, CommandSemanticStatus::Succeeded);
    assert_eq!(result.exit_code, Some(0));
    assert!(result.stdout_preview.contains("sandbox-ok"));
    assert!(result.redacted);
}

#[cfg(windows)]
#[test]
fn windows_restricted_token_backend_runs_cmd_from_verbatim_cwd() {
    let workspace = tempfile::tempdir().expect("workspace");
    let workspace_path = path_str(workspace.path());
    let verbatim_workspace = format!(r"\\?\{workspace_path}");
    let request = CommandRequest::project_verification(
        "command_verbatim_cwd",
        vec!["cmd.exe".to_string(), "/C".to_string(), "cd".to_string()],
        verbatim_workspace.clone(),
        verbatim_workspace,
    );
    let backend = WindowsRestrictedTokenSandboxBackend::new();

    let result = backend.execute(&request);

    assert_eq!(result.execution_status, CommandExecutionStatus::Completed);
    assert_eq!(result.semantic_status, CommandSemanticStatus::Succeeded);
    assert!(result.stdout_preview.contains(workspace_path));
}

#[cfg(windows)]
#[test]
fn windows_restricted_token_backend_times_out_and_kills_job() {
    let workspace = tempfile::tempdir().expect("workspace");
    let mut request = CommandRequest::project_verification(
        "command_timeout",
        vec![
            "cmd.exe".to_string(),
            "/C".to_string(),
            "ping -n 6 127.0.0.1 >NUL".to_string(),
        ],
        path_str(workspace.path()),
        path_str(workspace.path()),
    );
    request.timeout_seconds = 1;
    let backend = WindowsRestrictedTokenSandboxBackend::new();

    let result = backend.execute(&request);

    assert_eq!(result.execution_status, CommandExecutionStatus::TimedOut);
    assert_eq!(result.semantic_status, CommandSemanticStatus::TimedOut);
    assert!(result.timed_out);
    assert!(result.stderr_preview.contains("timed out"));
}

#[cfg(windows)]
#[test]
fn windows_restricted_token_backend_denies_sensitive_cwd_before_spawn() {
    let workspace = tempfile::tempdir().expect("workspace");
    let secret_dir = workspace.path().join(".ssh");
    std::fs::create_dir(&secret_dir).expect("secret dir");
    let mut request = CommandRequest::project_verification(
        "command_denied",
        vec![
            "cmd.exe".to_string(),
            "/C".to_string(),
            "echo should-not-run".to_string(),
        ],
        path_str(&secret_dir),
        path_str(workspace.path()),
    );
    request.filesystem.mode = SandboxFilesystemMode::ReadOnly;
    let backend = WindowsRestrictedTokenSandboxBackend::new();

    let result = backend.execute(&request);

    assert_eq!(
        result.execution_status,
        CommandExecutionStatus::PolicyDenied
    );
    assert_eq!(result.semantic_status, CommandSemanticStatus::PolicyBlocked);
    assert!(result.stdout_preview.is_empty());
    assert!(!result.stderr_preview.contains(".ssh"));
}

#[cfg(windows)]
#[test]
fn windows_restricted_token_backend_denies_write_allowlist_escape() {
    let workspace = tempfile::tempdir().expect("workspace");
    let outside = tempfile::tempdir().expect("outside");
    let mut request = CommandRequest::project_verification(
        "command_denied",
        vec![
            "cmd.exe".to_string(),
            "/C".to_string(),
            "echo ok".to_string(),
        ],
        path_str(workspace.path()),
        path_str(workspace.path()),
    );
    request.filesystem.writable_paths = vec![path_str(outside.path()).to_string()];
    let backend = WindowsRestrictedTokenSandboxBackend::new();

    let result = backend.execute(&request);

    assert_eq!(
        result.execution_status,
        CommandExecutionStatus::PolicyDenied
    );
    assert_eq!(result.semantic_status, CommandSemanticStatus::PolicyBlocked);
    assert!(result.stderr_preview.contains("outside workspace"));
}

#[cfg(windows)]
#[test]
fn windows_restricted_token_backend_denies_shell_parent_traversal_before_spawn() {
    let workspace = tempfile::tempdir().expect("workspace");
    let outside = tempfile::tempdir().expect("outside");
    let outside_file = outside.path().join("outside.txt");
    std::fs::write(&outside_file, "outside-secret").expect("outside file");
    let mut request = CommandRequest::project_verification(
        "command_denied",
        vec![
            "cmd.exe".to_string(),
            "/C".to_string(),
            format!("type {}", outside_file.display()),
        ],
        path_str(workspace.path()),
        path_str(workspace.path()),
    );
    request.filesystem.mode = SandboxFilesystemMode::ReadOnly;
    let backend = WindowsRestrictedTokenSandboxBackend::new();

    let result = backend.execute(&request);

    assert_eq!(
        result.execution_status,
        CommandExecutionStatus::PolicyDenied
    );
    assert_eq!(result.semantic_status, CommandSemanticStatus::PolicyBlocked);
    assert!(result.stderr_preview.contains("outside workspace"));
}

#[cfg(windows)]
#[test]
fn windows_restricted_token_backend_denies_workspace_symlink_escape_before_spawn() {
    let workspace = tempfile::tempdir().expect("workspace");
    let outside = tempfile::tempdir().expect("outside");
    let outside_file = outside.path().join("outside.txt");
    std::fs::write(&outside_file, "outside-secret").expect("outside file");
    let link = workspace.path().join("link_to_outside.txt");
    if std::os::windows::fs::symlink_file(&outside_file, &link).is_err() {
        return;
    }
    let request = CommandRequest::project_verification(
        "command_denied",
        vec![
            "cmd.exe".to_string(),
            "/C".to_string(),
            "type link_to_outside.txt".to_string(),
        ],
        path_str(workspace.path()),
        path_str(workspace.path()),
    );
    let backend = WindowsRestrictedTokenSandboxBackend::new();

    let result = backend.execute(&request);

    assert_eq!(
        result.execution_status,
        CommandExecutionStatus::PolicyDenied
    );
    assert_eq!(result.semantic_status, CommandSemanticStatus::PolicyBlocked);
    assert!(result.stderr_preview.contains("outside workspace"));
}

#[cfg(windows)]
#[test]
fn windows_restricted_token_backend_denies_sensitive_shell_path_before_spawn() {
    let workspace = tempfile::tempdir().expect("workspace");
    let request = CommandRequest::project_verification(
        "command_denied",
        vec![
            "cmd.exe".to_string(),
            "/C".to_string(),
            "type %USERPROFILE%\\.ssh\\id_rsa".to_string(),
        ],
        path_str(workspace.path()),
        path_str(workspace.path()),
    );
    let backend = WindowsRestrictedTokenSandboxBackend::new();

    let result = backend.execute(&request);

    assert_eq!(
        result.execution_status,
        CommandExecutionStatus::PolicyDenied
    );
    assert_eq!(result.semantic_status, CommandSemanticStatus::PolicyBlocked);
    assert!(!result.stderr_preview.contains("id_rsa"));
}

#[cfg(windows)]
#[test]
fn windows_restricted_token_backend_denies_read_only_redirection_before_spawn() {
    let workspace = tempfile::tempdir().expect("workspace");
    let written = workspace.path().join("write.txt");
    let mut request = CommandRequest::project_verification(
        "command_denied",
        vec![
            "cmd.exe".to_string(),
            "/C".to_string(),
            "echo should-not-write > write.txt".to_string(),
        ],
        path_str(workspace.path()),
        path_str(workspace.path()),
    );
    request.filesystem.mode = SandboxFilesystemMode::ReadOnly;
    let backend = WindowsRestrictedTokenSandboxBackend::new();

    let result = backend.execute(&request);

    assert_eq!(
        result.execution_status,
        CommandExecutionStatus::PolicyDenied
    );
    assert_eq!(result.semantic_status, CommandSemanticStatus::PolicyBlocked);
    assert!(!written.exists());
}

#[cfg(windows)]
#[test]
fn windows_restricted_token_backend_blocks_programmatic_workspace_write_in_read_only_mode() {
    let workspace = tempfile::tempdir().expect("workspace");
    let written = workspace.path().join("py_write.txt");
    let request = CommandRequest::project_verification(
        "command_write",
        vec![
            python_bin(),
            "-c".to_string(),
            "open('py_write.txt','w').write('x')".to_string(),
        ],
        path_str(workspace.path()),
        path_str(workspace.path()),
    );
    let mut request = request;
    request.filesystem.mode = SandboxFilesystemMode::ReadOnly;
    let backend = WindowsRestrictedTokenSandboxBackend::new();

    let result = backend.execute(&request);

    assert_ne!(result.semantic_status, CommandSemanticStatus::Succeeded);
    assert!(!written.exists());
}

#[cfg(windows)]
#[test]
fn windows_restricted_token_backend_allows_explicit_network_allowed_mode() {
    let workspace = tempfile::tempdir().expect("workspace");
    let mut request = CommandRequest::project_verification(
        "command_network_allowed",
        vec![
            "cmd.exe".to_string(),
            "/C".to_string(),
            "echo network-mode-ok".to_string(),
        ],
        path_str(workspace.path()),
        path_str(workspace.path()),
    );
    request.network.mode = SandboxNetworkMode::Allowed;
    let backend = WindowsRestrictedTokenSandboxBackend::new();

    let result = backend.execute(&request);

    assert_eq!(result.execution_status, CommandExecutionStatus::Completed);
    assert_eq!(result.semantic_status, CommandSemanticStatus::Succeeded);
    assert!(result.stdout_preview.contains("network-mode-ok"));
}

#[cfg(windows)]
#[test]
fn windows_restricted_token_backend_marks_network_denied_mode_unsupported() {
    let workspace = tempfile::tempdir().expect("workspace");
    let mut request = CommandRequest::project_verification(
        "command_network_denied",
        vec![
            "cmd.exe".to_string(),
            "/C".to_string(),
            "echo should-not-run".to_string(),
        ],
        path_str(workspace.path()),
        path_str(workspace.path()),
    );
    request.network.mode = SandboxNetworkMode::Denied;
    let backend = WindowsRestrictedTokenSandboxBackend::new();

    let result = backend.execute(&request);

    assert_eq!(result.execution_status, CommandExecutionStatus::Unsupported);
    assert_eq!(result.semantic_status, CommandSemanticStatus::Unsupported);
}

#[cfg(windows)]
#[test]
fn windows_restricted_token_backend_marks_network_allowlist_mode_unsupported() {
    let workspace = tempfile::tempdir().expect("workspace");
    let mut request = CommandRequest::project_verification(
        "command_network_allowlist",
        vec![
            "cmd.exe".to_string(),
            "/C".to_string(),
            "echo should-not-run".to_string(),
        ],
        path_str(workspace.path()),
        path_str(workspace.path()),
    );
    request.network.mode = SandboxNetworkMode::Allowlist;
    let backend = WindowsRestrictedTokenSandboxBackend::new();

    let result = backend.execute(&request);

    assert_eq!(result.execution_status, CommandExecutionStatus::Unsupported);
    assert_eq!(result.semantic_status, CommandSemanticStatus::Unsupported);
}

#[cfg(windows)]
#[test]
fn windows_restricted_token_backend_allows_programmatic_workspace_write_in_workspace_write_mode() {
    let workspace = tempfile::tempdir().expect("workspace");
    let written = workspace.path().join("py_write.txt");
    let mut request = CommandRequest::project_verification(
        "command_workspace_write",
        vec![
            python_bin(),
            "-c".to_string(),
            "open('py_write.txt','w').write('x')".to_string(),
        ],
        path_str(workspace.path()),
        path_str(workspace.path()),
    );
    request.filesystem.mode = SandboxFilesystemMode::WorkspaceWrite;
    let backend = WindowsRestrictedTokenSandboxBackend::new();

    let result = backend.execute(&request);

    assert_eq!(result.execution_status, CommandExecutionStatus::Completed);
    assert_eq!(result.semantic_status, CommandSemanticStatus::Succeeded);
    assert!(written.exists());
}

#[cfg(windows)]
#[test]
fn windows_restricted_token_backend_executes_danger_full_access_with_job_and_capture() {
    let workspace = tempfile::tempdir().expect("workspace");
    let mut request = CommandRequest::project_verification(
        "command_danger",
        vec![
            "cmd.exe".to_string(),
            "/C".to_string(),
            "echo danger-mode-ok".to_string(),
        ],
        path_str(workspace.path()),
        path_str(workspace.path()),
    );
    request.filesystem.mode = SandboxFilesystemMode::DangerFullAccess;
    let backend = WindowsRestrictedTokenSandboxBackend::new();

    let result = backend.execute(&request);

    assert!(backend.capabilities().job_object);
    assert_eq!(result.execution_status, CommandExecutionStatus::Completed);
    assert_eq!(result.semantic_status, CommandSemanticStatus::Succeeded);
    assert!(result.stdout_preview.contains("danger-mode-ok"));
}

#[cfg(not(windows))]
#[test]
fn windows_restricted_token_backend_is_unavailable_off_windows() {
    let workspace = tempfile::tempdir().expect("workspace");
    let request = CommandRequest::project_verification(
        "command_echo",
        vec!["echo".to_string(), "ok".to_string()],
        path_str(workspace.path()),
        path_str(workspace.path()),
    );
    let backend = WindowsRestrictedTokenSandboxBackend::new();

    let result = backend.execute(&request);

    assert_eq!(backend.name(), "windows_restricted_token");
    assert!(!backend.capabilities().supports_strict_command_execution());
    assert_eq!(
        result.execution_status,
        CommandExecutionStatus::BackendError
    );
    assert_eq!(result.semantic_status, CommandSemanticStatus::PolicyBlocked);
}

fn path_str(path: &Path) -> &str {
    path.to_str().expect("utf8 path")
}

#[cfg(windows)]
fn python_bin() -> String {
    std::env::var("PYTHON").unwrap_or_else(|_| "python".to_string())
}
